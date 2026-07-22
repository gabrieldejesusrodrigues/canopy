# Linear GraphQL Reference for Canopy

Endpoint: `POST https://api.linear.app/graphql` — body `{"query": "...", "variables": {...}}`, `Content-Type: application/json`.

## 1. Auth

Personal API key goes in the header **raw, no `Bearer`**:

```
Authorization: <API_KEY>
```

(`Bearer` prefix is only for OAuth access tokens.) Create keys at **Settings → Security & access**. Keys act as the creating user; rate limits are shared across all of a user's keys.

## 2. issueCreate

Required: `teamId` (+ `title` in practice). Relevant optional inputs: `description: String` (markdown), `parentId: String` (**accepts UUID or issue identifier like `"LIN-123"`**), `stateId: String`, `labelIds: [String!]`, `priority: Int` (0=None, 1=Urgent, 2=High, 3=Medium, 4=Low), `projectId: String`, `sortOrder: Float`, `subIssueSortOrder: Float`, `id: String` (client-supplied UUIDv4 — idempotency).

```graphql
mutation IssueCreate($input: IssueCreateInput!) {
  issueCreate(input: $input) {
    success
    lastSyncId
    issue { id identifier title url state { id name type } parent { id } sortOrder }
  }
}
```

Response shape (`IssuePayload`):
```json
{"data": {"issueCreate": {"success": true, "lastSyncId": 123456789.0,
  "issue": {"id": "uuid", "identifier": "CAN-42", "title": "...", "url": "...",
            "state": {"id": "uuid", "name": "Backlog", "type": "backlog"}, "parent": null, "sortOrder": 1.0}}}}
```

`issueBatchCreate(input: IssueBatchCreateInput!)` creates a list in one transaction; `issueBatchUpdate(ids: [UUID!]!, input: IssueUpdateInput!)` updates ≤50 at once.

## 3. Sub-issue trees

- At create: pass `parentId`. Reparent later: `issueUpdate` input `parentId` (+ `subIssueSortOrder`). `parentId: null` detaches.
- Children are a paginated connection (default 50!):

```graphql
query IssueWithChildren($id: String!, $first: Int!, $after: String) {
  issue(id: $id) {
    id identifier title
    parent { id identifier }
    children(first: $first, after: $after) {
      nodes { id identifier title state { id name type } sortOrder updatedAt }
      pageInfo { hasNextPage endCursor }
    }
  }
}
```

`issue(id:)` accepts UUID or `"CAN-42"`. No "all descendants" query — recurse.

## 4. Workflow states

`WorkflowState.type` ∈ `"triage" | "backlog" | "unstarted" | "started" | "completed" | "canceled" | "duplicate"`.

```graphql
query TeamStates($teamId: String!) {
  team(id: $teamId) { id name states(first: 50) { nodes { id name type position color } } }
}
```

Move: `issueUpdate(id: $id, input: { stateId: $stateId })` — `id` also accepts human identifier.

## 5. Comments

```graphql
mutation CommentCreate($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) { success comment { id body createdAt } }
}
```
`body` is markdown. `comment` is non-null here.

## 6. Projects

`ProjectCreateInput` required: `name: String!`, `teamIds: [String!]!`. Optional: `description` (plain), `content` (markdown), `startDate`/`targetDate`, `id`.

```graphql
mutation ProjectCreate($input: ProjectCreateInput!) {
  projectCreate(input: $input) { success project { id name url } }
}
```
Attach issues via `projectId` on issueCreate/Update. `ProjectPayload.project` is **nullable**.

## 7. Labels

```graphql
mutation LabelCreate($input: IssueLabelCreateInput!) {
  issueLabelCreate(input: $input) { success issueLabel { id name color } }
}
```
`name` required; `teamId` optional (omit → workspace label), `color` hex. On update prefer **`addedLabelIds`/`removedLabelIds`** over `labelIds` (which replaces the whole set — lost-update hazard).

## 8. Querying ready work

```graphql
query ReadyWork($filter: IssueFilter!, $first: Int!, $after: String) {
  issues(filter: $filter, first: $first, after: $after, orderBy: updatedAt) {
    nodes {
      id identifier title priority sortOrder updatedAt
      state { id name type }
      parent { id }
      labels { nodes { id name } }
      description
    }
    pageInfo { hasNextPage endCursor }
  }
}
```

Filter (AND implicit; `or:`/`and:` available; comparators: `eq, neq, in, nin, null, lt, lte, gt, gte, contains, startsWith`, `*IgnoreCase`):

```json
{"filter": {
  "team":    {"id": {"eq": "<team-uuid>"}},
  "project": {"id": {"eq": "<project-uuid>"}},
  "state":   {"type": {"eq": "unstarted"}},
  "labels":  {"name": {"eq": "executor"}}
}}
```

- Relay pagination: `nodes` + `pageInfo { hasNextPage endCursor }` → `after`. Default page 50, max `first` 250 (UNVERIFIED).
- `orderBy: PaginationOrderBy` = `{ createdAt, updatedAt }` only.
- Date filters accept ISO-8601 durations: `"updatedAt": {"gt": "-P1D"}` — cheap polling trick: `{gt: "-PT2M"}`.
- `includeArchived: Boolean` available.

## 9. Concurrency safety

**No optimistic locking / CAS in the public API.** `lastSyncId` is informational; you cannot send it back as a precondition. Last write wins. Mitigations:
- One designated writer per issue (the daemon owns claim-granularity writes).
- Re-read `updatedAt` before writing — advisory only.
- `addedLabelIds`/`removedLabelIds` instead of `labelIds`.
- Client-supplied `id` on creates → idempotent retries.

## 10. Rate limits (API-key auth)

| Limit | Value |
|---|---|
| Requests | **5,000/hour** per user (all keys combined) |
| Complexity budget | **3,000,000 points/hour** |
| Single query | **10,000 points max** |

Headers: `X-RateLimit-Requests-Remaining`, `X-Complexity`, etc. On exceed: **HTTP 400** (not 429) with `errors[].extensions.code == "RATELIMITED"`.

## 11. Webhooks

Exist (issues, comments, labels, projects...) but require workspace admin + public endpoint. Canopy polls instead: `issues(filter: {updatedAt: {gt: "-PT2M"}, ...}, orderBy: updatedAt)`, dedupe by `(id, updatedAt)`, back off on `X-RateLimit-Requests-Remaining`.

## Gotchas

- **No `Bearer`** for API keys.
- GraphQL errors come back **HTTP 200** with an `errors` array (rate limit is the 400 exception) — check both.
- `issue(id:)`, `issueUpdate(id:)`, `parentId` accept `"CAN-42"` identifiers as well as UUIDs.
- `IssuePayload.issue` / `ProjectPayload.project` nullable even when `success: true` — `Option` in serde.
- `priority` 0 = none; don't sort ascending naively.
- Every connection field paginates at 50 by default — nested "small" queries silently truncate.
- Deep nesting burns complexity; prefer flat filtered queries.
