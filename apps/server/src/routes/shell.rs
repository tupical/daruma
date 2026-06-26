//! Local host-shell endpoints for the standalone web UI.

use axum::{
    extract::State,
    response::{Html, IntoResponse},
    Json,
};
use serde::Serialize;
use sqlx::Row;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct HostShellConfig {
    pub home_url: String,
    pub switcher_url: String,
    pub current_workspace_label: String,
}

#[derive(Debug)]
struct WorkspaceView {
    id: String,
    name: String,
    roots: Vec<String>,
    projects: Vec<ProjectView>,
}

#[derive(Debug)]
struct ProjectView {
    id: String,
    slug: String,
    title: String,
    roots: Vec<String>,
}

/// `GET /.well-known/daruma-shell.json`
pub async fn host_shell_config() -> impl IntoResponse {
    Json(HostShellConfig {
        home_url: "/workspaces".to_string(),
        switcher_url: "/workspaces".to_string(),
        current_workspace_label: current_workspace_label(),
    })
}

/// `GET /workspaces`
pub async fn workspace_switcher(State(state): State<AppState>) -> impl IntoResponse {
    let workspaces = load_workspaces(&state).await.unwrap_or_else(|err| {
        tracing::warn!(error = %err, "failed to load local workspaces");
        Vec::new()
    });
    Html(render_workspace_switcher(&workspaces))
}

fn current_workspace_label() -> String {
    if let Ok(label) = std::env::var("DARUMA_WORKSPACE_LABEL") {
        let label = label.trim();
        if !label.is_empty() {
            return label.to_string();
        }
    }

    "Local workspaces".to_string()
}

async fn load_workspaces(state: &AppState) -> daruma_shared::Result<Vec<WorkspaceView>> {
    let pool = state.projects.pool();
    let tenant_rows = sqlx::query("SELECT id, name FROM tenants ORDER BY created_at ASC")
        .fetch_all(pool)
        .await
        .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?;

    let mut workspaces = Vec::new();
    for tenant in tenant_rows {
        let id: String = tenant
            .try_get("id")
            .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?;
        let name: String = tenant
            .try_get("name")
            .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?;
        let roots = workspace_roots(pool, &id).await?;
        let projects = projects_for_workspace(pool, &id).await?;
        workspaces.push(WorkspaceView {
            id,
            name,
            roots,
            projects,
        });
    }

    if workspaces.is_empty() {
        workspaces.push(WorkspaceView {
            id: daruma_domain::DEFAULT_TENANT_ID.to_string(),
            name: "Self-hosted".to_string(),
            roots: Vec::new(),
            projects: state
                .projects
                .list_all()
                .await
                .map(project_views_without_roots)?,
        });
    }

    Ok(workspaces)
}

async fn workspace_roots(
    pool: &sqlx::SqlitePool,
    tenant_id: &str,
) -> daruma_shared::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT root_path FROM workspace_roots WHERE tenant_id = ? ORDER BY root_path",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))
}

async fn projects_for_workspace(
    pool: &sqlx::SqlitePool,
    tenant_id: &str,
) -> daruma_shared::Result<Vec<ProjectView>> {
    let rows = sqlx::query(
        "SELECT id, slug, title FROM projects WHERE tenant_id = ? ORDER BY created_at ASC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?;

    let mut projects = Vec::new();
    for row in rows {
        let id: String = row
            .try_get("id")
            .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?;
        let roots = project_roots(pool, &id).await?;
        projects.push(ProjectView {
            id: id.clone(),
            slug: row
                .try_get("slug")
                .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?,
            title: row
                .try_get("title")
                .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))?,
            roots,
        });
    }

    Ok(projects)
}

async fn project_roots(
    pool: &sqlx::SqlitePool,
    project_id: &str,
) -> daruma_shared::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT root_path FROM project_roots WHERE project_id = ? ORDER BY root_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| daruma_shared::CoreError::storage(e.to_string()))
}

fn project_views_without_roots(projects: Vec<daruma_domain::Project>) -> Vec<ProjectView> {
    projects
        .into_iter()
        .map(|project| ProjectView {
            id: project.id.to_string(),
            slug: project.slug,
            title: project.title,
            roots: Vec::new(),
        })
        .collect()
}

fn render_workspace_switcher(workspaces: &[WorkspaceView]) -> String {
    let workspace_items = workspaces
        .iter()
        .map(render_workspace)
        .collect::<Vec<_>>()
        .join("");

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Daruma Workspaces</title>
  <style>
    body {{ margin: 0; padding: 32px 16px; background: #111418; color: #cdd9e5; font: 14px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
    main {{ max-width: 840px; margin: 0 auto; }}
    h1 {{ color: #539bf5; font-size: 18px; text-transform: uppercase; letter-spacing: .04em; }}
    h2 {{ color: #cdd9e5; font-size: 15px; margin-top: 28px; }}
    h3 {{ color: #768390; font-size: 12px; margin: 18px 0 8px; text-transform: uppercase; }}
    ul {{ list-style: none; padding: 0; }}
    li {{ padding: 8px 0; border-bottom: 1px solid #30363d; }}
    a {{ color: #539bf5; text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    code {{ color: #768390; font-size: 12px; }}
    .workspace {{ border: 1px solid #30363d; border-radius: 6px; padding: 16px; margin: 16px 0; background: #1c2128; }}
    .workspace-head {{ display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }}
    .muted {{ color: #768390; }}
    .project-row {{ display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 16px; }}
    .roots {{ margin-top: 6px; color: #768390; font-size: 12px; }}
    .empty {{ color: #768390; }}
  </style>
</head>
<body>
  <main>
    <h1>Daruma Workspaces</h1>
    {workspace_items}
  </main>
</body>
</html>"#,
        workspace_items = workspace_items,
    )
}

fn render_workspace(workspace: &WorkspaceView) -> String {
    let roots = render_roots(&workspace.roots);
    let projects = if workspace.projects.is_empty() {
        r#"<p class="empty">No projects yet.</p>"#.to_string()
    } else {
        format!(
            "<ul>{}</ul>",
            workspace
                .projects
                .iter()
                .map(render_project)
                .collect::<Vec<_>>()
                .join("")
        )
    };

    format!(
        r#"<section class="workspace">
  <div class="workspace-head">
    <h2>{name}</h2>
    <code>{id}</code>
  </div>
  {roots}
  <h3>Projects</h3>
  {projects}
</section>"#,
        name = escape_html(&workspace.name),
        id = escape_html(&workspace.id),
        roots = roots,
        projects = projects,
    )
}

fn render_project(project: &ProjectView) -> String {
    format!(
        r#"<li>
  <div class="project-row">
    <div>
      <a href="/web/{slug}">{title}</a>
      {roots}
    </div>
    <code>{id}</code>
  </div>
</li>"#,
        slug = escape_attr(&project.slug),
        title = escape_html(&project.title),
        id = escape_html(&project.id),
        roots = render_roots(&project.roots),
    )
}

fn render_roots(roots: &[String]) -> String {
    if roots.is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="roots">{}</div>"#,
        roots
            .iter()
            .map(|root| format!("<div>{}</div>", escape_html(root)))
            .collect::<Vec<_>>()
            .join("")
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    escape_html(value).replace('"', "&quot;")
}
