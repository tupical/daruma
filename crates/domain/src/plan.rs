//! Plan entity — a goal with an ordered list of tasks an agent works through.

use daruma_shared::{time, PlanId, ProjectId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::task::Status;

/// Deserialise `Option<Option<T>>` with proper three-way semantics:
/// - key absent  → `None`          (no change intended)
/// - key = null  → `Some(None)`    (unparent / clear)
/// - key = value → `Some(Some(v))` (set / re-parent)
pub fn deserialize_double_option<'de, T, D>(
    d: D,
) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

use crate::agent::Actor;
use crate::task::GitContext;

/// Longest accepted `source_ref`, in characters.
pub const SOURCE_REF_MAX_LEN: usize = 2048;

/// Query parameters dropped from `http(s)` refs: they carry credentials,
/// never identity (matched case-insensitively).
const SECRET_QUERY_PARAMS: &[&str] = &[
    "token",
    "private_token",
    "access_token",
    "api_key",
    "apikey",
    "sig",
    "signature",
    "password",
    "secret",
];

/// Drop `k=v` parameters whose (percent-decoded) name is a secret; keep the
/// rest in order. `None` when nothing is left.
fn strip_secret_params(params: &str) -> Option<String> {
    let kept: Vec<&str> = params
        .split('&')
        .filter(|param| {
            let name = percent_decode(param.split('=').next().unwrap_or_default());
            !name.is_empty()
                && !SECRET_QUERY_PARAMS
                    .iter()
                    .any(|secret| name.eq_ignore_ascii_case(secret))
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Decode `%XX` escapes; malformed escapes stay as written.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Normalise a plan source URI (ADR-0009). The scheme is the channel, so one
/// is required; it is lower-cased for every ref (RFC 3986). `http(s)` refs
/// need a host; they lose userinfo, the default port, a trailing `/` and
/// secret query parameters, while the host is lower-cased. Query and
/// fragment otherwise stay — they identify things (`?id=1`, `#inbox/<id>`).
/// Other schemes keep everything after the scheme as given.
pub fn normalize_source_ref(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("source_ref must not be empty".into());
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("source_ref must not contain whitespace or control characters".into());
    }
    let scheme = s.split(':').next().unwrap_or_default();
    let valid_scheme = s.contains(':')
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    if !valid_scheme {
        return Err(format!(
            "source_ref `{s}` must be a URI with a scheme (e.g. https://…, mailto:…, self://…)"
        ));
    }
    let lower = scheme.to_ascii_lowercase();
    let rest = &s[scheme.len() + 1..];
    let out = if lower == "http" || lower == "https" {
        let Some(after) = rest.strip_prefix("//") else {
            return Err(format!(
                "source_ref `{s}` must be an absolute {lower}:// URL"
            ));
        };
        // Userinfo is only an `@` inside the authority, i.e. before the
        // first `/`, `?` or `#` (`https://u:p/ss@host` has none).
        let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let (authority, tail) = after.split_at(authority_end);
        let (before_fragment, fragment) = match tail.split_once('#') {
            Some((b, f)) => (b, Some(f)),
            None => (tail, None),
        };
        let (path, query) = match before_fragment.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (before_fragment, None),
        };
        let host_port = &authority[authority.rfind('@').map_or(0, |i| i + 1)..];
        let default_port = if lower == "http" { ":80" } else { ":443" };
        let host_port = host_port.strip_suffix(default_port).unwrap_or(host_port);
        // A port, if any, is digits only (`https://u:p/ss@Host` is not a
        // host `u` on port `p`); a bracketed IPv6 host has its own colons.
        // Without brackets a host has at most one `:` (a bare IPv6 is not a
        // host); a bracketed IPv6 host has its own colons.
        let bare_ipv6 = !host_port.starts_with('[') && host_port.matches(':').count() > 1;
        let valid_host = !bare_ipv6
            && match host_port.rsplit_once(':') {
                Some((host, port)) if !host_port.ends_with(']') => {
                    !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
                }
                _ => !host_port.is_empty(),
            };
        if !valid_host {
            return Err(format!("source_ref `{s}` has no valid host"));
        }
        let mut out = format!(
            "{lower}://{}{}",
            host_port.to_ascii_lowercase(),
            path.trim_end_matches('/')
        );
        if let Some(query) = query.and_then(strip_secret_params) {
            out.push('?');
            out.push_str(&query);
        }
        // A fragment of `k=v` pairs (OAuth implicit flow) gets the same
        // secret filter; any other fragment (`#inbox/<id>`) is identity.
        // A hash route (`#/cb?k=v`) keeps its route and filters its params;
        // any part holding `k=v` pairs (route or params) gets the filter.
        let filter = |part: &str| match part.contains('=') {
            true => strip_secret_params(part).unwrap_or_default(),
            false => part.to_string(),
        };
        let fragment = fragment
            .map(|f| match f.split_once('?') {
                Some((route, params)) => match strip_secret_params(params) {
                    Some(params) => format!("{}?{params}", filter(route)),
                    None => filter(route),
                },
                None => filter(f),
            })
            .filter(|f| !f.is_empty());
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(&fragment);
        }
        out
    } else {
        format!("{lower}:{rest}")
    };
    if out.chars().count() > SOURCE_REF_MAX_LEN {
        return Err(format!(
            "source_ref is longer than {SOURCE_REF_MAX_LEN} characters"
        ));
    }
    Ok(out)
}

/// Status of a Plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Draft,
    Active,
    Completed,
    Abandoned,
}

/// Top-level plan entity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan_id: Option<PlanId>,
    pub title: String,
    pub description: String,
    pub goal: String,
    pub success_criteria: Vec<String>,
    pub status: PlanStatus,
    pub owner: Actor,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    /// §3.8.10 provenance: free-text "brief" that produced this plan
    /// (typically the original user prompt). Opaque blob; the producer
    /// chooses what to store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_brief: Option<String>,
    /// ADR-0009 source umbrella: normalised URI of where the work came from
    /// (issue, e-mail, `self://…`). The scheme is the channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

/// Associates a task with a plan at a given position.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanTask {
    pub plan_id: PlanId,
    pub task_id: TaskId,
    pub position: u32,
    /// Minimal DAG: IDs of tasks that must complete before this one.
    pub depends_on: Vec<TaskId>,
}

/// Derived progress snapshot — computed on read, never stored directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanProgress {
    pub tasks_total: u32,
    pub tasks_done: u32,
    pub sub_plans_total: u32,
    pub sub_plans_done: u32,
    /// 0.0..=100.0
    pub completion_pct: f32,
}

/// Executor-oriented progress snapshot for a single plan's task list.
///
/// Counts only direct `plan_tasks` members (not nested sub-plans). Used by
/// `GET /v1/plans/{id}/progress` and `daruma_plan_progress`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanProgressSummary {
    pub total: u32,
    pub done: u32,
    pub in_progress: u32,
    /// Tasks in `inbox` or `todo` (not yet started).
    pub todo: u32,
    /// First eligible task per [`NextTaskResolver`] when the plan is `Active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_ready: Option<daruma_shared::TaskId>,
}

/// Node in a plan execution graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanGraphNode {
    pub task_id: TaskId,
    pub position: u32,
    pub depends_on: Vec<TaskId>,
    pub title: String,
    pub status: Status,
}

/// Directed edge in a plan execution graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanGraphEdge {
    pub from: TaskId,
    pub to: TaskId,
    /// `depends_on` for plan-local dependencies, `blocks` for task relations.
    pub kind: String,
}

/// DAG-shaped read model for a plan's direct task list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanGraph {
    pub nodes: Vec<PlanGraphNode>,
    pub edges: Vec<PlanGraphEdge>,
}

/// One parallel execution wave for a plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanFanoutWave {
    pub wave: u32,
    pub tasks: Vec<TaskId>,
}

/// Blocker details returned by `daruma_can_start`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanStartBlocker {
    pub task_id: TaskId,
    pub title: String,
    pub status: Status,
}

/// A lifecycle rule standing between the task and `in_progress`.
///
/// Deliberately a separate list from [`CanStartBlocker`]: "waiting on another
/// task" and "a requirement is not met" are different problems with different
/// fixes, and collapsing them would tell the caller to go look at the wrong
/// thing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanStartRule {
    pub rule_key: String,
    pub message: String,
}

/// Readiness result for starting or continuing work on a task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanStart {
    pub ready: bool,
    pub blockers: Vec<CanStartBlocker>,
    /// `required` rules that would block the transition into `in_progress`.
    /// Non-empty means `ready == false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_blockers: Vec<CanStartRule>,
    /// `recommendation` rules: surfaced, but they do not block the transition,
    /// so they never move `ready`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_warnings: Vec<CanStartRule>,
    pub reason: String,
}

/// Sparse update for an existing Plan.
///
/// `None` outer = no change.  `Some(v)` = set to v.
///
/// `parent_plan_id` uses a three-way encoding:
/// - absent / `None`       → no change
/// - `Some(None)`          → unparent (set to NULL)
/// - `Some(Some(id))`      → re-parent to `id`
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_criteria: Option<Vec<String>>,
    /// Three-way parent field: absent = no change, `null` = unparent, `"<id>"` = re-parent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_double_option"
    )]
    pub parent_plan_id: Option<Option<PlanId>>,
}

impl PlanPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.goal.is_none()
            && self.success_criteria.is_none()
            && self.parent_plan_id.is_none()
    }

    pub fn apply(self, plan: &mut Plan) {
        if let Some(t) = self.title {
            plan.title = t;
        }
        if let Some(d) = self.description {
            plan.description = d;
        }
        if let Some(g) = self.goal {
            plan.goal = g;
        }
        if let Some(sc) = self.success_criteria {
            plan.success_criteria = sc;
        }
        if let Some(p) = self.parent_plan_id {
            plan.parent_plan_id = p;
        }
        plan.updated_at = time::now();
    }
}

/// Input for creating a new Plan.
///
/// Analogous to [`daruma_domain::NewTask`].  Optional fields default to
/// empty / absent when not supplied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewPlan {
    pub project_id: ProjectId,
    pub title: String,
    pub owner: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_criteria: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan_id: Option<PlanId>,
    /// §3.8.10 provenance: free-text brief that produced this plan
    /// (typically the original user prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_brief: Option<String>,
    /// ADR-0009 source umbrella URI. At materialize the server may also
    /// derive it (parent plan, project `intake_source` policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// Intake-only Git work context (not stored on the plan): its `branch`
    /// feeds the project policy's `derive` rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_context: Option<GitContext>,
    /// Intake-only source chain (ADR-0009): the nearest node and its
    /// `upstream`. Its ref (or a synthetic `note:`) becomes `source_ref`;
    /// the nodes are written as `SourceUpserted`, not stored on the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::source::SourceInput>,
}

impl NewPlan {
    /// Minimal constructor — all optional fields default to absent.
    pub fn new(title: impl Into<String>, project_id: ProjectId, owner: Actor) -> Self {
        Self {
            project_id,
            title: title.into(),
            owner,
            description: None,
            goal: None,
            success_criteria: None,
            parent_plan_id: None,
            source_brief: None,
            source_ref: None,
            git_context: None,
            source: None,
        }
    }

    /// Materialise into a full [`Plan`] given a pre-allocated id and wall-clock `now`.
    pub fn into_plan(self, id: PlanId, now: Timestamp) -> Plan {
        Plan {
            id,
            project_id: self.project_id,
            parent_plan_id: self.parent_plan_id,
            title: self.title,
            description: self.description.unwrap_or_default(),
            goal: self.goal.unwrap_or_default(),
            success_criteria: self.success_criteria.unwrap_or_default(),
            status: PlanStatus::default(),
            owner: self.owner,
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: self.source_brief,
            source_ref: self.source_ref,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::{PlanId, ProjectId, TaskId};

    fn make_plan() -> Plan {
        let now = time::now();
        Plan {
            id: PlanId::new(),
            project_id: ProjectId::new(),
            parent_plan_id: None,
            title: "Test plan".to_string(),
            description: "A description".to_string(),
            goal: "Achieve something".to_string(),
            success_criteria: vec!["criterion 1".to_string()],
            status: PlanStatus::Draft,
            owner: Actor::user(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            source_brief: None,
            source_ref: None,
        }
    }

    #[test]
    fn normalize_source_ref_branches() {
        let n = |s: &str| normalize_source_ref(s);
        // http(s): lower scheme/host, no userinfo/default port/trailing `/`,
        // secret params dropped, other query params and fragment kept.
        assert_eq!(
            n("  HTTPS://user:tok@GitLab.X:443/G/P/-/issues/12/?private_token=s&ID=1#note_1 ")
                .unwrap(),
            "https://gitlab.x/G/P/-/issues/12?ID=1#note_1"
        );
        assert_eq!(n("http://Host.IO:80/").unwrap(), "http://host.io");
        assert_eq!(n("http://host.io:8080/a").unwrap(), "http://host.io:8080/a");
        assert_eq!(
            n("https://x.io/p?private_token=x&id=1").unwrap(),
            "https://x.io/p?id=1"
        );
        assert_eq!(
            n("https://x.io/p?Token=a&SIG=b&api_key=c").unwrap(),
            "https://x.io/p"
        );
        assert_eq!(
            n("https://x.io/p?b=2&a=1").unwrap(),
            "https://x.io/p?b=2&a=1"
        );
        // Trackers use `key` as identity; names are %-decoded before matching.
        assert_eq!(
            n("https://jira.x/browse?key=PROJ-1&auth=a").unwrap(),
            "https://jira.x/browse?key=PROJ-1&auth=a"
        );
        assert_eq!(
            n("https://x.io/p?private%5Ftoken=x&id=1&%74oken=y").unwrap(),
            "https://x.io/p?id=1"
        );
        // Fragment `k=v` params get the same filter; other fragments stay.
        assert_eq!(
            n("https://app.x/cb#access_token=abc&state=s").unwrap(),
            "https://app.x/cb#state=s"
        );
        assert_eq!(
            n("https://app.x/cb#access_token=abc").unwrap(),
            "https://app.x/cb"
        );
        // `@` after the first `/` is path, not userinfo.
        assert_eq!(n("https://Host.x/ss@A/x").unwrap(), "https://host.x/ss@A/x");
        assert!(n("https://u:p/ss@Host/x")
            .unwrap_err()
            .contains("has no valid host"));
        assert!(n("https://host:/x").is_err());
        assert_eq!(n("https://[::1]:8443/x").unwrap(), "https://[::1]:8443/x");
        assert_eq!(n("https://[::1]/x").unwrap(), "https://[::1]/x");
        assert!(n("https://::1/x").is_err());
        assert!(n("https://fe80::1:8080/x").is_err());
        // A route part holding `k=v` gets the secret filter too.
        assert_eq!(
            n("https://app.x/#access_token=x?y").unwrap(),
            "https://app.x#?y"
        );
        assert_eq!(
            n("https://app.x/#access_token=x&s=1?y").unwrap(),
            "https://app.x#s=1?y"
        );
        // Hash-route params are filtered after the first `?`.
        assert_eq!(
            n("https://app.x/#/cb?access_token=x&s=1").unwrap(),
            "https://app.x#/cb?s=1"
        );
        assert_eq!(
            n("https://app.x/#/cb?access_token=x").unwrap(),
            "https://app.x#/cb"
        );
        assert_ne!(
            n("https://bugzilla.x/show_bug.cgi?id=1").unwrap(),
            n("https://bugzilla.x/show_bug.cgi?id=2").unwrap()
        );
        assert_eq!(
            n("https://mail.google.com/mail/u/0/#inbox/FMfcg123").unwrap(),
            "https://mail.google.com/mail/u/0#inbox/FMfcg123"
        );
        // Other schemes: scheme lower-cased, the rest kept verbatim.
        assert_eq!(
            n(" mailto:Client@Acme.ru#<Msg-1@x> ").unwrap(),
            "mailto:Client@Acme.ru#<Msg-1@x>"
        );
        assert_eq!(n("SELF://x").unwrap(), "self://x");
        assert_eq!(n("self://Alice").unwrap(), "self://Alice");
        // Errors.
        for bad in [
            "   ",
            "gitlab issue 12",
            "1abc:foo",
            ":foo",
            "self://a\nb",
            "self://a b",
            "https:foo",
            "https://",
            "https:///x",
            "https://user@/x",
        ] {
            assert!(n(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(n(&format!("verbal://{}", "x".repeat(2048))).is_err());
        assert!(n(&format!("verbal://{}", "x".repeat(2000))).is_ok());
    }

    #[test]
    fn plan_roundtrip_serde() {
        let plan = make_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_with_parent_roundtrip_serde() {
        let mut plan = make_plan();
        plan.parent_plan_id = Some(PlanId::new());
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_status_roundtrip_serde() {
        for status in [
            PlanStatus::Draft,
            PlanStatus::Active,
            PlanStatus::Completed,
            PlanStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: PlanStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn plan_status_snake_case() {
        assert_eq!(
            serde_json::to_string(&PlanStatus::Active).unwrap(),
            "\"active\""
        );
    }

    #[test]
    fn plan_task_roundtrip_serde() {
        let pt = PlanTask {
            plan_id: PlanId::new(),
            task_id: TaskId::new(),
            position: 0,
            depends_on: vec![TaskId::new()],
        };
        let json = serde_json::to_string(&pt).unwrap();
        let back: PlanTask = serde_json::from_str(&json).unwrap();
        assert_eq!(pt, back);
    }

    #[test]
    fn plan_progress_roundtrip_serde() {
        let progress = PlanProgress {
            tasks_total: 5,
            tasks_done: 2,
            sub_plans_total: 1,
            sub_plans_done: 0,
            completion_pct: 40.0,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: PlanProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(progress, back);
    }

    #[test]
    fn plan_patch_roundtrip_serde() {
        let patch = PlanPatch {
            title: Some("New title".to_string()),
            description: None,
            goal: Some("New goal".to_string()),
            success_criteria: None,
            parent_plan_id: None,
        };
        let json = serde_json::to_string(&patch).unwrap();
        let back: PlanPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(patch, back);
    }

    #[test]
    fn plan_patch_default_is_empty() {
        let patch = PlanPatch::default();
        assert!(patch.is_empty());
    }

    #[test]
    fn plan_patch_apply() {
        let mut plan = make_plan();
        let patch = PlanPatch {
            title: Some("Updated".to_string()),
            description: None,
            goal: None,
            success_criteria: None,
            parent_plan_id: None,
        };
        patch.apply(&mut plan);
        assert_eq!(plan.title, "Updated");
    }

    // ── parent_plan_id serde (absent / explicit-null / value) ─────────────────

    #[test]
    fn plan_patch_parent_absent() {
        // Key absent in JSON → None (no change intended)
        let json = r#"{"title":"x"}"#;
        let patch: PlanPatch = serde_json::from_str(json).unwrap();
        assert!(patch.parent_plan_id.is_none(), "absent key must yield None");
    }

    #[test]
    fn plan_patch_parent_unset_via_explicit_null() {
        // Explicit `null` value → Some(None) (unparent)
        let json = r#"{"parent_plan_id":null}"#;
        let patch: PlanPatch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch.parent_plan_id,
            Some(None),
            "explicit null must yield Some(None)"
        );
    }

    #[test]
    fn plan_patch_parent_reparent() {
        // String UUID value → Some(Some(id)) (re-parent)
        let id = PlanId::new();
        // Use serde_json::json! to serialise id with the same format serde uses,
        // avoiding any discrepancy with PlanId's Display impl.
        let json = serde_json::json!({ "parent_plan_id": id }).to_string();
        let patch: PlanPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(
            patch.parent_plan_id,
            Some(Some(id)),
            "id string must yield Some(Some(id))"
        );
    }
}
