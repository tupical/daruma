//! WorkspaceGraph sidecar projection.
//!
//! This repo owns derived graph state only. The canonical event log and the
//! existing projections remain authoritative; this index can be rebuilt.

use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use taskagent_domain::{Document, Plan, Project, RelationKind};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{CoreError, Result};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub source_id: String,
    pub project_id: Option<String>,
    pub title: String,
    pub text: String,
    pub updated_at: String,
    pub metadata: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    pub source_event_seq: Option<i64>,
    pub metadata: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GraphDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphContextItem {
    pub node: GraphNode,
    pub edge: GraphEdge,
    pub direction: GraphDirection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphNeighborhood {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphSearchHit {
    pub node: GraphNode,
    pub score: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphStatus {
    pub schema_version: u32,
    pub node_count: u64,
    pub edge_count: u64,
    pub last_event_seq: Option<u64>,
    pub last_error: Option<String>,
}

pub struct WorkspaceGraphRepo {
    pool: SqlitePool,
}

impl WorkspaceGraphRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workspacegraph_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                project_id TEXT,
                title TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_workspacegraph_nodes_kind_source
             ON workspacegraph_nodes(kind, source_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_workspacegraph_nodes_project
             ON workspacegraph_nodes(project_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workspacegraph_edges (
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                source_event_seq INTEGER,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY (from_id, to_id, kind)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_workspacegraph_edges_to
             ON workspacegraph_edges(to_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_workspacegraph_edges_kind
             ON workspacegraph_edges(kind)",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workspacegraph_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS workspacegraph_fts
             USING fts5(
                node_id UNINDEXED,
                kind UNINDEXED,
                project_id UNINDEXED,
                title,
                text
             )",
        )
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        self.set_meta("schema_version", "2").await?;
        Ok(())
    }

    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        self.ensure_schema().await?;
        let seq = envelope.seq as i64;
        let updated_at = envelope.occurred_at.to_rfc3339();

        match &envelope.payload {
            Event::ProjectCreated { project } => self.upsert_project(project).await?,
            Event::ProjectUpdated {
                project_id,
                title,
                description,
            } => {
                let node_id = project_node_id(project_id);
                if let Some(title) = title {
                    self.update_node_title(&node_id, title, &updated_at).await?;
                }
                if let Some(description) = description {
                    self.update_node_text(
                        &node_id,
                        description.as_deref().unwrap_or(""),
                        &updated_at,
                    )
                    .await?;
                }
            }
            Event::ProjectDeleted { project_id } => {
                self.delete_node(&project_node_id(project_id)).await?;
            }

            Event::TaskCreated { task } => {
                if let Some(task_id) = task.id {
                    let node_id = task_node_id(&task_id);
                    self.upsert_node(
                        &node_id,
                        "Task",
                        &task_id.to_string(),
                        task.project_id.map(|id| id.to_string()).as_deref(),
                        &task.title,
                        task.description.as_deref().unwrap_or(""),
                        &updated_at,
                        json!({
                            "status": task.status.map(|s| s.as_str()),
                            "priority": task.priority.map(|p| p.as_str()),
                            "due_at": task.due_at.map(|t| t.to_rfc3339()),
                        }),
                    )
                    .await?;
                    if let Some(project_id) = task.project_id {
                        self.upsert_edge(
                            &project_node_id(&project_id),
                            &node_id,
                            "Contains",
                            seq,
                            json!({}),
                        )
                        .await?;
                    }
                }
            }
            Event::TaskUpdated { task_id, patch } => {
                let node_id = task_node_id(task_id);
                if let Some(title) = &patch.title {
                    self.update_node_title(&node_id, title, &updated_at).await?;
                }
                if let Some(description) = &patch.description {
                    self.update_node_text(&node_id, description, &updated_at)
                        .await?;
                }
                if let Some(project_id) = patch.project_id {
                    self.delete_edges_to_kind(&node_id, "Contains").await?;
                    let project_id_s = project_id.map(|id| id.to_string());
                    self.update_node_project(&node_id, project_id_s.as_deref(), &updated_at)
                        .await?;
                    if let Some(project_id) = project_id {
                        self.upsert_edge(
                            &project_node_id(&project_id),
                            &node_id,
                            "Contains",
                            seq,
                            json!({}),
                        )
                        .await?;
                    }
                }
            }
            Event::TaskStatusChanged { task_id, to, .. } => {
                self.merge_node_metadata(
                    &task_node_id(task_id),
                    json!({ "status": to.as_str() }),
                    &updated_at,
                )
                .await?;
            }
            Event::TaskPriorityChanged { task_id, to, .. } => {
                self.merge_node_metadata(
                    &task_node_id(task_id),
                    json!({ "priority": to.as_str() }),
                    &updated_at,
                )
                .await?;
            }
            Event::TaskCompleted {
                task_id,
                completed_at,
            } => {
                self.merge_node_metadata(
                    &task_node_id(task_id),
                    json!({ "completed_at": completed_at.to_rfc3339() }),
                    &updated_at,
                )
                .await?;
            }
            Event::TaskDeleted { task_id } => self.delete_node(&task_node_id(task_id)).await?,

            Event::PlanCreated { plan } => self.upsert_plan(plan, seq).await?,
            Event::PlanUpdated { plan_id, patch } => {
                let node_id = plan_node_id(plan_id);
                if let Some(title) = &patch.title {
                    self.update_node_title(&node_id, title, &updated_at).await?;
                }
                if patch.description.is_some()
                    || patch.goal.is_some()
                    || patch.success_criteria.is_some()
                {
                    let mut metadata = self
                        .get_node(&node_id)
                        .await?
                        .map(|node| node.metadata)
                        .unwrap_or_else(|| json!({}));
                    let mut metadata_patch = json!({});
                    if let Some(description) = &patch.description {
                        metadata_patch["description"] = json!(description);
                    }
                    if let Some(goal) = &patch.goal {
                        metadata_patch["goal"] = json!(goal);
                    }
                    if let Some(success_criteria) = &patch.success_criteria {
                        metadata_patch["success_criteria"] = json!(success_criteria);
                    }
                    merge_json(&mut metadata, metadata_patch);
                    let description = metadata
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let goal = metadata.get("goal").and_then(Value::as_str).unwrap_or("");
                    let success_criteria = metadata
                        .get("success_criteria")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(ToOwned::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let text = plan_text(description, goal, &success_criteria);
                    self.update_node_text(&node_id, &text, &updated_at).await?;
                    self.replace_node_metadata(&node_id, metadata, &updated_at)
                        .await?;
                }
                if let Some(parent_plan_id) = patch.parent_plan_id {
                    self.delete_edges_to_kind(&node_id, "ParentPlan").await?;
                    if let Some(parent_plan_id) = parent_plan_id {
                        self.upsert_edge(
                            &plan_node_id(&parent_plan_id),
                            &node_id,
                            "ParentPlan",
                            seq,
                            json!({}),
                        )
                        .await?;
                    }
                }
            }
            Event::PlanArchived { plan_id, at } => {
                self.merge_node_metadata(
                    &plan_node_id(plan_id),
                    json!({ "archived_at": at.to_rfc3339() }),
                    &updated_at,
                )
                .await?;
            }
            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position,
                depends_on,
            } => {
                self.upsert_edge(
                    &plan_node_id(plan_id),
                    &task_node_id(task_id),
                    "PlanContains",
                    seq,
                    json!({ "position": position, "depends_on": depends_on }),
                )
                .await?;
            }
            Event::PlanTaskRemoved { plan_id, task_id } => {
                self.delete_edge(
                    &plan_node_id(plan_id),
                    &task_node_id(task_id),
                    "PlanContains",
                )
                .await?;
            }
            Event::PlanReordered { plan_id, order } => {
                for (position, task_id) in order.iter().enumerate() {
                    self.upsert_edge(
                        &plan_node_id(plan_id),
                        &task_node_id(task_id),
                        "PlanContains",
                        seq,
                        json!({ "position": position }),
                    )
                    .await?;
                }
            }

            Event::CommentAdded { comment } => {
                let node_id = comment_node_id(&comment.id);
                self.upsert_node(
                    &node_id,
                    "Comment",
                    &comment.id.to_string(),
                    None,
                    "Comment",
                    &comment.body,
                    &comment.created_at.to_rfc3339(),
                    json!({
                        "task_id": comment.task_id,
                        "kind": comment.kind.map(|k| k.as_str()),
                        "author": comment.author,
                    }),
                )
                .await?;
                self.upsert_edge(
                    &node_id,
                    &task_node_id(&comment.task_id),
                    "CommentOn",
                    seq,
                    json!({}),
                )
                .await?;
            }
            Event::CommentEdited {
                comment_id,
                patch,
                edited_at,
                ..
            } => {
                if let Some(body) = &patch.body {
                    self.update_node_text(
                        &comment_node_id(comment_id),
                        body,
                        &edited_at.to_rfc3339(),
                    )
                    .await?;
                }
            }
            Event::CommentDeleted { comment_id, .. } => {
                self.delete_node(&comment_node_id(comment_id)).await?;
            }

            Event::TaskLinked {
                from,
                to,
                kind,
                relation_id,
                ..
            } => {
                self.upsert_edge(
                    &task_node_id(from),
                    &task_node_id(to),
                    relation_kind_name(*kind),
                    seq,
                    json!({ "relation_id": relation_id }),
                )
                .await?;
            }
            Event::TaskUnlinked { from, to, kind, .. } => {
                self.delete_edge(
                    &task_node_id(from),
                    &task_node_id(to),
                    relation_kind_name(*kind),
                )
                .await?;
            }
            Event::TaskRelationKindChanged {
                from,
                to,
                from_kind,
                to_kind,
                relation_id,
                ..
            } => {
                self.delete_edge(
                    &task_node_id(from),
                    &task_node_id(to),
                    relation_kind_name(*from_kind),
                )
                .await?;
                self.upsert_edge(
                    &task_node_id(from),
                    &task_node_id(to),
                    relation_kind_name(*to_kind),
                    seq,
                    json!({ "relation_id": relation_id }),
                )
                .await?;
            }

            Event::DocumentCreated { document } => self.upsert_document(document, seq).await?,
            Event::DocumentContentReplaced {
                document_id,
                content,
                at,
            } => {
                self.update_node_text(&document_node_id(document_id), content, &at.to_rfc3339())
                    .await?;
            }
            Event::DocumentContentAppended {
                document_id,
                append,
                at,
            } => {
                sqlx::query(
                    "UPDATE workspacegraph_nodes
                     SET text = CASE WHEN text = '' THEN ? ELSE text || char(10) || ? END,
                         updated_at = ?
                     WHERE id = ?",
                )
                .bind(append)
                .bind(append)
                .bind(at.to_rfc3339())
                .bind(document_node_id(document_id))
                .execute(&self.pool)
                .await
                .map_err(storage_err)?;
                if let Some(node) = self.get_node(&document_node_id(document_id)).await? {
                    self.sync_fts_for_node(&node).await?;
                }
            }
            Event::DocumentRenamed {
                document_id,
                title,
                at,
            } => {
                self.update_node_title(&document_node_id(document_id), title, &at.to_rfc3339())
                    .await?;
            }
            Event::DocumentArchived { document_id, at } => {
                self.merge_node_metadata(
                    &document_node_id(document_id),
                    json!({ "archived_at": at.to_rfc3339() }),
                    &at.to_rfc3339(),
                )
                .await?;
            }

            // ── Artifact Registry (P4) ─────────────────────────────────────
            Event::ArtifactRegistered { artifact } => {
                let node_id = artifact_node_id(&artifact.id);
                self.upsert_node(
                    &node_id,
                    "Artifact",
                    &artifact.id.to_string(),
                    artifact.project_id.map(|p| p.to_string()).as_deref(),
                    &artifact.title,
                    &artifact.description,
                    &updated_at,
                    json!({
                        "uri": artifact.uri,
                        "status": artifact.status.as_str(),
                        "task_id": artifact.task_id.map(|t| t.to_string()),
                    }),
                )
                .await?;
                // Link to project if present.
                if let Some(project_id) = artifact.project_id {
                    self.upsert_edge(
                        &project_node_id(&project_id),
                        &node_id,
                        "Contains",
                        seq,
                        json!({}),
                    )
                    .await?;
                }
                // Link to producing task if present.
                if let Some(task_id) = artifact.task_id {
                    self.upsert_edge(
                        &task_node_id(&task_id),
                        &node_id,
                        "Produces",
                        seq,
                        json!({}),
                    )
                    .await?;
                }
            }

            Event::ArtifactStatusChanged {
                artifact_id, to, ..
            } => {
                self.merge_node_metadata(
                    &artifact_node_id(artifact_id),
                    json!({ "status": to.as_str() }),
                    &updated_at,
                )
                .await?;
            }

            Event::ArtifactChanged {
                artifact_id,
                title,
                description,
                ..
            } => {
                if let Some(t) = title {
                    self.update_node_title(&artifact_node_id(artifact_id), t, &updated_at)
                        .await?;
                }
                if let Some(d) = description {
                    self.update_node_text(&artifact_node_id(artifact_id), d, &updated_at)
                        .await?;
                }
            }

            Event::ArtifactOwnerAssigned {
                artifact_id,
                owner_agent_id,
                ..
            } => {
                self.merge_node_metadata(
                    &artifact_node_id(artifact_id),
                    json!({ "owner_agent_id": owner_agent_id.to_string() }),
                    &updated_at,
                )
                .await?;
            }

            Event::ArtifactWriteCommitted {
                artifact_id,
                version,
                ..
            } => {
                self.merge_node_metadata(
                    &artifact_node_id(artifact_id),
                    json!({
                        "status": "committed",
                        "version": version,
                    }),
                    &updated_at,
                )
                .await?;
            }

            Event::ArtifactDeprecated { artifact_id, .. } => {
                self.merge_node_metadata(
                    &artifact_node_id(artifact_id),
                    json!({ "status": "deprecated" }),
                    &updated_at,
                )
                .await?;
            }

            Event::ArtifactRelationAdded { relation } => {
                self.upsert_edge(
                    &artifact_node_id(&relation.from_id),
                    &artifact_node_id(&relation.to_id),
                    relation.kind.graph_edge_kind(),
                    seq,
                    json!({ "relation_id": relation.id.to_string() }),
                )
                .await?;
            }

            Event::ArtifactRelationRemoved {
                from_id,
                to_id,
                kind,
                ..
            } => {
                self.delete_edge(
                    &artifact_node_id(from_id),
                    &artifact_node_id(to_id),
                    kind.graph_edge_kind(),
                )
                .await?;
            }

            _ => {}
        }

        self.set_meta("last_event_seq", &envelope.seq.to_string())
            .await?;
        self.set_meta("last_error", "").await?;
        Ok(())
    }

    pub async fn context(&self, node_id: &str, limit: u32) -> Result<Vec<GraphContextItem>> {
        self.ensure_schema().await?;
        let rows = sqlx::query(
            "SELECT e.from_id, e.to_id, e.kind, e.source_event_seq, e.metadata_json,
                    n.id, n.kind AS node_kind, n.source_id, n.project_id,
                    n.title, n.text, n.updated_at, n.metadata_json AS node_metadata_json,
                    CASE WHEN e.from_id = ? THEN 'out' ELSE 'in' END AS direction
             FROM workspacegraph_edges e
             JOIN workspacegraph_nodes n
               ON n.id = CASE WHEN e.from_id = ? THEN e.to_id ELSE e.from_id END
             WHERE e.from_id = ? OR e.to_id = ?
             ORDER BY e.kind ASC, n.updated_at DESC
             LIMIT ?",
        )
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .bind(node_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;

        rows.iter().map(row_to_context_item).collect()
    }

    pub async fn related(
        &self,
        node_id: &str,
        depth: u32,
        limit: u32,
    ) -> Result<GraphNeighborhood> {
        self.ensure_schema().await?;
        let mut seen = HashSet::from([node_id.to_string()]);
        let mut frontier = VecDeque::from([(node_id.to_string(), 0_u32)]);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        if let Some(root) = self.get_node(node_id).await? {
            nodes.push(root);
        }

        while let Some((current, current_depth)) = frontier.pop_front() {
            if current_depth >= depth || nodes.len() as u32 >= limit {
                continue;
            }

            for item in self.context(&current, limit).await? {
                if !edges.iter().any(|e: &GraphEdge| {
                    e.from_id == item.edge.from_id
                        && e.to_id == item.edge.to_id
                        && e.kind == item.edge.kind
                }) {
                    edges.push(item.edge.clone());
                }

                if seen.insert(item.node.id.clone()) {
                    frontier.push_back((item.node.id.clone(), current_depth + 1));
                    nodes.push(item.node);
                    if nodes.len() as u32 >= limit {
                        break;
                    }
                }
            }
        }

        Ok(GraphNeighborhood { nodes, edges })
    }

    pub async fn impact(&self, node_id: &str, limit: u32) -> Result<GraphNeighborhood> {
        self.ensure_schema().await?;
        let mut seen = HashSet::from([node_id.to_string()]);
        let mut frontier = VecDeque::from([node_id.to_string()]);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        if let Some(root) = self.get_node(node_id).await? {
            nodes.push(root);
        }

        while let Some(current) = frontier.pop_front() {
            if nodes.len() as u32 >= limit {
                break;
            }

            let rows = sqlx::query(
                "SELECT from_id, to_id, kind, source_event_seq, metadata_json
                 FROM workspacegraph_edges
                 WHERE from_id = ?
                   AND kind IN (
                       'Blocks', 'PlanContains', 'Contains', 'Produces',
                       'ArtDependsOn', 'ArtImplements', 'ArtTests',
                       'ArtDocuments', 'ArtSupersedes', 'ArtConflictsWith'
                   )",
            )
            .bind(&current)
            .fetch_all(&self.pool)
            .await
            .map_err(storage_err)?;

            for row in rows {
                let edge = row_to_edge(&row)?;
                if let Some(node) = self.get_node(&edge.to_id).await? {
                    edges.push(edge);
                    if seen.insert(node.id.clone()) {
                        frontier.push_back(node.id.clone());
                        nodes.push(node);
                        if nodes.len() as u32 >= limit {
                            break;
                        }
                    }
                }
            }
        }

        Ok(GraphNeighborhood { nodes, edges })
    }

    pub async fn search(
        &self,
        query: &str,
        limit: u32,
        project_id: Option<&str>,
    ) -> Result<Vec<GraphSearchHit>> {
        self.search_inner(query, limit, project_id, None).await
    }

    pub async fn search_near(
        &self,
        query: &str,
        limit: u32,
        project_id: Option<&str>,
        near_node_id: &str,
    ) -> Result<Vec<GraphSearchHit>> {
        self.search_inner(query, limit, project_id, Some(near_node_id))
            .await
    }

    pub async fn status(&self) -> Result<GraphStatus> {
        self.ensure_schema().await?;
        let node_count =
            scalar_count(&self.pool, "SELECT COUNT(*) FROM workspacegraph_nodes").await?;
        let edge_count =
            scalar_count(&self.pool, "SELECT COUNT(*) FROM workspacegraph_edges").await?;
        let schema_version = self
            .get_meta("schema_version")
            .await?
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        let last_event_seq = self
            .get_meta("last_event_seq")
            .await?
            .and_then(|s| s.parse::<u64>().ok());
        let last_error = self.get_meta("last_error").await?.filter(|s| !s.is_empty());

        Ok(GraphStatus {
            schema_version,
            node_count,
            edge_count,
            last_event_seq,
            last_error,
        })
    }

    async fn search_inner(
        &self,
        query: &str,
        limit: u32,
        project_id: Option<&str>,
        near_node_id: Option<&str>,
    ) -> Result<Vec<GraphSearchHit>> {
        self.ensure_schema().await?;
        let candidate_limit = if near_node_id.is_some() {
            limit.saturating_mul(4).max(limit)
        } else {
            limit
        };
        let rows = sqlx::query(
            "SELECT n.id, n.kind, n.source_id, n.project_id, n.title, n.text,
                    n.updated_at, n.metadata_json, bm25(workspacegraph_fts) AS rank
             FROM workspacegraph_fts
             JOIN workspacegraph_nodes n ON n.id = workspacegraph_fts.node_id
             WHERE workspacegraph_fts MATCH ?
               AND (? IS NULL OR n.project_id = ?)
             ORDER BY rank ASC
             LIMIT ?",
        )
        .bind(query)
        .bind(project_id)
        .bind(project_id)
        .bind(candidate_limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_err)?;

        let proximity_ids = if let Some(near_node_id) = near_node_id {
            self.structural_proximity_ids(near_node_id).await?
        } else {
            HashSet::new()
        };

        let mut hits = rows
            .iter()
            .map(|row| {
                let node = row_to_node(row)?;
                let rank: f64 = row.try_get("rank").map_err(storage_err)?;
                let score = if proximity_ids.contains(&node.id) {
                    rank - 10.0
                } else {
                    rank
                };
                Ok(GraphSearchHit { node, score })
            })
            .collect::<Result<Vec<_>>>()?;

        hits.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node.id.cmp(&b.node.id))
        });
        hits.truncate(limit as usize);
        Ok(hits)
    }

    async fn structural_proximity_ids(&self, node_id: &str) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        for item in self.context(node_id, 200).await? {
            ids.insert(item.node.id);
        }
        Ok(ids)
    }

    async fn upsert_project(&self, project: &Project) -> Result<()> {
        self.upsert_node(
            &project_node_id(&project.id),
            "Project",
            &project.id.to_string(),
            Some(&project.id.to_string()),
            &project.title,
            project.description.as_deref().unwrap_or(""),
            &project.updated_at.to_rfc3339(),
            json!({ "slug": project.slug }),
        )
        .await
    }

    async fn upsert_plan(&self, plan: &Plan, source_event_seq: i64) -> Result<()> {
        let node_id = plan_node_id(&plan.id);
        self.upsert_node(
            &node_id,
            "Plan",
            &plan.id.to_string(),
            Some(&plan.project_id.to_string()),
            &plan.title,
            &plan_text(&plan.description, &plan.goal, &plan.success_criteria),
            &plan.updated_at.to_rfc3339(),
            json!({
                "status": plan.status,
                "owner": plan.owner,
                "description": plan.description,
                "goal": plan.goal,
                "success_criteria": plan.success_criteria,
            }),
        )
        .await?;
        self.upsert_edge(
            &project_node_id(&plan.project_id),
            &node_id,
            "Contains",
            source_event_seq,
            json!({}),
        )
        .await?;
        if let Some(parent) = plan.parent_plan_id {
            self.upsert_edge(
                &plan_node_id(&parent),
                &node_id,
                "ParentPlan",
                source_event_seq,
                json!({}),
            )
            .await?;
        }
        Ok(())
    }

    async fn upsert_document(&self, document: &Document, source_event_seq: i64) -> Result<()> {
        let node_id = document_node_id(&document.id);
        self.upsert_node(
            &node_id,
            "Document",
            &document.id.to_string(),
            Some(&document.project_id.to_string()),
            &document.title,
            &document.content,
            &document.updated_at.to_rfc3339(),
            json!({ "kind": document.kind.as_str() }),
        )
        .await?;
        self.upsert_edge(
            &project_node_id(&document.project_id),
            &node_id,
            "Contains",
            source_event_seq,
            json!({}),
        )
        .await
    }

    async fn upsert_node(
        &self,
        id: &str,
        kind: &str,
        source_id: &str,
        project_id: Option<&str>,
        title: &str,
        text: &str,
        updated_at: &str,
        metadata: Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspacegraph_nodes
             (id, kind, source_id, project_id, title, text, updated_at, metadata_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                source_id = excluded.source_id,
                project_id = excluded.project_id,
                title = excluded.title,
                text = excluded.text,
                updated_at = excluded.updated_at,
                metadata_json = excluded.metadata_json",
        )
        .bind(id)
        .bind(kind)
        .bind(source_id)
        .bind(project_id)
        .bind(title)
        .bind(text)
        .bind(updated_at)
        .bind(metadata.to_string())
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        if let Some(node) = self.get_node(id).await? {
            self.sync_fts_for_node(&node).await?;
        }

        Ok(())
    }

    async fn upsert_edge(
        &self,
        from_id: &str,
        to_id: &str,
        kind: &str,
        source_event_seq: i64,
        metadata: Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspacegraph_edges
             (from_id, to_id, kind, source_event_seq, metadata_json)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(from_id, to_id, kind) DO UPDATE SET
                source_event_seq = excluded.source_event_seq,
                metadata_json = excluded.metadata_json",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(kind)
        .bind(source_event_seq)
        .bind(metadata.to_string())
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;

        Ok(())
    }

    async fn get_node(&self, id: &str) -> Result<Option<GraphNode>> {
        let row = sqlx::query(
            "SELECT id, kind, source_id, project_id, title, text, updated_at, metadata_json
             FROM workspacegraph_nodes
             WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_err)?;

        row.as_ref().map(row_to_node).transpose()
    }

    async fn update_node_title(&self, node_id: &str, title: &str, updated_at: &str) -> Result<()> {
        sqlx::query("UPDATE workspacegraph_nodes SET title = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(updated_at)
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        if let Some(node) = self.get_node(node_id).await? {
            self.sync_fts_for_node(&node).await?;
        }
        Ok(())
    }

    async fn update_node_text(&self, node_id: &str, text: &str, updated_at: &str) -> Result<()> {
        sqlx::query("UPDATE workspacegraph_nodes SET text = ?, updated_at = ? WHERE id = ?")
            .bind(text)
            .bind(updated_at)
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        if let Some(node) = self.get_node(node_id).await? {
            self.sync_fts_for_node(&node).await?;
        }
        Ok(())
    }

    async fn update_node_project(
        &self,
        node_id: &str,
        project_id: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE workspacegraph_nodes SET project_id = ?, updated_at = ? WHERE id = ?")
            .bind(project_id)
            .bind(updated_at)
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        if let Some(node) = self.get_node(node_id).await? {
            self.sync_fts_for_node(&node).await?;
        }
        Ok(())
    }

    async fn merge_node_metadata(
        &self,
        node_id: &str,
        patch: Value,
        updated_at: &str,
    ) -> Result<()> {
        if let Some(mut node) = self.get_node(node_id).await? {
            merge_json(&mut node.metadata, patch);
            sqlx::query(
                "UPDATE workspacegraph_nodes
                 SET metadata_json = ?, updated_at = ?
                 WHERE id = ?",
            )
            .bind(node.metadata.to_string())
            .bind(updated_at)
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        }
        Ok(())
    }

    async fn replace_node_metadata(
        &self,
        node_id: &str,
        metadata: Value,
        updated_at: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE workspacegraph_nodes
             SET metadata_json = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(metadata.to_string())
        .bind(updated_at)
        .bind(node_id)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_node(&self, node_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM workspacegraph_edges WHERE from_id = ? OR to_id = ?")
            .bind(node_id)
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        sqlx::query("DELETE FROM workspacegraph_fts WHERE node_id = ?")
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        sqlx::query("DELETE FROM workspacegraph_nodes WHERE id = ?")
            .bind(node_id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn sync_fts_for_node(&self, node: &GraphNode) -> Result<()> {
        sqlx::query("DELETE FROM workspacegraph_fts WHERE node_id = ?")
            .bind(&node.id)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        sqlx::query(
            "INSERT INTO workspacegraph_fts(node_id, kind, project_id, title, text)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&node.id)
        .bind(&node.kind)
        .bind(node.project_id.as_deref())
        .bind(&node.title)
        .bind(&node.text)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_edge(&self, from_id: &str, to_id: &str, kind: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM workspacegraph_edges
             WHERE from_id = ? AND to_id = ? AND kind = ?",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(kind)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn delete_edges_to_kind(&self, to_id: &str, kind: &str) -> Result<()> {
        sqlx::query("DELETE FROM workspacegraph_edges WHERE to_id = ? AND kind = ?")
            .bind(to_id)
            .bind(kind)
            .execute(&self.pool)
            .await
            .map_err(storage_err)?;
        Ok(())
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO workspacegraph_meta(key, value)
             VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(storage_err)?;
        Ok(())
    }

    async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        sqlx::query("SELECT value FROM workspacegraph_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_err)?
            .map(|row| row.try_get("value").map_err(storage_err))
            .transpose()
    }
}

fn row_to_context_item(row: &sqlx::sqlite::SqliteRow) -> Result<GraphContextItem> {
    let direction_s: String = row.try_get("direction").map_err(storage_err)?;
    Ok(GraphContextItem {
        node: GraphNode {
            id: row.try_get("id").map_err(storage_err)?,
            kind: row.try_get("node_kind").map_err(storage_err)?,
            source_id: row.try_get("source_id").map_err(storage_err)?,
            project_id: row.try_get("project_id").map_err(storage_err)?,
            title: row.try_get("title").map_err(storage_err)?,
            text: row.try_get("text").map_err(storage_err)?,
            updated_at: row.try_get("updated_at").map_err(storage_err)?,
            metadata: parse_metadata(row.try_get("node_metadata_json").map_err(storage_err)?)?,
        },
        edge: row_to_edge(row)?,
        direction: if direction_s == "out" {
            GraphDirection::Outgoing
        } else {
            GraphDirection::Incoming
        },
    })
}

fn row_to_node(row: &sqlx::sqlite::SqliteRow) -> Result<GraphNode> {
    Ok(GraphNode {
        id: row.try_get("id").map_err(storage_err)?,
        kind: row.try_get("kind").map_err(storage_err)?,
        source_id: row.try_get("source_id").map_err(storage_err)?,
        project_id: row.try_get("project_id").map_err(storage_err)?,
        title: row.try_get("title").map_err(storage_err)?,
        text: row.try_get("text").map_err(storage_err)?,
        updated_at: row.try_get("updated_at").map_err(storage_err)?,
        metadata: parse_metadata(row.try_get("metadata_json").map_err(storage_err)?)?,
    })
}

fn row_to_edge(row: &sqlx::sqlite::SqliteRow) -> Result<GraphEdge> {
    Ok(GraphEdge {
        from_id: row.try_get("from_id").map_err(storage_err)?,
        to_id: row.try_get("to_id").map_err(storage_err)?,
        kind: row.try_get("kind").map_err(storage_err)?,
        source_event_seq: row.try_get("source_event_seq").map_err(storage_err)?,
        metadata: parse_metadata(row.try_get("metadata_json").map_err(storage_err)?)?,
    })
}

async fn scalar_count(pool: &SqlitePool, sql: &str) -> Result<u64> {
    let row = sqlx::query(sql)
        .fetch_one(pool)
        .await
        .map_err(storage_err)?;
    let count: i64 = row.try_get(0).map_err(storage_err)?;
    Ok(count as u64)
}

fn parse_metadata(raw: String) -> Result<Value> {
    serde_json::from_str(&raw).map_err(|e| CoreError::serde(e.to_string()))
}

fn merge_json(base: &mut Value, patch: Value) {
    if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn plan_text(description: &str, goal: &str, success_criteria: &[String]) -> String {
    let mut parts = Vec::new();
    if !description.is_empty() {
        parts.push(description.to_string());
    }
    if !goal.is_empty() {
        parts.push(goal.to_string());
    }
    if !success_criteria.is_empty() {
        parts.push(success_criteria.join("\n"));
    }
    parts.join("\n\n")
}

fn relation_kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Blocks => "Blocks",
        RelationKind::RelatesTo => "RelatesTo",
        RelationKind::Duplicates => "Duplicates",
        RelationKind::WasBlocking => "WasBlocking",
    }
}

fn project_node_id(id: &impl ToString) -> String {
    format!("project:{}", id.to_string())
}

fn plan_node_id(id: &impl ToString) -> String {
    format!("plan:{}", id.to_string())
}

fn task_node_id(id: &impl ToString) -> String {
    format!("task:{}", id.to_string())
}

fn document_node_id(id: &impl ToString) -> String {
    format!("document:{}", id.to_string())
}

fn comment_node_id(id: &impl ToString) -> String {
    format!("comment:{}", id.to_string())
}

fn artifact_node_id(id: &impl ToString) -> String {
    format!("artifact:{}", id.to_string())
}

fn storage_err<E: std::fmt::Display>(err: E) -> CoreError {
    CoreError::storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;
    use taskagent_domain::{
        Actor, Comment, Document, DocumentKind, NewTask, Plan, PlanStatus, Priority, Project,
        RelationKind, Status,
    };
    use taskagent_events::{Event, EventEnvelope};
    use taskagent_shared::{CommentId, DocumentId, PlanId, ProjectId, RelationId, TaskId};

    use super::*;

    async fn repo() -> WorkspaceGraphRepo {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let repo = WorkspaceGraphRepo::new(pool);
        repo.ensure_schema().await.unwrap();
        repo
    }

    fn env(seq: u64, payload: Event) -> EventEnvelope {
        let mut envelope = EventEnvelope::new(Actor::user(), payload);
        envelope.seq = seq;
        envelope
    }

    #[tokio::test]
    async fn projects_tasks_plans_comments_documents_and_relations_are_indexed() {
        let repo = repo().await;
        let now = Utc::now();
        let project_id = ProjectId::new();
        let task_id = TaskId::new();
        let blocked_id = TaskId::new();
        let plan_id = PlanId::new();
        let comment_id = CommentId::new();
        let document_id = DocumentId::new();
        let relation_id = RelationId::new();

        repo.apply_event(&env(
            1,
            Event::ProjectCreated {
                project: Project {
                    id: project_id,
                    tenant_id: None,
                    slug: "demo".into(),
                    title: "Demo".into(),
                    description: Some("Project body".into()),
                    triage_enabled: false,
                    created_at: now,
                    updated_at: now,
                },
            },
        ))
        .await
        .unwrap();

        for (id, title) in [(task_id, "Build index"), (blocked_id, "Use index")] {
            repo.apply_event(&env(
                2,
                Event::TaskCreated {
                    task: NewTask {
                        id: Some(id),
                        project_id: Some(project_id),
                        title: title.into(),
                        description: Some("Task body".into()),
                        status: Some(Status::Todo),
                        priority: Some(Priority::P2),
                        triage_state: None,
                        due_at: None,
                    },
                },
            ))
            .await
            .unwrap();
        }

        repo.apply_event(&env(
            4,
            Event::PlanCreated {
                plan: Plan {
                    id: plan_id,
                    project_id,
                    parent_plan_id: None,
                    title: "Graph plan".into(),
                    description: "Plan body".into(),
                    goal: "Ship graph".into(),
                    success_criteria: vec!["Queries work".into()],
                    status: PlanStatus::Active,
                    owner: Actor::user(),
                    created_at: now,
                    updated_at: now,
                    archived_at: None,
                    source_brief: None,
                },
            },
        ))
        .await
        .unwrap();

        repo.apply_event(&env(
            5,
            Event::PlanTaskAdded {
                plan_id,
                task_id,
                position: 0,
                depends_on: vec![],
            },
        ))
        .await
        .unwrap();

        repo.apply_event(&env(
            6,
            Event::CommentAdded {
                comment: Comment {
                    id: comment_id,
                    task_id,
                    author: Actor::user(),
                    body: "Comment body".into(),
                    parent_id: None,
                    kind: None,
                    created_at: now,
                    edited_at: None,
                    deleted_at: None,
                },
            },
        ))
        .await
        .unwrap();

        repo.apply_event(&env(
            7,
            Event::DocumentCreated {
                document: Document {
                    id: document_id,
                    project_id,
                    kind: DocumentKind::HumanLog,
                    title: "Human Log".into(),
                    content: "Log body".into(),
                    created_at: now,
                    updated_at: now,
                    archived_at: None,
                },
            },
        ))
        .await
        .unwrap();

        repo.apply_event(&env(
            8,
            Event::TaskLinked {
                relation_id,
                from: task_id,
                to: blocked_id,
                kind: RelationKind::Blocks,
                actor: Actor::user(),
                occurred_at: now,
            },
        ))
        .await
        .unwrap();

        let context = repo.context(&task_node_id(&task_id), 20).await.unwrap();
        let context_kinds: HashSet<_> =
            context.iter().map(|item| item.edge.kind.as_str()).collect();
        assert!(context_kinds.contains("Contains"));
        assert!(context_kinds.contains("PlanContains"));
        assert!(context_kinds.contains("CommentOn"));
        assert!(context_kinds.contains("Blocks"));

        let impact = repo.impact(&task_node_id(&task_id), 10).await.unwrap();
        assert!(
            impact
                .nodes
                .iter()
                .any(|node| node.id == task_node_id(&blocked_id)),
            "blocked task should be in impact set: {impact:?}"
        );

        let status = repo.status().await.unwrap();
        assert_eq!(status.node_count, 6);
        assert_eq!(status.last_event_seq, Some(8));
    }

    #[tokio::test]
    async fn indexes_10k_synthetic_tasks() {
        let repo = repo().await;
        let now = Utc::now();
        let project_id = ProjectId::new();

        repo.apply_event(&env(
            1,
            Event::ProjectCreated {
                project: Project {
                    id: project_id,
                    tenant_id: None,
                    slug: "large".into(),
                    title: "Large".into(),
                    description: None,
                    triage_enabled: false,
                    created_at: now,
                    updated_at: now,
                },
            },
        ))
        .await
        .unwrap();

        for i in 0..10_000_u64 {
            let task_id = TaskId::new();
            repo.apply_event(&env(
                i + 2,
                Event::TaskCreated {
                    task: NewTask {
                        id: Some(task_id),
                        project_id: Some(project_id),
                        title: format!("Task {i}"),
                        description: Some("Synthetic task".into()),
                        status: Some(Status::Todo),
                        priority: Some(Priority::P3),
                        triage_state: None,
                        due_at: None,
                    },
                },
            ))
            .await
            .unwrap();
        }

        let status = repo.status().await.unwrap();
        assert_eq!(status.node_count, 10_001);
        assert_eq!(status.edge_count, 10_000);
        assert_eq!(status.last_event_seq, Some(10_001));
    }

    #[tokio::test]
    async fn lexical_search_indexes_nodes_and_respects_project_scope() {
        let repo = repo().await;
        let now = Utc::now();
        let project_a = ProjectId::new();
        let project_b = ProjectId::new();
        let task_a = TaskId::new();
        let task_b = TaskId::new();
        let comment_id = CommentId::new();
        let document_id = DocumentId::new();

        for (seq, project_id, title) in [
            (1, project_a, "Alpha project"),
            (2, project_b, "Beta project"),
        ] {
            repo.apply_event(&env(
                seq,
                Event::ProjectCreated {
                    project: Project {
                        id: project_id,
                        tenant_id: None,
                        slug: title.to_lowercase().replace(' ', "-"),
                        title: title.into(),
                        description: None,
                        triage_enabled: false,
                        created_at: now,
                        updated_at: now,
                    },
                },
            ))
            .await
            .unwrap();
        }

        for (seq, task_id, project_id, title) in [
            (3, task_a, project_a, "Build lexical marmalade index"),
            (4, task_b, project_b, "Other marmalade task"),
        ] {
            repo.apply_event(&env(
                seq,
                Event::TaskCreated {
                    task: NewTask {
                        id: Some(task_id),
                        project_id: Some(project_id),
                        title: title.into(),
                        description: Some("Search body".into()),
                        status: Some(Status::Todo),
                        priority: Some(Priority::P2),
                        triage_state: None,
                        due_at: None,
                    },
                },
            ))
            .await
            .unwrap();
        }

        repo.apply_event(&env(
            5,
            Event::CommentAdded {
                comment: Comment {
                    id: comment_id,
                    task_id: task_a,
                    author: Actor::user(),
                    body: "Comment mentions rhubarb".into(),
                    parent_id: None,
                    kind: None,
                    created_at: now,
                    edited_at: None,
                    deleted_at: None,
                },
            },
        ))
        .await
        .unwrap();

        repo.apply_event(&env(
            6,
            Event::DocumentCreated {
                document: Document {
                    id: document_id,
                    project_id: project_a,
                    kind: DocumentKind::HumanLog,
                    title: "Research archive".into(),
                    content: "Document mentions persimmon".into(),
                    created_at: now,
                    updated_at: now,
                    archived_at: None,
                },
            },
        ))
        .await
        .unwrap();

        let scoped = repo
            .search("marmalade", 10, Some(&project_a.to_string()))
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].node.id, task_node_id(&task_a));

        let comment_hits = repo.search("rhubarb", 10, None).await.unwrap();
        assert_eq!(comment_hits[0].node.id, comment_node_id(&comment_id));

        let document_hits = repo.search("persimmon", 10, None).await.unwrap();
        assert_eq!(document_hits[0].node.id, document_node_id(&document_id));
    }

    #[tokio::test]
    async fn search_near_boosts_structurally_adjacent_hits() {
        let repo = repo().await;
        let now = Utc::now();
        let project_id = ProjectId::new();
        let anchor = TaskId::new();
        let adjacent = TaskId::new();
        let unrelated = TaskId::new();
        let relation_id = RelationId::new();

        repo.apply_event(&env(
            1,
            Event::ProjectCreated {
                project: Project {
                    id: project_id,
                    tenant_id: None,
                    slug: "near".into(),
                    title: "Near".into(),
                    description: None,
                    triage_enabled: false,
                    created_at: now,
                    updated_at: now,
                },
            },
        ))
        .await
        .unwrap();

        for (seq, task_id, title) in [
            (2, anchor, "Anchor task"),
            (3, adjacent, "Need apricot follow-up"),
            (4, unrelated, "Need apricot unrelated"),
        ] {
            repo.apply_event(&env(
                seq,
                Event::TaskCreated {
                    task: NewTask {
                        id: Some(task_id),
                        project_id: Some(project_id),
                        title: title.into(),
                        description: None,
                        status: Some(Status::Todo),
                        priority: Some(Priority::P2),
                        triage_state: None,
                        due_at: None,
                    },
                },
            ))
            .await
            .unwrap();
        }

        repo.apply_event(&env(
            5,
            Event::TaskLinked {
                relation_id,
                from: anchor,
                to: adjacent,
                kind: RelationKind::RelatesTo,
                actor: Actor::user(),
                occurred_at: now,
            },
        ))
        .await
        .unwrap();

        let hits = repo
            .search_near(
                "apricot",
                2,
                Some(&project_id.to_string()),
                &task_node_id(&anchor),
            )
            .await
            .unwrap();
        assert_eq!(hits[0].node.id, task_node_id(&adjacent));
        assert!(
            hits.iter()
                .any(|hit| hit.node.id == task_node_id(&unrelated)),
            "unrelated FTS hit should still be present: {hits:?}"
        );
    }
}
