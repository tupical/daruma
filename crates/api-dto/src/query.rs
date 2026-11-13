//! Draft wire types for the query contract. Not yet connected to a server.
use serde::{Deserialize, Serialize};

/// Version and resource are explicit so older servers cannot silently ignore a query.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    pub query_version: QueryVersion,
    pub resource: Resource,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Expression>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<Sort>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
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
#[serde(untagged)]
pub enum Expression {
    And { and: Vec<Expression> },
    Or { or: Vec<Expression> },
    Not { not: Box<Expression> },
    Leaf(Leaf),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "RawLeaf")]
pub struct Leaf { pub field: FilterField, pub op: Operator, pub value: Option<serde_json::Value> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLeaf { field: FilterField, op: Operator, value: Option<serde_json::Value> }

impl TryFrom<RawLeaf> for Leaf {
    type Error = &'static str;
    fn try_from(raw: RawLeaf) -> Result<Self, Self::Error> {
        if matches!(raw.field, FilterField::ClaimActive | FilterField::DependenciesNone | FilterField::DependenciesSatisfied | FilterField::ReadinessUnblocked)
            && (raw.op != Operator::Eq || !matches!(raw.value, Some(serde_json::Value::Bool(_))))
        {
            return Err("boolean predicates require eq and a boolean value");
        }
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
pub struct Sort { pub field: String, pub direction: Direction }
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
