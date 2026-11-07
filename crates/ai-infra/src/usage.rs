//! Token usage of provider replies, handed to a caller that asks for it.
//!
//! [`crate::AiProvider`] returns the parsed answer only, so an observer that
//! wraps a provider (a metrics recorder) never sees the reply's `usage`.
//! [`capture_usage`] opens a task-local slot around one call; every reply the
//! client reads inside it adds its counters there — before parsing, so a reply
//! cut by the token limit still reports what it cost. Outside a capture nothing
//! is recorded and nothing changes.

use std::{cell::Cell, future::Future};

use serde_json::Value;

/// Tokens of one call. `input_tokens` is the whole prompt on every protocol:
/// Anthropic's uncached part plus its cache write and read (Responses and Chat
/// Completions already count cached tokens in their prompt total).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    fn add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

tokio::task_local! {
    static CAPTURED: Cell<Option<TokenUsage>>;
}

/// Runs `call` and returns its output with the summed usage of every reply
/// read during it; `None` when no reply carried a `usage` object (an HTTP
/// error, a transport failure, a provider that omits it).
pub async fn capture_usage<F: Future>(call: F) -> (F::Output, Option<TokenUsage>) {
    CAPTURED
        .scope(Cell::new(None), async move {
            let output = call.await;
            (output, CAPTURED.with(Cell::get))
        })
        .await
}

/// Adds `usage` to the open capture, if any.
pub(crate) fn report(usage: TokenUsage) {
    let _ = CAPTURED.try_with(|slot| {
        slot.set(Some(slot.get().unwrap_or_default().add(usage)));
    });
}

/// `usage` of a Responses, Chat Completions or Anthropic Messages reply.
/// Missing counters read as zero; no `usage` object reads as `None`.
pub(crate) fn reply_usage(json: &Value) -> Option<TokenUsage> {
    let usage = json.get("usage")?.as_object()?;
    let count = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    let cache_creation_input_tokens = count("cache_creation_input_tokens");
    let cache_read_input_tokens = count("cache_read_input_tokens");
    let (input, output) = if usage.contains_key("prompt_tokens") {
        (count("prompt_tokens"), count("completion_tokens"))
    } else {
        (
            count("input_tokens")
                .saturating_add(cache_creation_input_tokens)
                .saturating_add(cache_read_input_tokens),
            count("output_tokens"),
        )
    };
    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens,
        cache_read_input_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reply_usage_reads_each_protocol_as_whole_prompt_and_output() {
        let responses = json!({"usage": {"input_tokens": 120, "output_tokens": 30,
            "input_tokens_details": {"cached_tokens": 100}}});
        assert_eq!(
            reply_usage(&responses),
            Some(TokenUsage {
                input_tokens: 120,
                output_tokens: 30,
                ..TokenUsage::default()
            })
        );
        let chat = json!({"usage": {"prompt_tokens": 40, "completion_tokens": 9,
            "total_tokens": 49}});
        assert_eq!(
            reply_usage(&chat),
            Some(TokenUsage {
                input_tokens: 40,
                output_tokens: 9,
                ..TokenUsage::default()
            })
        );
        let anthropic = json!({"usage": {"input_tokens": 5, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 1200, "output_tokens": 7}});
        assert_eq!(
            reply_usage(&anthropic),
            Some(TokenUsage {
                input_tokens: 1205,
                output_tokens: 7,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1200,
            })
        );
        assert_eq!(reply_usage(&json!({"output": []})), None);
    }

    #[tokio::test]
    async fn capture_sums_reports_inside_and_ignores_reports_outside() {
        report(TokenUsage {
            output_tokens: 99,
            ..TokenUsage::default()
        });
        let ((), usage) = capture_usage(async {
            for output_tokens in [3, 4] {
                report(TokenUsage {
                    input_tokens: 10,
                    output_tokens,
                    ..TokenUsage::default()
                });
            }
        })
        .await;
        assert_eq!(
            usage,
            Some(TokenUsage {
                input_tokens: 20,
                output_tokens: 7,
                ..TokenUsage::default()
            })
        );
        let ((), none) = capture_usage(async {}).await;
        assert_eq!(none, None);
    }
}
