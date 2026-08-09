use std::collections::BTreeMap;

use daruma_mcp::{tool_definitions, tool_definitions_for, ToolProfile};
use serde_json::{json, Value};

// Baseline: 22,951 measured bytes after slimming, rounded up with ~10% room for
// new tools. If this trips, inspect the printed breakdown for prose growth
// before raising the limit.
const MAX_DEFAULT_TOOLS_LIST_BYTES: usize = 25_000;

struct ToolSize {
    name: String,
    total: usize,
    description: usize,
    input_schema: usize,
    schema_prose: usize,
    annotations: usize,
}

fn field_len(object: &Value, key: &str) -> usize {
    serde_json::to_string(key).unwrap().len()
        + 1
        + serde_json::to_string(&object[key]).unwrap().len()
}

fn schema_prose_len(value: &Value, descriptions: &mut BTreeMap<String, usize>) -> usize {
    match value {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| {
                let own = if key == "description" && value.is_string() {
                    *descriptions
                        .entry(value.as_str().unwrap().to_owned())
                        .or_default() += 1;
                    serde_json::to_string(key).unwrap().len()
                        + 1
                        + serde_json::to_string(value).unwrap().len()
                } else {
                    0
                };
                own + schema_prose_len(value, descriptions)
            })
            .sum(),
        Value::Array(values) => values
            .iter()
            .map(|value| schema_prose_len(value, descriptions))
            .sum(),
        _ => 0,
    }
}

fn print_part(label: &str, bytes: usize, total: usize) {
    println!(
        "{label:<32} {bytes:>8} bytes  {:>6.2}%",
        bytes as f64 * 100.0 / total as f64
    );
}

#[test]
fn measure_default_tools_list() {
    let tools = tool_definitions_for(ToolProfile::Default);
    assert!(!tools.is_empty());

    let response = serde_json::to_string(&json!({ "tools": &tools })).unwrap();
    let total = response.len();
    let mut descriptions = BTreeMap::new();
    let mut rows = Vec::with_capacity(tools.len());
    let mut names_and_titles = 0;
    let mut tool_descriptions = 0;
    let mut input_schemas = 0;
    let mut schema_prose = 0;
    let mut annotations = 0;

    for tool in &tools {
        let value = serde_json::to_value(tool).unwrap();
        let row = ToolSize {
            name: tool.name.to_owned(),
            total: serde_json::to_string(tool).unwrap().len(),
            description: field_len(&value, "description"),
            input_schema: field_len(&value, "inputSchema"),
            schema_prose: schema_prose_len(&value["inputSchema"], &mut descriptions),
            annotations: field_len(&value, "annotations"),
        };
        names_and_titles += field_len(&value, "name") + field_len(&value, "title");
        tool_descriptions += row.description;
        input_schemas += row.input_schema;
        schema_prose += row.schema_prose;
        annotations += row.annotations;
        rows.push(row);
    }

    let overhead = total - names_and_titles - tool_descriptions - input_schemas - annotations;

    println!("\nA. Summary");
    println!("response bytes: {total}");
    println!("default tools: {}", tools.len());
    println!(
        "catalog counts: Full/tool_definitions()={} | Default={}",
        tool_definitions().len(),
        tools.len()
    );

    println!("\nB. Components");
    print_part("name + title", names_and_titles, total);
    print_part("description", tool_descriptions, total);
    print_part("inputSchema", input_schemas, total);
    print_part("  schema prose", schema_prose, total);
    print_part("  schema structure", input_schemas - schema_prose, total);
    print_part("annotations", annotations, total);
    print_part("JSON overhead", overhead, total);

    rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));
    println!("\nC. Tools by serialized size");
    println!("name | total | description | inputSchema | schema-prose | annotations");
    for row in rows {
        println!(
            "{} | {} | {} | {} | {} | {}",
            row.name,
            row.total,
            row.description,
            row.input_schema,
            row.schema_prose,
            row.annotations
        );
    }

    let mut repeated: Vec<_> = descriptions
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect();
    repeated.sort_by(|(text_a, count_a), (text_b, count_b)| {
        (count_b * text_b.len())
            .cmp(&(count_a * text_a.len()))
            .then_with(|| text_a.cmp(text_b))
    });
    println!("\nD. Repeated schema prose");
    println!("count | len | count*len | text");
    for (text, count) in repeated {
        println!(
            "{} | {} | {} | {:?}",
            count,
            text.len(),
            count * text.len(),
            text
        );
    }

    assert!(
        total <= MAX_DEFAULT_TOOLS_LIST_BYTES,
        "default tools/list is {total} bytes; limit is {MAX_DEFAULT_TOOLS_LIST_BYTES}"
    );
}
