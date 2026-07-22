use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{AgentRef, NewNode, Node, NodeKind, NodeState, Run};
use crate::tracker::Tracker;

// ---------------------------------------------------------------------------
// GraphQL constants
// ---------------------------------------------------------------------------

const ENDPOINT: &str = "https://api.linear.app/graphql";

const Q_TEAM_STATES: &str = r#"
query TeamStates($teamId: String!) {
  team(id: $teamId) { id name states(first: 50) { nodes { id name type position color } } }
}
"#;

const M_PROJECT_CREATE: &str = r#"
mutation ProjectCreate($input: ProjectCreateInput!) {
  projectCreate(input: $input) { success project { id name url } }
}
"#;

const M_PROJECT_UPDATE: &str = r#"
mutation ProjectUpdate($id: String!, $input: ProjectUpdateInput!) {
  projectUpdate(id: $id, input: $input) { success project { id content } }
}
"#;

const Q_PROJECT: &str = r#"
query GetProject($id: String!) {
  project(id: $id) { id name content }
}
"#;

const M_ISSUE_CREATE: &str = r#"
mutation IssueCreate($input: IssueCreateInput!) {
  issueCreate(input: $input) {
    success
    issue { id identifier title url state { id name type } parent { id } }
  }
}
"#;

const M_ISSUE_UPDATE: &str = r#"
mutation IssueUpdate($id: String!, $input: IssueUpdateInput!) {
  issueUpdate(id: $id, input: $input) {
    success
    issue { id identifier title description state { id name type } }
  }
}
"#;

const M_COMMENT_CREATE: &str = r#"
mutation CommentCreate($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) { success comment { id body createdAt } }
}
"#;

const Q_ISSUE: &str = r#"
query GetIssue($id: String!) {
  issue(id: $id) {
    id identifier title description
    state { id name type }
    parent { id }
    updatedAt
  }
}
"#;

const Q_ISSUES_IN_PROJECT: &str = r#"
query IssuesInProject($filter: IssueFilter!, $first: Int!, $after: String) {
  issues(filter: $filter, first: $first, after: $after, orderBy: updatedAt) {
    nodes {
      id identifier title description
      state { id name type }
      parent { id }
      updatedAt
    }
    pageInfo { hasNextPage endCursor }
  }
}
"#;

// ---------------------------------------------------------------------------
// Metadata footer embedded in issue description
// ---------------------------------------------------------------------------

const META_MARKER: &str = "\n\n<!-- canopy:";

/// Machine-readable canopy metadata stored at the END of every issue description.
/// The authoritative state lives here; the Linear workflow state is a best-effort mirror.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub run_id: String,
    pub kind: NodeKind,
    pub state: NodeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRef>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub depth: u32,
    #[serde(default)]
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
}

/// Encode meta as footer appended to a user-visible description body.
fn encode_description(body: &str, meta: &Meta) -> String {
    let json = serde_json::to_string(meta).unwrap();
    format!("{body}{META_MARKER}{json} -->")
}

/// Split description into (body, meta). Returns None for meta if no marker.
fn split_description(description: &str) -> (&str, Option<Meta>) {
    if let Some(marker_pos) = description.rfind(META_MARKER) {
        let body = &description[..marker_pos];
        let rest = &description[marker_pos + META_MARKER.len()..];
        // rest is `{json} -->`
        if let Some(end) = rest.rfind(" -->") {
            let json_str = &rest[..end];
            if let Ok(meta) = serde_json::from_str::<Meta>(json_str) {
                return (body, Some(meta));
            }
        }
    }
    (description, None)
}

// ---------------------------------------------------------------------------
// Project-level footer (stores run metadata on the Linear project)
// ---------------------------------------------------------------------------

const RUN_MARKER: &str = "<!-- canopy-run:";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RunMeta {
    branch: String,
    root_node: String,
    objective: String,
}

fn encode_run_content(run_meta: &RunMeta) -> String {
    let json = serde_json::to_string(run_meta).unwrap();
    format!("{RUN_MARKER}{json} -->")
}

fn parse_run_content(content: &str) -> Option<RunMeta> {
    let start = content.find(RUN_MARKER)? + RUN_MARKER.len();
    let rest = &content[start..];
    let end = rest.find(" -->")?;
    serde_json::from_str(&rest[..end]).ok()
}

// ---------------------------------------------------------------------------
// Linear API response types (minimal, serde structs)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    #[serde(default)]
    errors: Vec<GraphqlError>,
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
    #[serde(default)]
    extensions: Option<ErrorExtensions>,
}

#[derive(Debug, Deserialize)]
struct ErrorExtensions {
    code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkflowStateNode {
    id: String,
    #[serde(rename = "type")]
    state_type: String,
}

#[derive(Debug, Deserialize)]
struct IssueNode {
    id: String,
    title: String,
    description: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<DateTime<Utc>>,
    parent: Option<ParentRef>,
}

#[derive(Debug, Deserialize)]
struct ParentRef {
    id: String,
}

// ---------------------------------------------------------------------------
// LinearTracker
// ---------------------------------------------------------------------------

pub struct LinearTracker {
    client: Client,
    api_key: String,
    team_id: String,
    /// Cached mapping: NodeState -> Linear workflow state id.
    state_map: HashMap<String, String>,
}

impl LinearTracker {
    pub async fn connect(api_key: String, team_id: String) -> Result<Self> {
        let client = Client::new();

        // Fetch workflow states and build NodeState -> stateId mapping.
        let resp = Self::gql_raw(&client, &api_key, Q_TEAM_STATES, json!({"teamId": team_id})).await?;
        let states: Vec<WorkflowStateNode> = serde_json::from_value(
            resp["team"]["states"]["nodes"].clone(),
        ).context("parsing team states")?;

        // Pick one state per Linear type category (lowest position / first encountered).
        let mut by_type: HashMap<String, String> = HashMap::new();
        for s in states {
            by_type.entry(s.state_type).or_insert(s.id);
        }

        let pick = |t: &str| -> Result<String> {
            by_type.get(t).cloned().with_context(|| format!("no Linear workflow state of type '{t}' found in team"))
        };

        // Map canopy states to Linear workflow types.
        // decomposed -> started (planner still owns its subtree).
        let mut state_map = HashMap::new();
        state_map.insert("ready".into(),       pick("unstarted")?);
        state_map.insert("running".into(),     pick("started")?);
        state_map.insert("decomposed".into(),  pick("started")?);
        state_map.insert("needs_merge".into(), pick("started")?);
        state_map.insert("merging".into(),     pick("started")?);
        state_map.insert("in_review".into(),   pick("started")?);
        state_map.insert("blocked".into(),     pick("backlog")?);
        state_map.insert("done".into(),        pick("completed")?);
        state_map.insert("failed".into(),      pick("canceled")?);

        Ok(Self { client, api_key, team_id, state_map })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn gql_raw(client: &Client, api_key: &str, query: &str, variables: Value) -> Result<Value> {
        Self::gql_raw_with_retry(client, api_key, query, variables).await
    }

    async fn gql_raw_with_retry(client: &Client, api_key: &str, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });

        let resp = client
            .post(ENDPOINT)
            .header("Authorization", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending GraphQL request")?;

        let status = resp.status();

        // HTTP 400 with RATELIMITED: sleep 30s and retry once.
        if status == 400 {
            let text = resp.text().await.unwrap_or_default();
            if text.contains("RATELIMITED") {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                return Box::pin(Self::gql_raw_with_retry(client, api_key, query, variables)).await;
            }
            anyhow::bail!("Linear HTTP 400: {text}");
        }

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Linear HTTP {status}: {text}");
        }

        let parsed: GraphqlResponse = resp.json().await.context("parsing GraphQL response")?;

        // HTTP 200 can still carry errors (GraphQL spec).
        if !parsed.errors.is_empty() {
            let msgs: Vec<_> = parsed.errors.iter().map(|e| e.message.as_str()).collect();
            anyhow::bail!("Linear GraphQL errors: {}", msgs.join("; "));
        }

        parsed.data.context("GraphQL response had no data field")
    }

    async fn gql(&self, query: &str, variables: Value) -> Result<Value> {
        Self::gql_raw(&self.client, &self.api_key, query, variables).await
    }

    fn linear_state_id(&self, state: NodeState) -> &str {
        self.state_map.get(state.as_str()).map(|s| s.as_str()).unwrap_or("")
    }

    /// Fetch an issue's description, mutate the metadata, and write it back.
    /// Returns the updated Meta.
    async fn mutate_meta<F>(&self, id: &str, f: F) -> Result<Meta>
    where
        F: FnOnce(&mut Meta) + Send,
    {
        let data = self.gql(Q_ISSUE, json!({ "id": id })).await?;
        let issue: IssueNode = serde_json::from_value(data["issue"].clone())
            .context("parsing issue")?;
        let desc = issue.description.as_deref().unwrap_or("");
        let (body, existing_meta) = split_description(desc);
        let mut meta = existing_meta.context("issue has no canopy metadata")?;
        f(&mut meta);
        let new_state_id = self.linear_state_id(meta.state);
        let new_desc = encode_description(body, &meta);
        self.gql(M_ISSUE_UPDATE, json!({
            "id": id,
            "input": {
                "description": new_desc,
                "stateId": new_state_id,
            }
        })).await?;
        Ok(meta)
    }

    /// Parse a Linear issue node into a canopy Node given pre-fetched title info.
    fn issue_to_node(issue: IssueNode, run_id: &str) -> Option<Node> {
        let desc = issue.description.as_deref().unwrap_or("");
        let (_, meta) = split_description(desc);
        let meta = meta?; // no canopy marker → ignore

        let updated_at = issue.updated_at.unwrap_or_else(Utc::now);

        Some(Node {
            id: issue.id,
            run_id: run_id.to_string(),
            parent_id: issue.parent.map(|p| p.id),
            kind: meta.kind,
            state: meta.state,
            title: issue.title,
            spec: {
                // body part of the description is the spec
                let desc2 = issue.description.as_deref().unwrap_or("");
                let (body, _) = split_description(desc2);
                body.to_string()
            },
            agent: meta.agent,
            depends_on: meta.depends_on,
            depth: meta.depth,
            attempt: meta.attempt,
            claimed_at: meta.claimed_at,
            updated_at,
        })
    }
}

#[async_trait]
impl Tracker for LinearTracker {
    async fn init_run(&self, objective: &str, branch: &str) -> Result<Run> {
        // Truncate project name to 60 chars + timestamp for uniqueness.
        let ts = Utc::now().format("%Y%m%d-%H%M%S");
        let short = if objective.len() > 60 { &objective[..60] } else { objective };
        let project_name = format!("{short} [{ts}]");

        let data = self.gql(M_PROJECT_CREATE, json!({
            "input": {
                "name": project_name,
                "teamIds": [self.team_id],
            }
        })).await?;

        let project = data["projectCreate"]["project"].as_object()
            .context("projectCreate returned null project")?;
        let run_id = project["id"].as_str().context("project id missing")?.to_string();

        // Create root Plan issue.
        let root_node_id = uuid::Uuid::new_v4().to_string();
        let meta = Meta {
            run_id: run_id.clone(),
            kind: NodeKind::Plan,
            state: NodeState::Ready,
            agent: None,
            depends_on: vec![],
            depth: 0,
            attempt: 0,
            claimed_at: None,
        };
        let state_id = self.linear_state_id(NodeState::Ready);
        let description = encode_description(objective, &meta);

        let data = self.gql(M_ISSUE_CREATE, json!({
            "input": {
                "id": root_node_id,
                "teamId": self.team_id,
                "projectId": run_id,
                "title": "Objective",
                "description": description,
                "stateId": state_id,
            }
        })).await?;

        let issue = data["issueCreate"]["issue"].as_object()
            .context("issueCreate returned null issue")?;
        let actual_root_id = issue["id"].as_str().context("issue id missing")?.to_string();

        // Store branch + root_node on the project so load_run can reconstruct Run faithfully.
        let run_meta = RunMeta {
            branch: branch.to_string(),
            root_node: actual_root_id.clone(),
            objective: objective.to_string(),
        };
        self.gql(M_PROJECT_UPDATE, json!({
            "id": run_id,
            "input": { "content": encode_run_content(&run_meta) }
        })).await?;

        let now = Utc::now();
        Ok(Run {
            id: run_id,
            objective: objective.to_string(),
            root_node: actual_root_id,
            branch: branch.to_string(),
            created_at: now,
        })
    }

    async fn load_run(&self, run_id: &str) -> Result<Run> {
        let data = self.gql(Q_PROJECT, json!({ "id": run_id })).await?;
        let content = data["project"]["content"].as_str().unwrap_or("");
        let run_meta = parse_run_content(content)
            .with_context(|| format!("project {run_id} has no canopy-run footer"))?;
        Ok(Run {
            id: run_id.to_string(),
            objective: run_meta.objective,
            root_node: run_meta.root_node,
            branch: run_meta.branch,
            created_at: Utc::now(), // creation time not stored; use now as a stand-in
        })
    }

    async fn create_node(&self, new: NewNode) -> Result<Node> {
        let node_id = uuid::Uuid::new_v4().to_string();
        let state = if new.ready { NodeState::Ready } else { NodeState::Blocked };
        let meta = Meta {
            run_id: new.run_id.clone(),
            kind: new.kind,
            state,
            agent: new.agent.clone(),
            depends_on: new.depends_on.clone(),
            depth: new.depth,
            attempt: 0,
            claimed_at: None,
        };
        let state_id = self.linear_state_id(state);
        let description = encode_description(&new.spec, &meta);

        let mut input = json!({
            "id": node_id,
            "teamId": self.team_id,
            "projectId": new.run_id,
            "title": new.title,
            "description": description,
            "stateId": state_id,
        });
        if let Some(pid) = &new.parent_id {
            input["parentId"] = json!(pid);
        }

        let data = self.gql(M_ISSUE_CREATE, json!({ "input": input })).await?;
        let issue = data["issueCreate"]["issue"].as_object()
            .context("issueCreate returned null issue")?;
        let actual_id = issue["id"].as_str().context("issue id missing")?.to_string();

        Ok(Node {
            id: actual_id,
            run_id: new.run_id,
            parent_id: new.parent_id,
            kind: new.kind,
            state,
            title: new.title,
            spec: new.spec,
            agent: new.agent,
            depends_on: new.depends_on,
            depth: new.depth,
            attempt: 0,
            claimed_at: None,
            updated_at: Utc::now(),
        })
    }

    async fn node(&self, id: &str) -> Result<Node> {
        let data = self.gql(Q_ISSUE, json!({ "id": id })).await?;
        let issue: IssueNode = serde_json::from_value(data["issue"].clone())
            .context("parsing issue")?;
        let run_id = {
            let desc = issue.description.as_deref().unwrap_or("");
            let (_, meta) = split_description(desc);
            meta.as_ref().map(|m| m.run_id.clone()).unwrap_or_default()
        };
        Self::issue_to_node(issue, &run_id).context("issue has no canopy metadata")
    }

    async fn children(&self, parent_id: &str) -> Result<Vec<Node>> {
        // Fetch the parent to get run_id, then query all project issues with this parentId.
        let parent_node = self.node(parent_id).await?;
        let all = self.nodes_in_state_paginated(&parent_node.run_id, None).await?;
        Ok(all.into_iter().filter(|n| n.parent_id.as_deref() == Some(parent_id)).collect())
    }

    async fn nodes_in_state(&self, run_id: &str, state: NodeState) -> Result<Vec<Node>> {
        let all = self.nodes_in_state_paginated(run_id, None).await?;
        Ok(all.into_iter().filter(|n| n.state == state).collect())
    }

    /// Atomic claim emulated with read-modify-write.
    /// Safe because the daemon is the single writer at claim granularity.
    /// ponytail: no distributed lock — single-daemon guarantee documented in design
    async fn try_claim(&self, id: &str) -> Result<Option<Node>> {
        let data = self.gql(Q_ISSUE, json!({ "id": id })).await?;
        let issue: IssueNode = serde_json::from_value(data["issue"].clone())
            .context("parsing issue")?;
        let desc = issue.description.as_deref().unwrap_or("");
        let (body, existing_meta) = split_description(desc);
        let meta = match existing_meta {
            Some(m) => m,
            None => return Ok(None),
        };
        // Only claim if currently ready.
        if meta.state != NodeState::Ready {
            return Ok(None);
        }
        let mut new_meta = meta.clone();
        new_meta.state = NodeState::Running;
        new_meta.claimed_at = Some(Utc::now());
        let state_id = self.linear_state_id(NodeState::Running);
        let new_desc = encode_description(body, &new_meta);
        let resp = self.gql(M_ISSUE_UPDATE, json!({
            "id": id,
            "input": { "description": new_desc, "stateId": state_id }
        })).await?;
        let updated_issue: IssueNode = serde_json::from_value(resp["issueUpdate"]["issue"].clone())
            .context("parsing updated issue")?;
        let run_id = new_meta.run_id.clone();
        Ok(Self::issue_to_node(updated_issue, &run_id))
    }

    async fn transition(&self, id: &str, from: NodeState, to: NodeState) -> Result<bool> {
        let data = self.gql(Q_ISSUE, json!({ "id": id })).await?;
        let issue: IssueNode = serde_json::from_value(data["issue"].clone())
            .context("parsing issue")?;
        let desc = issue.description.as_deref().unwrap_or("");
        let (body, existing_meta) = split_description(desc);
        let meta = match existing_meta {
            Some(m) => m,
            None => return Ok(false),
        };
        if meta.state != from {
            return Ok(false);
        }
        let mut new_meta = meta;
        new_meta.state = to;
        let state_id = self.linear_state_id(to);
        let new_desc = encode_description(body, &new_meta);
        self.gql(M_ISSUE_UPDATE, json!({
            "id": id,
            "input": { "description": new_desc, "stateId": state_id }
        })).await?;
        Ok(true)
    }

    async fn set_state(&self, id: &str, to: NodeState) -> Result<()> {
        self.mutate_meta(id, |m| m.state = to).await?;
        Ok(())
    }

    async fn bump_attempt(&self, id: &str) -> Result<u32> {
        let meta = self.mutate_meta(id, |m| m.attempt += 1).await?;
        Ok(meta.attempt)
    }

    async fn update_spec(&self, id: &str, spec: &str) -> Result<()> {
        // Rewrite description body while keeping metadata footer.
        let data = self.gql(Q_ISSUE, json!({ "id": id })).await?;
        let issue: IssueNode = serde_json::from_value(data["issue"].clone())
            .context("parsing issue")?;
        let desc = issue.description.as_deref().unwrap_or("");
        let (_, existing_meta) = split_description(desc);
        let meta = existing_meta.context("issue has no canopy metadata")?;
        let new_desc = encode_description(spec, &meta);
        self.gql(M_ISSUE_UPDATE, json!({
            "id": id,
            "input": { "description": new_desc }
        })).await?;
        Ok(())
    }

    async fn set_agent(&self, id: &str, agent: Option<&crate::model::AgentRef>) -> Result<()> {
        let agent_clone = agent.cloned();
        self.mutate_meta(id, |m| m.agent = agent_clone).await?;
        Ok(())
    }

    async fn comment(&self, id: &str, body: &str) -> Result<()> {
        self.gql(M_COMMENT_CREATE, json!({ "issueId": id, "body": body })).await?;
        Ok(())
    }

    async fn expire_leases(&self, run_id: &str, lease_secs: i64) -> Result<u32> {
        let cutoff = Utc::now() - chrono::Duration::seconds(lease_secs);
        let running = self.nodes_in_state(run_id, NodeState::Running).await?;
        let mut count = 0u32;
        for node in running {
            if let Some(claimed_at) = node.claimed_at {
                if claimed_at < cutoff {
                    // Reset to ready, clear claimed_at.
                    let id = node.id.clone();
                    self.mutate_meta(&id, |m| {
                        m.state = NodeState::Ready;
                        m.claimed_at = None;
                    }).await?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    async fn unblock_satisfied(&self, run_id: &str) -> Result<u32> {
        let blocked = self.nodes_in_state(run_id, NodeState::Blocked).await?;
        let all = self.nodes_in_state_paginated(run_id, None).await?;
        let done_ids: std::collections::HashSet<&str> = all.iter()
            .filter(|n| n.state == NodeState::Done)
            .map(|n| n.id.as_str())
            .collect();

        let mut count = 0u32;
        for node in blocked {
            let all_done = node.depends_on.iter().all(|dep| done_ids.contains(dep.as_str()));
            if all_done {
                let id = node.id.clone();
                self.mutate_meta(&id, |m| m.state = NodeState::Ready).await?;
                count += 1;
            }
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Pagination helper (not a trait method, used internally)
// ---------------------------------------------------------------------------

impl LinearTracker {
    /// Fetch all issues in a project, optionally filtered by Linear state type.
    /// Metadata is authoritative; Linear state type is only used to narrow the
    /// server-side result set (optional optimisation — pass None to fetch all).
    async fn nodes_in_state_paginated(&self, run_id: &str, _state_type: Option<&str>) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let variables = json!({
                "filter": {
                    "project": { "id": { "eq": run_id } },
                    "team":    { "id": { "eq": self.team_id } },
                },
                "first": 250,
                "after": cursor,
            });
            let data = self.gql(Q_ISSUES_IN_PROJECT, variables).await?;
            let page = &data["issues"];
            let issue_nodes: Vec<IssueNode> =
                serde_json::from_value(page["nodes"].clone()).context("parsing issues page")?;

            for issue in issue_nodes {
                let desc = issue.description.as_deref().unwrap_or("");
                let (_, meta) = split_description(desc);
                if let Some(meta) = meta {
                    if meta.run_id == run_id {
                        if let Some(node) = Self::issue_to_node(issue, run_id) {
                            nodes.push(node);
                        }
                    }
                }
            }

            let has_next = page["pageInfo"]["hasNextPage"].as_bool().unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = page["pageInfo"]["endCursor"].as_str().map(|s| s.to_string());
        }

        Ok(nodes)
    }
}

// ---------------------------------------------------------------------------
// Tests (no network — metadata encode/decode only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CliKind, NodeKind, NodeState};

    fn sample_meta() -> Meta {
        Meta {
            run_id: "run-abc".into(),
            kind: NodeKind::Execute,
            state: NodeState::Ready,
            agent: Some(AgentRef { cli: CliKind::Claude, model: "opus".into() }),
            depends_on: vec!["dep-1".into(), "dep-2".into()],
            depth: 3,
            attempt: 2,
            claimed_at: None,
        }
    }

    #[test]
    fn meta_roundtrip() {
        let meta = sample_meta();
        let desc = encode_description("do the thing", &meta);
        let (body, parsed) = split_description(&desc);
        assert_eq!(body, "do the thing");
        let parsed = parsed.expect("meta should be present");
        assert_eq!(parsed, meta);
    }

    #[test]
    fn meta_roundtrip_with_claimed_at() {
        let mut meta = sample_meta();
        meta.claimed_at = Some(Utc::now());
        let desc = encode_description("spec here", &meta);
        let (body, parsed) = split_description(&desc);
        assert_eq!(body, "spec here");
        let parsed = parsed.unwrap();
        assert_eq!(parsed.state, meta.state);
        assert!(parsed.claimed_at.is_some());
    }

    #[test]
    fn no_marker_returns_none() {
        let (body, meta) = split_description("just some text with no marker");
        assert_eq!(body, "just some text with no marker");
        assert!(meta.is_none());
    }

    #[test]
    fn description_split_preserves_multiline_body() {
        let meta = sample_meta();
        let body = "line one\nline two\n\nline three";
        let desc = encode_description(body, &meta);
        let (parsed_body, parsed_meta) = split_description(&desc);
        assert_eq!(parsed_body, body);
        assert_eq!(parsed_meta.unwrap(), meta);
    }

    #[test]
    fn meta_state_all_variants() {
        let states = [
            NodeState::Ready, NodeState::Running, NodeState::Decomposed,
            NodeState::NeedsMerge, NodeState::Merging, NodeState::InReview,
            NodeState::Blocked, NodeState::Done, NodeState::Failed,
        ];
        for state in states {
            let mut meta = sample_meta();
            meta.state = state;
            let desc = encode_description("spec", &meta);
            let (_, parsed) = split_description(&desc);
            assert_eq!(parsed.unwrap().state, state, "roundtrip failed for {state:?}");
        }
    }

    #[test]
    fn run_meta_roundtrip() {
        let run_meta = RunMeta {
            branch: "canopy/run-abc123".into(),
            root_node: "issue-uuid-xyz".into(),
            objective: "build a payment service".into(),
        };
        let content = encode_run_content(&run_meta);
        let parsed = parse_run_content(&content).expect("should parse");
        assert_eq!(parsed, run_meta);
    }

    #[test]
    fn run_meta_embedded_in_longer_content() {
        let run_meta = RunMeta {
            branch: "canopy/run-1".into(),
            root_node: "root-id".into(),
            objective: "do stuff".into(),
        };
        // Linear project content field may have other text around the marker.
        let content = format!("# Project notes\nSome prose.\n\n{}", encode_run_content(&run_meta));
        let parsed = parse_run_content(&content).expect("should parse embedded footer");
        assert_eq!(parsed, run_meta);
    }

    #[test]
    fn run_meta_missing_returns_none() {
        assert!(parse_run_content("no marker here").is_none());
    }

    #[test]
    fn issue_to_node_ignores_no_marker() {
        let issue = IssueNode {
            id: "abc".into(),
            title: "Some issue".into(),
            description: Some("plain text, no canopy".into()),
            updated_at: None,
            parent: None,
        };
        assert!(LinearTracker::issue_to_node(issue, "run-1").is_none());
    }

    #[test]
    fn issue_to_node_parses_correctly() {
        let meta = Meta {
            run_id: "run-1".into(),
            kind: NodeKind::Plan,
            state: NodeState::Running,
            agent: None,
            depends_on: vec![],
            depth: 0,
            attempt: 1,
            claimed_at: None,
        };
        let desc = encode_description("my spec", &meta);
        let issue = IssueNode {
            id: "issue-id".into(),
            title: "root".into(),
            description: Some(desc),
            updated_at: Some(Utc::now()),
            parent: None,
        };
        let node = LinearTracker::issue_to_node(issue, "run-1").unwrap();
        assert_eq!(node.id, "issue-id");
        assert_eq!(node.state, NodeState::Running);
        assert_eq!(node.spec, "my spec");
        assert_eq!(node.depth, 0);
        assert_eq!(node.attempt, 1);
    }
}
