use daruma_domain::Actor;
use daruma_events::EventEnvelope;
use daruma_shared::{CoreError, Result, VersionId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqliteRow, Row, Sqlite, SqlitePool, Transaction};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EntityVersion {
    pub id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub version_number: i64,
    pub actor: Value,
    pub event_type: String,
    pub reason: Option<String>,
    pub source_event_id: Option<String>,
    pub source_event_seq: Option<i64>,
    pub created_at: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub diff: Value,
    pub changed_fields: Vec<String>,
    pub summary: String,
}

#[derive(Clone)]
pub struct EntityVersionRepo {
    pool: SqlitePool,
}

impl EntityVersionRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_for_entity(
        &self,
        entity_type: &str,
        entity_id: &str,
        limit: u32,
    ) -> Result<Vec<EntityVersion>> {
        validate_entity_type(entity_type)?;
        let entity_id = normalize_entity_id(entity_type, entity_id);
        let limit = clamp_limit(limit);
        let rows = sqlx::query(
            "SELECT * FROM entity_versions \
             WHERE entity_type = ? AND entity_id = ? \
             ORDER BY version_number DESC \
             LIMIT ?",
        )
        .bind(entity_type)
        .bind(entity_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.into_iter().map(row_to_entity_version).collect()
    }

    pub async fn latest(&self, limit: u32) -> Result<Vec<EntityVersion>> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query(
            "SELECT * FROM entity_versions \
             ORDER BY created_at DESC, id DESC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.into_iter().map(row_to_entity_version).collect()
    }

    pub async fn get(&self, id: &str) -> Result<Option<EntityVersion>> {
        let row = sqlx::query("SELECT * FROM entity_versions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        row.map(row_to_entity_version).transpose()
    }

    pub async fn get_by_number(
        &self,
        entity_type: &str,
        entity_id: &str,
        version_number: i64,
    ) -> Result<Option<EntityVersion>> {
        validate_entity_type(entity_type)?;
        let entity_id = normalize_entity_id(entity_type, entity_id);
        let row = sqlx::query(
            "SELECT * FROM entity_versions \
             WHERE entity_type = ? AND entity_id = ? AND version_number = ?",
        )
        .bind(entity_type)
        .bind(entity_id)
        .bind(version_number)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.map(row_to_entity_version).transpose()
    }

    pub async fn mark_rollback(
        &self,
        source_event_id: &str,
        rollback_of_version_id: &str,
    ) -> Result<()> {
        let rows =
            sqlx::query("SELECT id, diff_json FROM entity_versions WHERE source_event_id = ?")
                .bind(source_event_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;

        for row in rows {
            let id: String = row.get("id");
            let diff_json: String = row.get("diff_json");
            let mut diff: Value = parse_json(&diff_json)?;
            ensure_metadata(&mut diff).insert(
                "rollback_of_version_id".to_string(),
                Value::String(rollback_of_version_id.to_string()),
            );
            let diff_json =
                serde_json::to_string(&diff).map_err(|e| CoreError::serde(e.to_string()))?;
            sqlx::query(
                "UPDATE entity_versions SET reason = 'rollback', diff_json = ? WHERE id = ?",
            )
            .bind(diff_json)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        }

        Ok(())
    }
}

pub(crate) async fn insert_entity_version(
    tx: &mut Transaction<'_, Sqlite>,
    entity_type: &'static str,
    entity_id: String,
    before: Option<Value>,
    after: Option<Value>,
    envelope: &EventEnvelope,
    summary: String,
) -> Result<()> {
    if before == after {
        return Ok(());
    }

    let source_event_id = envelope.id.to_string();
    let already_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_versions \
         WHERE entity_type = ? AND entity_id = ? AND source_event_id = ?",
    )
    .bind(entity_type)
    .bind(&entity_id)
    .bind(&source_event_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;
    if already_exists > 0 {
        return Ok(());
    }

    let version_number: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version_number), 0) + 1 FROM entity_versions \
         WHERE entity_type = ? AND entity_id = ?",
    )
    .bind(entity_type)
    .bind(&entity_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;

    let changed_fields = changed_fields(before.as_ref(), after.as_ref());
    let diff = field_diff(
        before.as_ref(),
        after.as_ref(),
        envelope.kind(),
        &changed_fields,
    );
    let actor_json =
        serde_json::to_string(&envelope.actor).map_err(|e| CoreError::serde(e.to_string()))?;
    let (actor_kind, actor_id, actor_name) = actor_columns(&envelope.actor);
    let before_json = before
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let after_json = after
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let diff_json = serde_json::to_string(&diff).map_err(|e| CoreError::serde(e.to_string()))?;
    let changed_fields_json =
        serde_json::to_string(&changed_fields).map_err(|e| CoreError::serde(e.to_string()))?;

    sqlx::query(
        "INSERT INTO entity_versions \
         (id, entity_type, entity_id, version_number, actor_json, actor_kind, actor_id, \
          actor_name, event_type, reason, source_event_id, source_event_seq, created_at, \
          before_json, after_json, diff_json, changed_fields_json, summary) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(VersionId::new().to_string())
    .bind(entity_type)
    .bind(entity_id)
    .bind(version_number)
    .bind(actor_json)
    .bind(actor_kind)
    .bind(actor_id)
    .bind(actor_name)
    .bind(envelope.kind())
    .bind(source_event_id)
    .bind(envelope.seq as i64)
    .bind(envelope.occurred_at.to_rfc3339())
    .bind(before_json)
    .bind(after_json)
    .bind(diff_json)
    .bind(changed_fields_json)
    .bind(summary)
    .execute(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(())
}

pub(crate) fn update_summary(
    entity_label: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> String {
    match (before, after) {
        (None, Some(_)) => format!("{entity_label} created"),
        (Some(_), None) => format!("{entity_label} deleted"),
        (Some(before), Some(after)) => {
            let fields = changed_fields(Some(before), Some(after));
            match fields.as_slice() {
                [] => format!("{entity_label} unchanged"),
                [field] if field == "content" => {
                    let before_len = before
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default();
                    let after_len = after
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .unwrap_or_default();
                    if after_len >= before_len {
                        format!(
                            "{entity_label} content changed: +{} characters",
                            after_len - before_len
                        )
                    } else {
                        format!(
                            "{entity_label} content changed: -{} characters",
                            before_len - after_len
                        )
                    }
                }
                [field] => {
                    let before_value = before.get(field).cloned().unwrap_or(Value::Null);
                    let after_value = after.get(field).cloned().unwrap_or(Value::Null);
                    format!(
                        "{entity_label} {field} changed: {} -> {}",
                        summary_value(&before_value),
                        summary_value(&after_value)
                    )
                }
                _ => format!("{entity_label} updated: {}", fields.join(", ")),
            }
        }
        (None, None) => format!("{entity_label} unchanged"),
    }
}

fn actor_columns(actor: &Actor) -> (&'static str, Option<String>, Option<String>) {
    match actor {
        Actor::User => ("user", None, None),
        Actor::Agent { id, name } => ("agent", Some(id.to_string()), Some(name.clone())),
    }
}

fn validate_entity_type(entity_type: &str) -> Result<()> {
    match entity_type {
        "task" | "document" => Ok(()),
        other => Err(CoreError::validation(format!(
            "unknown version entity_type: {other}"
        ))),
    }
}

fn normalize_entity_id(entity_type: &str, entity_id: &str) -> String {
    match entity_type {
        "task" if !entity_id.starts_with("tsk_") => format!("tsk_{entity_id}"),
        "document" if !entity_id.starts_with("doc_") => format!("doc_{entity_id}"),
        _ => entity_id.to_string(),
    }
}

fn clamp_limit(limit: u32) -> i64 {
    i64::from(limit.clamp(1, 200))
}

fn row_to_entity_version(row: SqliteRow) -> Result<EntityVersion> {
    let actor_json: String = row.get("actor_json");
    let before_json: Option<String> = row.get("before_json");
    let after_json: Option<String> = row.get("after_json");
    let diff_json: String = row.get("diff_json");
    let changed_fields_json: String = row.get("changed_fields_json");

    Ok(EntityVersion {
        id: row.get("id"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        version_number: row.get("version_number"),
        actor: parse_json(&actor_json)?,
        event_type: row.get("event_type"),
        reason: row.get("reason"),
        source_event_id: row.get("source_event_id"),
        source_event_seq: row.get("source_event_seq"),
        created_at: row.get("created_at"),
        before: parse_optional_json(before_json)?,
        after: parse_optional_json(after_json)?,
        diff: parse_json(&diff_json)?,
        changed_fields: parse_json(&changed_fields_json)?,
        summary: row.get("summary"),
    })
}

fn parse_json<T>(raw: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(raw).map_err(|e| CoreError::serde(e.to_string()))
}

fn parse_optional_json<T>(raw: Option<String>) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    raw.map(|s| parse_json(&s)).transpose()
}

fn changed_fields(before: Option<&Value>, after: Option<&Value>) -> Vec<String> {
    match (before, after) {
        (None, Some(_)) => vec!["created".to_string()],
        (Some(_), None) => vec!["deleted".to_string()],
        (Some(Value::Object(before)), Some(Value::Object(after))) => {
            let mut keys: Vec<String> = before
                .keys()
                .chain(after.keys())
                .filter(|key| before.get(*key) != after.get(*key))
                .cloned()
                .collect();
            keys.sort();
            keys.dedup();
            keys
        }
        (Some(before), Some(after)) if before != after => vec!["value".to_string()],
        _ => Vec::new(),
    }
}

fn field_diff(
    before: Option<&Value>,
    after: Option<&Value>,
    source: &'static str,
    changed_fields: &[String],
) -> Value {
    if changed_fields.iter().any(|field| field == "content") {
        if let (Some(Value::Object(before)), Some(Value::Object(after))) = (before, after) {
            let before_content = before.get("content").and_then(Value::as_str);
            let after_content = after.get("content").and_then(Value::as_str);
            if let (Some(before_content), Some(after_content)) = (before_content, after_content) {
                let mut fields = field_changes(before, after, changed_fields);
                fields.insert(
                    "content".to_string(),
                    json!({
                        "before_hash": sha256_digest(before_content),
                        "after_hash": sha256_digest(after_content),
                        "unified_diff": unified_text_diff(before_content, after_content),
                    }),
                );

                return json!({
                    "kind": "document_text_patch",
                    "fields": fields,
                    "metadata": {
                        "source": source,
                    },
                });
            }
        }
    }

    json!({
        "kind": "field_json_patch",
        "fields": match (before, after) {
            (Some(Value::Object(before)), Some(Value::Object(after))) => {
                field_changes(before, after, changed_fields)
            }
            (None, Some(after)) => {
                let mut fields = Map::new();
                fields.insert("created".to_string(), json!({ "before": null, "after": after }));
                fields
            }
            (Some(before), None) => {
                let mut fields = Map::new();
                fields.insert("deleted".to_string(), json!({ "before": before, "after": null }));
                fields
            }
            _ => Map::new(),
        },
        "metadata": {
            "source": source,
        },
    })
}

fn field_changes(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    changed_fields: &[String],
) -> Map<String, Value> {
    let mut fields = Map::new();
    for field in changed_fields {
        fields.insert(
            field.clone(),
            json!({
                "before": before.get(field).cloned().unwrap_or(Value::Null),
                "after": after.get(field).cloned().unwrap_or(Value::Null),
            }),
        );
    }
    fields
}

fn sha256_digest(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

fn unified_text_diff(before: &str, after: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let mut out = String::from("--- before\n+++ after\n@@\n");

    let common_prefix = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    for line in before_lines.iter().take(common_prefix) {
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    for line in before_lines.iter().skip(common_prefix) {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in after_lines.iter().skip(common_prefix) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }

    out
}

fn summary_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn ensure_metadata(diff: &mut Value) -> &mut Map<String, Value> {
    let obj = diff.as_object_mut().expect("diff must be a JSON object");
    let metadata = obj
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    metadata
        .as_object_mut()
        .expect("metadata object just inserted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_field_diff_records_changed_values() {
        let before = json!({
            "title": "old",
            "status": "todo"
        });
        let after = json!({
            "title": "new",
            "status": "todo"
        });
        let fields = changed_fields(Some(&before), Some(&after));

        let diff = field_diff(Some(&before), Some(&after), "task_updated", &fields);

        assert_eq!(diff["kind"], "field_json_patch");
        assert_eq!(diff["fields"]["title"]["before"], "old");
        assert_eq!(diff["fields"]["title"]["after"], "new");
        assert_eq!(diff["metadata"]["source"], "task_updated");
    }

    #[test]
    fn document_content_diff_records_hashes_and_unified_diff() {
        let before = json!({
            "title": "Human Log",
            "content": "first\nsecond"
        });
        let after = json!({
            "title": "Human Log",
            "content": "first\nthird"
        });
        let fields = changed_fields(Some(&before), Some(&after));

        let diff = field_diff(
            Some(&before),
            Some(&after),
            "document_content_replaced",
            &fields,
        );

        assert_eq!(diff["kind"], "document_text_patch");
        assert_eq!(
            diff["fields"]["content"]["before_hash"]
                .as_str()
                .unwrap()
                .len(),
            71
        );
        assert!(diff["fields"]["content"]["unified_diff"]
            .as_str()
            .unwrap()
            .contains("-second\n+third"));
        assert_eq!(diff["metadata"]["source"], "document_content_replaced");
    }
}
