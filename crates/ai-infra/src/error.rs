//! Error type for the AI crate and its conversion to [`CoreError`].

use daruma_shared::CoreError;
use thiserror::Error;

/// Render an error together with everything that caused it, `a: b: c`.
fn cause_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        // Chains repeat themselves often enough (a wrapper whose Display is its
        // cause's) that without this the message doubles up and reads as noise.
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

/// All failure modes that can occur in the AI crate.
#[derive(Debug, Error)]
pub enum AiError {
    /// A required configuration value was absent.
    #[error("configuration error: {0}")]
    Config(String),

    /// The HTTP transport failed (network, timeout, …).
    ///
    /// Displayed with its full cause chain. `reqwest::Error`'s own `Display`
    /// stops at "error sending request for url (…)", which names the URL and
    /// nothing else — every transport failure looks identical in a log, and the
    /// one fact an operator needs (connect refused? timed out? TLS? connection
    /// closed mid-body?) lives in `source()`. Consumers stringify this error
    /// into logs and API responses, so the chain has to be in `Display` itself.
    #[error("HTTP error: {}", cause_chain(.0))]
    Http(#[from] reqwest::Error),

    /// The OpenAI API returned a non-2xx status.
    #[error("API error {status}: {message}")]
    Api { status: u16, message: String },

    /// The model's response could not be parsed into the expected shape.
    #[error("parse failed: {0}")]
    ParseFailed(String),

    /// JSON (de)serialisation failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The model returned a message without the expected tool call.
    #[error("no tool call in response")]
    NoToolCall,
}

impl From<AiError> for CoreError {
    fn from(e: AiError) -> Self {
        CoreError::ai(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Inner;
    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "connection refused")
        }
    }
    impl std::error::Error for Inner {}

    #[derive(Debug)]
    struct Outer(Inner);
    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error sending request")
        }
    }
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn cause_chain_appends_the_reason_not_just_the_symptom() {
        assert_eq!(
            cause_chain(&Outer(Inner)),
            "error sending request: connection refused"
        );
    }

    #[test]
    fn cause_chain_does_not_repeat_a_pass_through_wrapper() {
        #[derive(Debug)]
        struct PassThrough(Inner);
        impl std::fmt::Display for PassThrough {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection refused")
            }
        }
        impl std::error::Error for PassThrough {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        assert_eq!(cause_chain(&PassThrough(Inner)), "connection refused");
    }
}
