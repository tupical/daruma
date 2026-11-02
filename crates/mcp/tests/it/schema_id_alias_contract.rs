//! Id-alias schema contract (task 01a14418-2ceb-7d23-9278-e980b776bf0e):
//! `docs/guides/mcp-client.md` says the key task/plan tools take either id
//! name. A client that validates arguments against the published
//! `inputSchema` must therefore accept both `{key}` and `{alias}`; the tests
//! below check every such call against the schema itself, not just the
//! handler.
//!
//! "At least one of" is deliberately not a schema keyword: the Anthropic
//! Messages API rejects a tool `input_schema` with a top-level
//! `anyOf`/`oneOf`/`allOf`, so it would make the whole catalogue unusable for
//! Claude clients. Both names are listed, neither is `required`, and the
//! handler rejects a call with neither, naming the accepted names.

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, routing::any, Router};
use daruma_mcp::tools::{call_tool, tool_definitions};
use daruma_mcp::ApiClient;
use serde_json::{json, Map, Value};

type Captured = Arc<Mutex<Vec<String>>>;

const AGENT: &str = "019e0000-0000-7000-8000-000000000001";

/// Tool, schema name, alias, and the other arguments its schema requires.
fn aliased_tools() -> Vec<(&'static str, &'static str, &'static str, Value)> {
    vec![
        ("daruma_get", "id", "task_id", json!({})),
        ("daruma_update", "id", "task_id", json!({"title": "t"})),
        (
            "daruma_set_status",
            "id",
            "task_id",
            json!({"status": "todo"}),
        ),
        ("daruma_complete", "id", "task_id", json!({})),
        ("daruma_comment", "task_id", "id", json!({"body": "b"})),
        ("daruma_relations", "task_id", "id", json!({})),
        ("daruma_claim", "task_id", "id", json!({"agent_id": AGENT})),
        (
            "daruma_release",
            "task_id",
            "id",
            json!({"agent_id": AGENT}),
        ),
        ("daruma_plan_get", "id", "plan_id", json!({})),
        ("daruma_plan_progress", "plan_id", "id", json!({})),
        ("daruma_plan_drain_next", "plan_id", "id", json!({})),
        ("daruma_plan_next_task", "id", "plan_id", json!({})),
    ]
}

fn schema_of(tool: &str) -> Value {
    tool_definitions()
        .into_iter()
        .find(|t| t.name == tool)
        .unwrap_or_else(|| panic!("no tool {tool}"))
        .input_schema
}

fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// The subset of JSON Schema a validating MCP client applies to top-level
/// tool arguments: `required`, `additionalProperties: false`, property
/// `type` (string, list or `anyOf` of types) and `enum`.
fn validate(schema: &Value, args: &Value) -> Result<(), String> {
    let args = args.as_object().ok_or("arguments are not an object")?;
    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for name in schema["required"].as_array().into_iter().flatten() {
        let name = name.as_str().unwrap();
        if !args.contains_key(name) {
            return Err(format!("missing required `{name}`"));
        }
    }
    for (name, value) in args {
        let Some(prop) = props.get(name) else {
            if schema["additionalProperties"] == json!(false) {
                return Err(format!("`{name}` is not a declared property"));
            }
            continue;
        };
        let types: Vec<&str> = match &prop["type"] {
            Value::String(ty) => vec![ty.as_str()],
            Value::Array(tys) => tys.iter().filter_map(Value::as_str).collect(),
            _ => prop["anyOf"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|branch| branch["type"].as_str())
                .collect(),
        };
        if !types.is_empty() && !types.iter().any(|ty| type_matches(ty, value)) {
            return Err(format!("`{name}` is not of type {types:?}"));
        }
        if let Some(allowed) = prop["enum"].as_array() {
            if !allowed.contains(value) {
                return Err(format!("`{name}` = {value} is not in the enum"));
            }
        }
    }
    Ok(())
}

fn with_id(extra: &Value, name: &str, id: &str) -> Value {
    let mut args: Map<String, Value> = extra.as_object().unwrap().clone();
    args.insert(name.to_string(), json!(id));
    Value::Object(args)
}

async fn serve(captured: Captured) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let captured = captured.clone();
        async move {
            let path = req.uri().path_and_query().unwrap().as_str().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap();
            captured
                .lock()
                .unwrap()
                .push(format!("{path} {}", String::from_utf8_lossy(&bytes)));
            axum::Json(json!({}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (ApiClient::new(base, "test-token"), handle)
}

/// Requests `tool` sent and its error, if any.
async fn call(tool: &str, args: Value) -> (Vec<String>, Option<String>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    let err = call_tool(&client, tool, args)
        .await
        .err()
        .map(|e| e.to_string());
    handle.abort();
    let sent = captured.lock().unwrap().clone();
    (sent, err)
}

#[tokio::test]
async fn either_id_name_passes_the_published_schema_and_reaches_the_server() {
    for (tool, key, alias, extra) in aliased_tools() {
        let schema = schema_of(tool);
        let id = if tool.starts_with("daruma_plan_") {
            "pln_1"
        } else {
            "tsk_1"
        };
        for name in [key, alias] {
            let args = with_id(&extra, name, id);
            validate(&schema, &args)
                .unwrap_or_else(|e| panic!("schema of {tool} rejects {args}: {e}"));
            let (sent, err) = call(tool, args.clone()).await;
            assert!(err.is_none(), "{tool} {args}: {err:?}");
            assert!(
                sent.iter().any(|request| request.contains(id)),
                "{tool} {{{name}}} did not reach the server with {id}: {sent:?}"
            );
        }
    }
}

#[tokio::test]
async fn neither_id_name_passes_the_schema_but_fails_naming_both() {
    for (tool, key, alias, extra) in aliased_tools() {
        validate(&schema_of(tool), &extra)
            .unwrap_or_else(|e| panic!("{tool}: the other arguments fail the schema: {e}"));
        let (sent, err) = call(tool, extra.clone()).await;
        assert!(
            sent.is_empty(),
            "{tool} sent a request without an id: {sent:?}"
        );
        let err = err.unwrap_or_else(|| panic!("{tool} accepted a call without an id"));
        assert!(
            err.contains(&format!("`{key}`")) && err.contains(&format!("`{alias}`")),
            "{tool}: error does not name `{key}` and `{alias}`: {err}"
        );
    }
}

/// Every tool whose schema lists two id names without requiring either is in
/// the table above, so a new alias cannot ship without these checks.
#[test]
fn every_optional_id_pair_is_covered() {
    let covered: Vec<&str> = aliased_tools().iter().map(|(tool, ..)| *tool).collect();
    for def in tool_definitions() {
        let props = &def.input_schema["properties"];
        let required = def.input_schema["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let optional = |name: &str| props.get(name).is_some() && !required.contains(&json!(name));
        if optional("id") && (optional("task_id") || optional("plan_id")) {
            assert!(
                covered.contains(&def.name),
                "{} lists an optional id pair but is not in aliased_tools()",
                def.name
            );
        }
    }
}

#[test]
fn no_tool_schema_uses_top_level_composition() {
    for def in tool_definitions() {
        let schema = &def.input_schema;
        assert_eq!(schema["type"], "object", "{}", def.name);
        for keyword in ["anyOf", "oneOf", "allOf", "not"] {
            assert!(
                schema.get(keyword).is_none(),
                "{} has a top-level `{keyword}`, which the Anthropic API rejects",
                def.name
            );
        }
    }
}

/// `docs/guides/mcp-client.md` with every whitespace run collapsed to one
/// space, so reflowing a paragraph does not move a phrase out of reach.
fn mcp_client_doc() -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/guides/mcp-client.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `` `daruma_<tool> {<arg>}` `` call examples of the guide.
fn doc_call_examples(doc: &str) -> Vec<(String, String)> {
    doc.match_indices("`daruma_")
        .filter_map(|(at, _)| {
            let span = &doc[at + 1..];
            let span = &span[..span.find('`')?];
            let (tool, rest) = span.split_once(" {")?;
            let arg = rest.strip_suffix('}')?;
            let ident = !arg.is_empty() && arg.chars().all(|c| c == '_' || c.is_ascii_lowercase());
            ident.then(|| (tool.to_string(), arg.to_string()))
        })
        .collect()
}

/// A schema-validating client sends `tool {arg}` (plus the tool's other,
/// non-id arguments) only when `arg` is declared and no other id name is
/// required.
fn example_fits_schema(tool: &str, arg: &str) -> Result<(), String> {
    let schema = schema_of(tool);
    if schema["properties"].get(arg).is_none() {
        return Err(format!("`{arg}` is not a declared property"));
    }
    for name in schema["required"].as_array().into_iter().flatten() {
        let name = name.as_str().unwrap();
        if name != arg && (name == "id" || name.ends_with("_id")) {
            return Err(format!("requires `{name}`"));
        }
    }
    Ok(())
}

/// Task 01a144fe-e537-7bb0-8e87-9c41d64f0245: the guide promised
/// `daruma_run_complete {id}` while that schema requires `run_id`, so a
/// validating client rejected a documented call. Every example in the guide
/// must now pass the published schema.
#[test]
fn guide_call_examples_pass_the_published_schema() {
    let examples = doc_call_examples(&mcp_client_doc());
    assert!(
        !examples.is_empty(),
        "no call examples found in mcp-client.md"
    );
    for (tool, arg) in examples {
        example_fits_schema(&tool, &arg).unwrap_or_else(|e| {
            panic!("mcp-client.md shows `{tool} {{{arg}}}`, but its schema {e}")
        });
    }
    // The check rejects the example the guide used to carry.
    assert!(example_fits_schema("daruma_run_complete", "id").is_err());
}

/// The guide's key-tool list names exactly the tools whose schemas take
/// either id name.
#[test]
fn guide_key_tool_list_matches_aliased_tools() {
    let doc = mcp_client_doc();
    let start = doc
        .find("The schemas of the key tools")
        .expect("key-tool sentence missing from mcp-client.md");
    let end = start
        + doc[start..]
            .find("list both names")
            .expect("key-tool sentence has no end");
    let mut listed: Vec<&str> = doc[start..end].split('`').skip(1).step_by(2).collect();
    listed.sort_unstable();
    let mut covered: Vec<&str> = aliased_tools().iter().map(|(tool, ..)| *tool).collect();
    covered.sort_unstable();
    assert_eq!(listed, covered);
}
