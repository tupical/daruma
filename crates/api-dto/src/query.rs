//! Draft wire types for the query contract. Not yet connected to a server.
use serde::{Deserialize, Serialize};

/// Version and resource are explicit so older servers cannot silently ignore a query.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "RawQueryRequest")]
pub struct QueryRequest {
    pub query_version: QueryVersion,
    pub resource: Resource,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Expression>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Vec<SelectField>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<Sort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueryRequest {
    query_version: QueryVersion,
    resource: Resource,
    scope: Scope,
    #[serde(default)] filter: Option<Expression>,
    #[serde(default)] select: Option<Vec<SelectField>>,
    #[serde(default)] sort: Option<Vec<Sort>>,
    #[serde(default)] limit: Option<u16>,
    #[serde(default)] cursor: Option<String>,
}

impl TryFrom<RawQueryRequest> for QueryRequest {
    type Error = &'static str;
    fn try_from(raw: RawQueryRequest) -> Result<Self, Self::Error> {
        let scope_valid = match &raw.scope {
            Scope::Inbox(v) | Scope::AllVisible(v) => *v,
            Scope::ProjectIds(ids) => !ids.is_empty() && ids.len() <= 100
                && ids.iter().all(|id| id.parse::<daruma_shared::ProjectId>().is_ok()),
        };
        if !scope_valid || raw.limit.is_some_and(|n| !(1..=200).contains(&n))
            || raw.select.as_ref().is_some_and(|fields| fields.is_empty() || fields.len() > 16)
            || raw.sort.as_ref().is_some_and(|keys| keys.is_empty() || keys.len() > 2)
            || raw.cursor.as_ref().is_some_and(String::is_empty)
        { return Err("invalid query scope or bounds"); }
        Ok(Self { query_version: raw.query_version, resource: raw.resource, scope: raw.scope,
            filter: raw.filter, select: raw.select, sort: raw.sort, limit: raw.limit, cursor: raw.cursor })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "u8", into = "u8")]
pub struct QueryVersion;
impl TryFrom<u8> for QueryVersion {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 1 { Ok(Self) } else { Err("unsupported query version") }
    }
}
impl From<QueryVersion> for u8 { fn from(_: QueryVersion) -> Self { 1 } }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Resource { Tasks, Projects }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope { ProjectIds(Vec<String>), Inbox(bool), AllVisible(bool) }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum Expression {
    And { #[serde(deserialize_with = "non_empty")] and: Vec<Expression> },
    Or { #[serde(deserialize_with = "non_empty")] or: Vec<Expression> },
    Not { not: Box<Expression> },
    Leaf(Leaf),
}

fn non_empty<'de, D>(deserializer: D) -> Result<Vec<Expression>, D::Error>
where D: serde::Deserializer<'de> {
    let values = Vec::<Expression>::deserialize(deserializer)?;
    if values.is_empty() { return Err(serde::de::Error::custom("expression list must not be empty")); }
    Ok(values)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "RawLeaf")]
pub struct Leaf {
    pub field: FilterField,
    pub op: Operator,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLeaf {
    field: FilterField,
    op: Operator,
    #[serde(default, deserialize_with = "present_value")]
    value: Option<serde_json::Value>,
}

fn present_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where D: serde::Deserializer<'de> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

impl TryFrom<RawLeaf> for Leaf {
    type Error = &'static str;
    fn try_from(raw: RawLeaf) -> Result<Self, Self::Error> {
        let valid_scalar = |value: &serde_json::Value| -> bool {
            match raw.field {
                FilterField::Id => value.as_str().is_some_and(|v| v.parse::<daruma_shared::TaskId>().is_ok()),
                FilterField::ProjectId => value.as_str().is_some_and(|v| v.parse::<daruma_shared::ProjectId>().is_ok()),
                FilterField::Status => serde_json::from_value::<daruma_domain::task::Status>(value.clone()).is_ok(),
                FilterField::Priority => serde_json::from_value::<daruma_domain::task::Priority>(value.clone()).is_ok(),
                FilterField::CreatedAt => serde_json::from_value::<daruma_shared::Timestamp>(value.clone()).is_ok(),
                FilterField::Assignee => {
                    let Some(object) = value.as_object() else { return false };
                    object.len() == 1 && (object.get("self") == Some(&serde_json::Value::Bool(true))
                        || object.get("logical_agent_id").and_then(|v| v.as_str()).is_some_and(|v| uuid::Uuid::parse_str(v).is_ok()))
                }
                FilterField::ClaimActive | FilterField::DependenciesNone | FilterField::DependenciesSatisfied | FilterField::ReadinessUnblocked => value.is_boolean(),
            }
        };
        let valid = match (&raw.field, &raw.op, &raw.value) {
            (FilterField::ProjectId | FilterField::Assignee, Operator::IsNull, None) => true,
            (FilterField::Id | FilterField::ProjectId | FilterField::Status | FilterField::Priority, Operator::In, Some(serde_json::Value::Array(values))) =>
                !values.is_empty() && values.iter().all(valid_scalar),
            (FilterField::CreatedAt, Operator::Eq | Operator::Lt | Operator::Lte | Operator::Gt | Operator::Gte, Some(value)) => valid_scalar(value),
            (_, Operator::Eq, Some(value)) => valid_scalar(value),
            _ => false,
        };
        if !valid { return Err("invalid field, operator or value combination"); }
        Ok(Self { field: raw.field, op: raw.op, value: raw.value })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Operator { Eq, In, IsNull, Lt, Lte, Gt, Gte }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum FilterField {
    #[serde(rename = "id")] Id,
    #[serde(rename = "project_id")] ProjectId,
    #[serde(rename = "status")] Status,
    #[serde(rename = "priority")] Priority,
    #[serde(rename = "created_at")] CreatedAt,
    #[serde(rename = "assignee")] Assignee,
    #[serde(rename = "claim.active")] ClaimActive,
    #[serde(rename = "dependencies.none")] DependenciesNone,
    #[serde(rename = "dependencies.satisfied")] DependenciesSatisfied,
    #[serde(rename = "readiness.unblocked")] ReadinessUnblocked,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sort { pub field: SortField, pub direction: Direction }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SelectField { Id, ProjectId, Title, Description, Status, Priority, CreatedAt, UpdatedAt, Assignee, Revision }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SortField { Id, CreatedAt, Priority }
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Direction { Asc, Desc }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_operator_for_boolean_field() {
        let request = r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"filter":{"field":"claim.active","op":"lt","value":true}}"#;
        assert!(serde_json::from_str::<QueryRequest>(request).is_err());
    }
    #[test]
    fn rejects_invalid_scope_and_request_bounds() {
        for fragment in [r#""scope":{"all_visible":false}"#, r#""scope":{"project_ids":[]}"#,
            r#""scope":{"project_ids":["bad"]}"#, r#""scope":{"inbox":true},"limit":0"#,
            r#""scope":{"inbox":true},"limit":201"#, r#""scope":{"inbox":true},"cursor":"""#] {
            let query = format!(r#"{{"query_version":1,"resource":"tasks",{fragment}}}"#);
            assert!(serde_json::from_str::<QueryRequest>(&query).is_err(), "{query}");
        }
    }
    #[test]
    fn rejects_unknown_select_and_sort_fields() {
        for query in [
            r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"select":["invented"]}"#,
            r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"sort":[{"field":"invented","direction":"asc"}]}"#,
        ] { assert!(serde_json::from_str::<QueryRequest>(query).is_err()); }
        assert!(serde_json::from_str::<QueryRequest>(r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"select":["id","title"],"sort":[{"field":"created_at","direction":"desc"}]}"#).is_ok());
    }
    #[test]
    fn null_predicate_round_trips_without_value() {
        let leaf: Leaf = serde_json::from_str(r#"{"field":"project_id","op":"is_null"}"#).unwrap();
        let encoded = serde_json::to_string(&leaf).unwrap();
        assert!(!encoded.contains("value"));
        assert_eq!(serde_json::from_str::<Leaf>(&encoded).unwrap(), leaf);
    }
    #[test]
    fn validates_field_operator_and_value_together() {
        for invalid in [
            r#"{"field":"status","op":"lt","value":"todo"}"#,
            r#"{"field":"status","op":"eq","value":"active"}"#,
            r#"{"field":"status","op":"in","value":[]}"#,
            r#"{"field":"priority","op":"eq","value":"p4"}"#,
            r#"{"field":"id","op":"eq","value":"not-an-id"}"#,
            r#"{"field":"assignee","op":"eq","value":{"self":false}}"#,
            r#"{"field":"project_id","op":"is_null","value":true}"#,
            r#"{"field":"project_id","op":"is_null","value":null}"#,
        ] { assert!(serde_json::from_str::<Leaf>(invalid).is_err(), "{invalid}"); }
        for valid in [
            r#"{"field":"status","op":"in","value":["todo","done"]}"#,
            r#"{"field":"assignee","op":"eq","value":{"self":true}}"#,
            r#"{"field":"project_id","op":"is_null"}"#,
            r#"{"field":"created_at","op":"gt","value":"2026-01-01T00:00:00Z"}"#,
        ] { assert!(serde_json::from_str::<Leaf>(valid).is_ok(), "{valid}"); }
    }
    #[test]
    fn rejects_empty_boolean_groups() {
        for input in [r#"{"and":[]}"#, r#"{"or":[]}"#] {
            assert!(serde_json::from_str::<Expression>(input).is_err());
        }
        assert!(serde_json::from_str::<Expression>(r#"{"and":[{"field":"claim.active","op":"eq","value":true}]}"#).is_ok());
    }
    #[test]
    fn rejects_expression_with_multiple_variants_or_extra_keys() {
        for filter in [
            r#"{"and":[],"not":{"field":"claim.active","op":"eq","value":true}}"#,
            r#"{"not":{"field":"claim.active","op":"eq","value":true},"extra":1}"#,
            r#"{"field":"claim.active","op":"eq","value":true,"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<Expression>(filter).is_err(), "{filter}");
        }
    }
    #[test]
    fn accepts_boolean_eq_and_rejects_non_boolean_values() {
        for value in ["true", "false"] {
            let request = format!(r#"{{"field":"dependencies.none","op":"eq","value":{value}}}"#);
            assert!(serde_json::from_str::<Leaf>(&request).is_ok());
        }
        for value in ["null", "1", "\"true\""] {
            let request = format!(r#"{{"field":"dependencies.none","op":"eq","value":{value}}}"#);
            assert!(serde_json::from_str::<Leaf>(&request).is_err());
        }
    }
    #[test]
    fn rejects_unknown_operator() {
        let request = r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"filter":{"field":"status","op":"invented","value":"todo"}}"#;
        assert!(serde_json::from_str::<QueryRequest>(request).is_err());
    }
    #[test]
    fn rejects_unknown_filter_field() {
        let request = r#"{"query_version":1,"resource":"tasks","scope":{"all_visible":true},"filter":{"field":"invented","op":"eq","value":true}}"#;
        assert!(serde_json::from_str::<QueryRequest>(request).is_err());
    }
}
