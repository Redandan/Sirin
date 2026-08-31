# Sirin MCP API Reference

`http://127.0.0.1:7700/mcp` — MCP 2024-11-05 Streamable HTTP (JSON-RPC 2.0 over POST)

Port override: `SIRIN_RPC_PORT=<n>` env var at Sirin launch time (default `7700`).
When port 7700 is held by a zombie socket from a previously-killed Sirin, set
this to `7701` or similar.

Use this when Sirin is running and you want to drive it from an external agent
(Claude Code, Claude Desktop, custom scripts).

## Tool inventory contract

The accepted clean-build baseline contains **190 tools**. The canonical sorted
name list is `config/mcp_tool_baseline.json`; the unit test
`deployed_tool_inventory_matches_baseline` compares it with the directly
constructed registry plus the legacy `browser_exec` entry. A deliberate tool
addition, rename, or removal must update the baseline and this reference in the
same commit. See `docs/MCP_TOOL_PROVENANCE.md` for recovery provenance and the
deployment gate.

## sirin-call CLI

`sirin-call` is a thin Rust CLI wrapper that avoids bash shell-escaping pain
with CJK/Unicode payloads in curl.  Binary: `src/bin/sirin_call.rs`.

```bash
# Build once:
cargo build --release   # → target/release/sirin-call.exe

# key=value syntax (values auto-typed — numbers/booleans/arrays parsed as JSON):
sirin-call browser_exec action=url
sirin-call browser_exec action=ax_find role=button name=登入
sirin-call browser_exec action=wait_for_url target="#/home"

# Stdin JSON (Unicode-safe — no bash escaping needed):
echo '{"action":"ax_find","role":"button","name":"購買"}' | sirin-call browser_exec

# List available tools:
sirin-call --list

# Port override:
SIRIN_RPC_PORT=7701 sirin-call browser_exec action=url
```

Key=value pairs are auto-typed: numbers, booleans, arrays, and objects are
parsed as JSON first, falling back to plain string.  Bare strings don't need
quoting (`target=#/home` works).  Stdin JSON can supplement key=value args for
nested fields.

---

## Headless mode (no auto-launched browser)

Sirin can run without auto-opening the browser to the web UI — useful for
servers, SSH sessions, CI runners, and Docker containers. Headless mode
skips the `open_browser()` call only; the RPC/MCP server, browser
singleton (the controlled Chrome that tests drive), Telegram listeners,
and test_runner all start normally. The web UI itself remains reachable
at `http://127.0.0.1:7700/ui/` if you navigate there manually.

```bash
# CLI flag
./sirin --headless

# OR env var (precedence: same)
SIRIN_HEADLESS=1 ./sirin

# Combine with non-default RPC port
SIRIN_RPC_PORT=7710 ./sirin --headless
```

Logs the bound RPC port on startup, then parks the main thread until Ctrl-C
(or taskkill on Windows). All MCP tool catalog entries below work identically
in headless mode — the MCP transport is the same TCP listener.

## Transport

All requests POST JSON-RPC 2.0 to `/mcp`:

```bash
curl -s http://127.0.0.1:7700/mcp -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Responses follow MCP's content format — tools returning structured data wrap
it as `{"content":[{"type":"text","text":"<stringified JSON>"}]}`.

## Registration (Claude Desktop / Claude Code)

```json
{
  "mcpServers": {
    "sirin": { "url": "http://127.0.0.1:7700/mcp" }
  }
}
```

## Methods

- `initialize` — MCP handshake (returns protocol version + server info)
- `tools/list` — enumerate tools; optional `profile` reduces discovery context
- `tools/call` — invoke one tool with JSON arguments

### Compact tool discovery

`tools/list` defaults to the complete, backward-compatible catalog. AI clients
that need a smaller context can request one of `core`, `testing`, `ios`, or
`ops`:

```bash
curl -s http://127.0.0.1:7700/mcp -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"profile":"testing"}}'
```

Set `SIRIN_MCP_TOOL_PROFILE=testing` to make a compact profile the process
default; an explicit request parameter overrides the environment value. A
profiled response includes `_meta` counts for the selected and full catalogs.

This is a discovery/context optimization, not an authorization boundary:
tools omitted from a profile remain callable by exact name through
`tools/call`, with their existing validation and approval rules unchanged.

## Tool Catalog

### General (pre-existing)

#### `memory_search`
Search Sirin's memory (FTS5 SQLite).
```json
{"name":"memory_search","arguments":{"query":"...","limit":5}}
```

#### `skill_list`
List all YAML + built-in skills.
```json
{"name":"skill_list","arguments":{}}
```

The YAML catalog includes `test-health-repair`. It starts with read-only
`script_health` / `test_analytics` evidence, requires explicit approval for a
specific `test_id` before executing that test, and only reports a repair after
three consecutive replay passes. It never deletes saved scripts or weakens a
test's success criteria.

#### `teams_pending` / `teams_approve`
Manage Teams pending draft queue.
```json
{"name":"teams_approve","arguments":{"id":"<pending_id>"}}
```

#### `trigger_research`
Kick off a research task.
```json
{"name":"trigger_research","arguments":{"topic":"...","url":"..."}}
```

#### `research_sentinel_run_once`
Run one official/investment RSS research pass for a Codex-defined review goal.
Set `deep_research=true` or `deep_research_mode=auto|required` to request the
optional local LLM analysis layer. `auto` keeps the deterministic RSS result if
the LLM is unavailable; `required` records the deep research step as failed.
```json
{"name":"research_sentinel_run_once","arguments":{"topic":"AgoraMarketAPI auto trading news","deep_research":true,"max_deep_events":4}}
```

#### `a2a_worker_status`
Inspect Sirin's server-side A2A worker. The worker is a local runner only:
it claims compatible tasks from an external MCP task service, executes Sirin
capabilities such as `SIRIN_RESEARCH_SENTINEL_RUN_ONCE` and
`AGORA_MARKET_OPS_DAILY_BRIEF` / `AGORA_MARKET_ISSUE_SCOUT` /
`SIRIN_AGORA_TEST_HEALTH_SCOUT`, and reports artifacts back to the server for
Codex/server review.
```json
{"name":"a2a_worker_status","arguments":{}}
```

The worker is disabled by default. Configure `config/a2a_worker.yaml` or set
`SIRIN_A2A_WORKER_ENABLED=1`, `SIRIN_A2A_SERVER_URL`, and either
`SIRIN_A2A_BEARER` or the configured bearer env var before launch.

`AGORA_MARKET_OPS_DAILY_BRIEF` is intentionally read-only. It calls existing
AgoraMarketAPI MCP ops tools, builds a daily brief, and returns `NEEDS_REVIEW`;
it does not send Telegram messages or mutate orders, products, funds, wallets,
or trading state.

Before calling protected AgoraMarketAPI ops tools, the worker uses the OPS path:
it reads the authorization from `SIRIN_AGORA_OPS_AUTHORIZATION`, `MCP_OPS_KEY`,
or `~/.claude.json` `mcpServers.agora-ops.headers.X-OPS-Authorization`, strips
a leading `Bearer ` if present, and sends the bare token as
`X-OPS-Authorization`. Sirin intentionally does not fall back to
`mcpServers.agora-ops.headers.Authorization`, because that bearer may be an
external-AI key and would re-enter the Telegram batch-approval path. Sirin does
not ask for external-AI Telegram approval before scouting. If OPS authorization
is missing, Sirin stops the
marketplace collection and records a single `mcp_auth_required` /
`INTEGRATION_BLOCKER` candidate targeted at Sirin with `allowedAction:
CODEX_VERIFY_INTEGRATION`. Tool-level OPS permission failures are recorded as
checks/candidates for Codex integration verification instead of blocking the
whole report. Sirin must not turn `BATCH_PLAN_REQUIRED` into AgoraMarket product
issues.

`AGORA_MARKET_ISSUE_SCOUT` uses the same read-only evidence pass, then emits
an `ops_report` plus structured `issue_candidates`. `ops_report` is the
operator-facing report: it always contains the six categories `service`,
`product`, `seller`, `order`, `growth`, and `support`, each with read-only
checks, candidate issues, `evidenceLevel`, `allowedAction`, next actor, and
next action. Top-level `reportStatus` is `READY_FOR_REVIEW` when marketplace
evidence is collected, or `BLOCKED_INTEGRATION` for Sirin/OPS MCP integration
blockers. Explicit healthy states such as no pending orders, no dispute
backlog, cleared ops backlog, or zero pending KB gaps remain as read-only checks
and do not create issue candidates. The report includes `actionSummary`, a
compact decision layer with counts for new issue candidates, handoff-only
candidates, already tracked external issues, pending inbox review, healthy
checks, blocked checks, and a recommended next step. If
marketplace MCP evidence is blocked, empty categories are marked
`blocked_pending_marketplace_evidence` with a `coverageGap` that names the
Sirin/MCP integration blocker, `evidenceLevel`, next actor, and allowed action;
the same gaps are also exposed at top-level `coverageGaps` for machine-readable
handoff. OPS authorization/configuration blockers use `CODEX` /
`CODEX_VERIFY_INTEGRATION`, because Sirin should pass through the OPS channel
instead of asking for external-AI approval. By default it also embeds Sirin-local
`script_health` evidence in the `service` category (`localTestHealthReady` /
`localTestHealth`) so the report still contains locally verifiable test/replay
health while OPS MCP integration is blocked. When the current embedded
`script_health` pass is healthy and produces zero candidates, stale
Sirin-local test-health inbox records are suppressed from that report's
`inboxEvents` / `nextActions`; the historical inbox rows are not deleted. It also
reads the local
`ops_event_inbox` and adds active, non-rejected records to each category under
`inboxEvents`; the top-level `opsEventInbox` field records the included count
and storage path. If an inbox record is `ACCEPTED` with `externalIssueUrl` /
`externalIssueNumber`, the report exposes it under top-level
`trackedExternalIssues`, keeps the external issue metadata on the category
`inboxEvents[]`, and emits a `tracked_external_issue` next action instead of a
fresh `ops_event_inbox` review action. Categories with only such events are
marked `tracked_external_issue`, or `blocked_external_issue_tracked` if local
checks are still blocked by the same external contract issue. Inbox records are
de-duplicated by `dedupeKey`; repeated MCP batch/auth blockers are collapsed to
the latest representative event so the next-action list remains reviewable.
Legacy `BATCH_PLAN_REQUIRED` / MCP auth records are routed to `service`
integration work instead of contaminating seller/order/growth/support
categories. Tool-level MCP failures also preserve
machine-readable JSON-RPC `error.data` under check/candidate `errorData`, so a
handoff can see fields such as `deniedTool`, `planTool`, `requiredNextAction`,
and `requestedTools` without scraping the text excerpt. When that error data
shows an AgoraMarketAPI MCP auth/session contract blocker such as
`BATCH_PLAN_REQUIRED`, Sirin routes the candidate to `targetProject:
AgoraMarketAPI` with `allowedAction: CODEX_VERIFY_INTEGRATION`; it should be
opened or updated as an external-project issue/handoff rather than a Sirin
backlog item or AgoraMarket product bug.
`issue_candidates` is the
review/handoff list with
`candidateType`, `severity`, `confidence`, `dedupeKey`, `affectedSurface`,
evidence excerpts, and suggested next actor (`CHATGPT`, `CODEX`, or `HUMAN`).
Each candidate also attaches an `opsEvent` gate: `eventType`, `targetProject`,
`evidenceLevel`, `allowedAction`, and `productIssueGate`. This is the
automation boundary for using Sirin as the AgoraMarket marketplace operations
AI: Sirin discovers and classifies operational events, then hands them to
ChatGPT/Codex/human review. It does not turn every signal into a product issue
automatically.

Typical gates:
- Read-only AgoraMarket ops MCP evidence can use
  `OPEN_PRODUCT_ISSUE_AFTER_REVIEW` after review verifies user/service impact.
- Growth signals use `HANDOFF_REVIEW_ONLY`; they are hypotheses, not product
  bugs.
- MCP/tool/auth failures use `CODEX_VERIFY_INTEGRATION` first and target Sirin
  integration/backlog work until marketplace evidence is collected.
- Human-sensitive fulfillment signals use `OPERATOR_REVIEW`.

Each candidate also includes a handoff packet with a ChatGPT review prompt, a
Codex task payload, and a GitHub issue draft. The issue draft includes the ops
event gate and remains a draft until review.

The A2A artifact list includes `agora_market_ops_report`,
`agora_market_issue_candidates`, and the full
`agora_market_issue_scout_result`. Local schedules store the same report under
`last_result.ops_report`.

`AGORA_MARKET_FIRST_ORDER_ACTIVATION` is the first-order operations playbook.
It reuses the read-only `AGORA_MARKET_ISSUE_SCOUT` evidence, then adds
`first_order_activation`: production order/GMV state, buyer-path/test-health
readiness, tracked conversion issues, top blockers, today's operator /
ChatGPT / Codex actions, and the verification rule for the first production
order. It does not place orders, send messages, mutate products, or modify
AgoraMarket/API state. Use it when the goal is not "find more issues" but
"produce the first non-test production order".

When this task includes a Codex action, Sirin also writes a local
`CODEX_REVIEW` item to `tracking/ops_action_ledger.jsonl`. This is the
Sirin-initiated review handoff: Codex can list, claim, update, and complete the
review through the `ops_review_*` tools below. The ledger is local Sirin state;
it does not call Codex by itself and does not mutate AgoraMarket/API.

`SIRIN_AGORA_TEST_HEALTH_SCOUT` is local-only. It reads Sirin's own
`script_health`, replay status, and recent test-run history for an AgoraMarket
suite such as `active-smoke`, then emits candidates for failed checks, replay
drift, stale fallback scripts, LLM-only tests, and untested checks. It does not
call AgoraMarketAPI and does not run browser tests by itself. These candidates
always use `allowedAction: SIRIN_BACKLOG_ONLY`; they may create Sirin
automation/handoff work, but they are not enough evidence to open AgoraMarket
product issues by themselves.

For failed-check candidates, the evidence includes the aggregate health numbers
plus `failed_tests`: concrete test ids, tags, replay status, latest run id,
status, duration, replay/LLM mode, screenshot path, failure category, and
token/cost fields when available. For routine runs, the local scout excludes
`obsolete`, `mutating`, `requires-operator-approval`, `interactive-media`,
`selector-risk`, `needs-manual-verification`, and `external-issue` tests by
default. Params may set `"includeObsolete": true` or target the `obsolete` /
`legacy` suite to inspect archived tests, and an explicit `excludeTags` list
fully overrides the default.

#### `a2a_issue_candidates`
List Sirin-local issue candidates produced by ops scouts such as
`AGORA_MARKET_ISSUE_SCOUT` and `SIRIN_AGORA_TEST_HEALTH_SCOUT`.
Sirin stores these in its local app data review inbox, so this does not require
new AgoraMarketAPI review/list APIs.
```json
{"name":"a2a_issue_candidates","arguments":{"status":"CANDIDATE","severity":"HIGH","limit":20}}
```

Optional filters:
- `task_id` — only candidates from one A2A task.
- `status` — `CANDIDATE`, `CONVERT_TO_ISSUE`, `NEEDS_MORE_EVIDENCE`,
  `NEEDS_MORE_SOURCES`, `REJECTED`, or `ACCEPTED`.
- `severity` — `HIGH`, `MEDIUM`, or `LOW`.
- `limit` — defaults to 20, max 100.

#### `ops_event_inbox`
Preferred AgoraMarket operations-AI inbox. This reads the same Sirin-local
records as `a2a_issue_candidates`, but returns them as ops events and supports
the gate fields Sirin uses for routing.
```json
{"name":"ops_event_inbox","arguments":{"allowedAction":"OPEN_PRODUCT_ISSUE_AFTER_REVIEW","eventType":"trust_support","limit":20}}
```

Optional filters:
- `task_id` — only events from one A2A task.
- `status` — review state such as `CANDIDATE`, `NEEDS_MORE_EVIDENCE`, or
  `CONVERT_TO_ISSUE`.
- `severity` — `HIGH`, `MEDIUM`, or `LOW`.
- `eventType` — `service_health`, `order_risk`, `trust_support`,
  `seller_fulfillment`, `growth_signal`, `test_coverage_gap`,
  `service_health_validation`, etc.
- `evidenceLevel` — `suspected`, `observed`, `needs_verification`,
  `hypothesis`, or `confirmed`.
- `allowedAction` — `SIRIN_BACKLOG_ONLY`, `CODEX_VERIFY_INTEGRATION`,
  `HANDOFF_REVIEW_ONLY`, `OPERATOR_REVIEW`, or
  `OPEN_PRODUCT_ISSUE_AFTER_REVIEW`.
- `targetProject` — usually `Sirin`, `AgoraMarket`, or `AgoraMarketAPI`.
- `limit` — defaults to 20, max 100.

Use this tool for routine Sirin operations review. Keep `a2a_issue_candidates`
for backward compatibility with older clients that still expect candidate
terminology.

#### `a2a_review_issue_candidate`
Review one Sirin-local issue candidate by `dedupeKey`. This updates Sirin's
local review metadata and returns the candidate handoff packet. It does not
create a GitHub issue, does not call AgoraMarketAPI review APIs, and does not
mutate orders, funds, wallets, trading, KB, or automation state.
```json
{"name":"a2a_review_issue_candidate","arguments":{"dedupeKey":"agora-market:orders:listpendingorders:ops_risk:...","reviewStatus":"CONVERT_TO_ISSUE","reviewNote":"Evidence is strong enough for Codex verification.","reviewedBy":"codex","externalIssueUrl":"https://github.com/Redandan/AgoraMarketAPI/issues/560","externalIssueProject":"AgoraMarketAPI","externalIssueNumber":560}}
```

Review is gated by `allowedAction`. `reviewStatus: CONVERT_TO_ISSUE` is only
accepted when the event has `allowedAction:
OPEN_PRODUCT_ISSUE_AFTER_REVIEW`. `SIRIN_BACKLOG_ONLY`,
`HANDOFF_REVIEW_ONLY`, `CODEX_VERIFY_INTEGRATION`, and `OPERATOR_REVIEW`
events may be marked `ACCEPTED`, `NEEDS_MORE_EVIDENCE`, or `REJECTED`, but they
cannot be converted into product issues until a later verification step creates
a new event with the product-issue gate open.

When a blocker has already been opened in another project, pass
`externalIssueUrl`, `externalIssueProject`, and optionally `externalIssueNumber`.
Sirin stores that metadata in the local candidate record and queued handoff so
future ops reviews can see that the event is already tracked externally.

The same local inbox is available in the web UI from the sidebar:
`運營 Scout`. The page calls these two Sirin MCP tools directly, shows
candidate evidence, lets the operator set `CONVERT_TO_ISSUE`,
`NEEDS_MORE_EVIDENCE`, `REJECTED`, or `ACCEPTED`, and exposes the handoff JSON
for ChatGPT/Codex. The UI does not require AgoraMarketAPI review/list support.

When a candidate is reviewed as `CONVERT_TO_ISSUE` or `ACCEPTED`, Sirin also
adds or updates an item in the local approved handoff queue. This queue is the
explicit boundary between "Sirin found and reviewed a problem" and "a human is
ready to hand work to ChatGPT/Codex".

#### `a2a_handoff_queue`
List approved handoff queue items. Items are stored in Sirin app data under
`tracking/a2a_handoff_queue.jsonl`.
```json
{"name":"a2a_handoff_queue","arguments":{"status":"READY","limit":50}}
```

`status` defaults to `READY`; pass `ALL` to include every local handoff state.
This is read-only and does not call ChatGPT, Codex, GitHub, or AgoraMarketAPI.

#### `a2a_handoff_approval_packets`
Generate read-only operator approval packets for `READY` handoffs. This is the
direct entry point for the "Sirin found it, operator approves, ChatGPT/Codex
reviews next" workflow.
```json
{"name":"a2a_handoff_approval_packets","arguments":{"limit":20}}
```

Each packet includes a copyable `a2a_update_handoff` `ACKED` template,
blocked mutations, the expected local `executionPackage` that will be created
after ACK, and the original handoff payload preview. This tool is read-only: it
does not ACK the handoff, call ChatGPT/Codex, create GitHub issues, or mutate
AgoraMarket/AgoraMarketAPI.

#### `a2a_request_handoff_approval`
Create or update a Sirin-local operator approval request for a `READY` handoff.
This is the persistent "Sirin is asking for approval" state used by long-running
ops cycles.
```json
{"name":"a2a_request_handoff_approval","arguments":{"handoffId":"handoff:ait-test-1:agora-market:test:tool:ops_risk:abc","requestedBy":"sirin","note":"Request approval for local review package only."}}
```

The request is stored under
`tracking/a2a_handoff_approval_requests.jsonl` and includes the same approval
packet returned by `a2a_handoff_approval_packets`. It does not ACK the handoff,
does not create an `executionPackage`, does not call ChatGPT/Codex, and does
not mutate GitHub, AgoraMarket, AgoraMarketAPI, orders, payments, customers, or
sellers.

#### `a2a_handoff_approval_requests`
List Sirin-local handoff approval requests. The default status filter is
`PENDING_OPERATOR_APPROVAL`; pass `ALL` to inspect every request.
```json
{"name":"a2a_handoff_approval_requests","arguments":{"status":"PENDING_OPERATOR_APPROVAL","limit":50}}
```

This is read-only. It lets Sirin distinguish "approval has not been requested"
from "approval is already pending" during repeated operations cycles.

#### `a2a_update_handoff_approval_request`
Update a Sirin-local handoff approval request after operator review.
```json
{"name":"a2a_update_handoff_approval_request","arguments":{"handoffId":"handoff:ait-test-1:agora-market:test:tool:ops_risk:abc","status":"APPROVED","reviewer":"operator-redan","decision":"APPROVE_LOCAL_ACK_ONLY","note":"Approve local ACK package only."}}
```

Allowed statuses are `PENDING_OPERATOR_APPROVAL`, `APPROVED`, `REJECTED`, and
`CANCELLED`. This only records the operator decision on the approval request.
It does not ACK the handoff and does not create an `executionPackage`. When the
request is `APPROVED`, Sirin may then use the embedded
`approvalPacket.toolCallTemplate` with `a2a_update_handoff` to ACK local handoff
metadata and create the local review package. External mutations remain blocked.

#### `a2a_handoff_review_inbox`
Read-only downstream review inbox for approved handoffs.
```json
{"name":"a2a_handoff_review_inbox","arguments":{"actor":"CHATGPT","limit":50}}
```

Items with `status: READY_FOR_REVIEW` already have a local
`executionPackage` and can be reviewed by the target actor within the package
boundary. Items with `status: APPROVAL_REQUIRED` are still `READY` handoffs and
include an `approvalPacket` with the safe ACK template; if
`a2a_request_handoff_approval` has already been called, they also include
`approvalRequestId` and `approvalRequestStatus`. Legacy `ACKED` rows
without execution packages appear as `LEGACY_ACK_MISSING_EXECUTION_PACKAGE` and
must not be treated as approved review packages unless structured
`operator_review` evidence exists. The tool does not ACK handoffs or call
ChatGPT/Codex/GitHub/AgoraMarket/AgoraMarketAPI.

#### `a2a_update_handoff`
Update a local approved handoff status after an operator copies or hands off
the payload.
```json
{"name":"a2a_update_handoff","arguments":{"handoffId":"handoff:ait-test-1:agora-market:test:tool:ops_risk:abc","status":"ACKED","note":"Copied to Codex"}}
```

Structured review metadata is supported and should be used for operator-gated
ops handoffs:
```json
{
  "name": "a2a_update_handoff",
  "arguments": {
    "handoffId": "handoff:ait-test-1:agora-market:test:tool:ops_risk:abc",
    "status": "ACKED",
    "note": "Operator reviewed payload for ChatGPT handoff",
    "reviewer": "operator-redan",
    "decision": "ACK_FOR_CHATGPT_REVIEW",
    "approvedBoundary": "Sirin-local metadata update only; no customer or seller message approved.",
    "approvedActions": ["sirin_local_metadata_update"],
    "blockedMutations": ["customer_message", "seller_message", "order_update", "payment_action"]
  }
}
```

When status is `ACKED`, Sirin writes an `executionPackage` into the handoff
record. The package is local review material for ChatGPT/Codex and includes the
target actor, allowed action `HANDOFF_REVIEW_ONLY`, approval evidence,
blocked mutations, the original handoff payload, and a boundary statement. It
does not execute the handoff.

Allowed statuses: `READY`, `ACKED`, `DONE`, and `CANCELLED`. This only updates
Sirin-local queue metadata and always records `mutationApproved=false`; it does
not call ChatGPT, Codex, GitHub, AgoraMarket, AgoraMarketAPI, orders, payments,
customers, or sellers.

#### `a2a_prepare_handoff_execution_packages`
Prepare local execution packages for legacy `ACKED` handoffs that already have
structured `operator_review` evidence but were acknowledged before
`executionPackage` existed.
```json
{"name":"a2a_prepare_handoff_execution_packages","arguments":{"dryRun":true,"limit":50}}
```

`dryRun` defaults to `true`. With `dryRun=false`, Sirin only backfills the local
`tracking/a2a_handoff_queue.jsonl` record. It does not call ChatGPT, Codex,
GitHub, AgoraMarket, AgoraMarketAPI, orders, payments, customers, or sellers.

#### `codex_supervisor_report` / `codex_supervisor_snapshot`

The Codex desktop heartbeat reports minimal recent-turn evidence to Sirin; the
supervisor classifies the task without persisting prompts, assistant messages,
titles, credentials, or command output. `latestUserTurnKey` and
`unfinishedScopeKey` must be opaque hashes/cursors, not natural-language text.

```json
{"name":"codex_supervisor_report","arguments":{"threadId":"01a...","hostId":"local","latestUserTurnKey":"turn-a1","unfinishedScopeKey":"scope-a1","latestTurnStatus":"INTERRUPTED","objectiveUnfinished":true,"contract":{"intent":"CHANGE","authorizedActions":["READ","EDIT","TEST"]},"coverage":[{"surface":"CODE","status":"PASS","required":true},{"surface":"WEB_UI","status":"MISSING_PROOF","required":true,"humanRequired":false}]}}
```

Read the local dashboard state with:

```json
{"name":"codex_supervisor_snapshot","arguments":{}}
```

Reports older than 15 minutes remain available to the local ledger for
retention and dedupe, but are excluded from the current snapshot. With no
fresh reports, the snapshot returns `WAITING_FOR_HEARTBEAT` instead of
presenting stale classifications as current evidence.

Every scan also reports through this same tool with the reserved
`threadId=sirin-supervisor-scan`. A successful empty scan uses
`latestTurnStatus=COMPLETED` and produces `HEALTHY_IDLE`; the scan record is
freshness metadata only, is hidden from the task list, and can never be claimed.
Failed scans use `FAILED` plus a stable `readErrorCode` and remain fail closed.

#### `codex_supervisor_claim` / `codex_supervisor_complete_action`

`codex_supervisor_claim` reserves at most one fresh, dedupe-safe
`RECOVERABLE_INTERRUPTION` for ten minutes and returns a scope-preserving
continuation prompt. It does not call Codex or grant approval.

```json
{"name":"codex_supervisor_claim","arguments":{}}
```

After the Codex heartbeat re-reads the target and attempts
`send_message_to_thread`, it records the result locally:

```json
{"name":"codex_supervisor_complete_action","arguments":{"claimId":"<claim id>","outcome":"ACCEPTED"}}
```

Allowed outcomes are `ACCEPTED`, `NOT_SENT`, `FAILED`, and `TARGET_RUNNING`.
Only `ACCEPTED` starts the six-hour fingerprint dedupe window. See
[`CODEX_WORK_SUPERVISOR.md`](CODEX_WORK_SUPERVISOR.md) for the classification,
coverage, privacy, and heartbeat rules.

#### `ops_review_queue_list`
List Sirin-local Codex review actions from `tracking/ops_action_ledger.jsonl`.
This is the queue Codex should poll when Sirin has initiated a review.
```json
{"name":"ops_review_queue_list","arguments":{"status":"PENDING_CODEX_REVIEW","actor":"CODEX","limit":20}}
```

Optional filters:
- `status` — defaults to `PENDING_CODEX_REVIEW`; pass `ALL` to include every
  ledger state.
- `actor` — usually `CODEX`.
- `targetProject` — for example `Sirin`, `AgoraMarket`, or `AgoraMarketAPI`.
- `limit` — defaults to 50, max 200.

#### `ops_review_claim`
Claim one Sirin-local Codex review action before working on it.
```json
{"name":"ops_review_claim","arguments":{"actionId":"OPS-ACTION-20260615-0001","claimedBy":"codex"}}
```

This changes only local ledger metadata to `CLAIMED`.

#### `ops_review_update`
Update review progress while Codex is checking evidence.
```json
{"name":"ops_review_update","arguments":{"actionId":"OPS-ACTION-20260615-0001","status":"IN_PROGRESS","note":"Checking buyer path and existing issue state."}}
```

Allowed progress states: `PENDING_CODEX_REVIEW`, `CLAIMED`, `IN_PROGRESS`,
`NEEDS_MORE_EVIDENCE`, and `BLOCKED`.

#### `ops_review_complete`
Write Codex's conclusion back to Sirin.
```json
{"name":"ops_review_complete","arguments":{"actionId":"OPS-ACTION-20260615-0001","status":"VERIFIED","result":{"is_real_issue":true,"evidence":"Buyer path is reachable but no production orders are present.","recommended_next_action":"Keep AgoraMarket issue #254 open and run operator CTA experiment.","issue_to_open_or_update":"https://github.com/Redandan/AgoraMarket/issues/254"}}}
```

Allowed completion states: `VERIFIED`, `DONE`, `REJECTED`, and
`NEEDS_MORE_EVIDENCE`. This does not create a GitHub issue by itself; Codex
must still use the normal issue workflow when the target project is not Sirin.

#### `ops_schedule_create`
Create a Sirin-local operations schedule entry. This is the operator-owned
task queue for using Sirin as an operations AI: schedules, priority, params,
and lifecycle state live in Sirin app data under `tracking/ops_schedule.jsonl`.
Local mutations serialize the complete read-modify-write transaction, so
concurrent creates or updates do not overwrite one another. It does not create
server tasks and does not modify AgoraMarketAPI.
```json
{"name":"ops_schedule_create","arguments":{"title":"Daily Agora issue scout","taskType":"AGORA_MARKET_ISSUE_SCOUT","cadence":"daily","priority":"P1","params":{"maxCandidates":12}}}
```

Local test-health scout example:
```json
{"name":"ops_schedule_create","arguments":{"title":"Daily Agora test health scout","taskType":"SIRIN_AGORA_TEST_HEALTH_SCOUT","cadence":"daily","priority":"P1","params":{"suite":"active-smoke","windowDays":30,"maxCandidates":12}}}
```

First production order activation example:
```json
{"name":"ops_schedule_create","arguments":{"title":"P0 First production order activation","taskType":"AGORA_MARKET_FIRST_ORDER_ACTIVATION","cadence":"daily","priority":"P0","params":{"goal":"FIRST_PRODUCTION_ORDER","successMetric":"production_orders >= 1 OR production_gmv > 0","maxCandidates":12,"includeTestHealth":true}}}
```

Fields:
- `title` — required operator-facing schedule name.
- `taskType` — defaults to `SIRIN_AGORA_TEST_HEALTH_SCOUT`; use
  `AGORA_MARKET_ISSUE_SCOUT` for external read-only ops MCP scouting, or
  `AGORA_MARKET_FIRST_ORDER_ACTIVATION` for a first-order conversion playbook.
- `params` — local task params, for example `maxCandidates`, `windowDays`,
  `includeLlmOnly`, `includeUntested`, `excludeTags`, `includeObsolete`,
  `days`, `hoursOverdue`, `includeCustomerSupport`, `includeSourcing`,
  `includeTestHealth`, `testHealthSuite`, `testHealthWindowDays`, or
  `maxTestHealthCandidates`.
- `cadence` — `manual`, `daily`, `weekly`, or `once`; first version stores the
  schedule semantics for operator review and later runner automation.
- `priority` — `P0`, `P1`, or `P2`; defaults to `P1`.
- `scheduledFor` — optional timestamp or operator-readable target time.

#### `ops_schedule_list`
List Sirin-local operations schedule entries.
```json
{"name":"ops_schedule_list","arguments":{"status":"SCHEDULED","limit":50}}
```

Optional filters: `status`, `taskType`, `priority`, and `limit`.
Each schedule may include `last_result_status`, which mirrors
`last_result.ops_report.reportStatus` so operators can distinguish a completed
local run from a marketplace report that is still waiting on approval or
integration repair.

#### `ops_schedule_digest`
Summarize recent Sirin-local operations schedule results without running a new
task. The digest aggregates each schedule's `last_result.ops_report.actionSummary`
so operators can see whether the recent window is blocked, waiting for issue
review, waiting for handoff review, or healthy monitoring.
```json
{"name":"ops_schedule_digest","arguments":{"taskType":"AGORA_MARKET_ISSUE_SCOUT","limit":10}}
```

Returned `digest` fields include recent run counts, total new issue candidates,
handoff-only candidates, tracked external issue appearances,
`uniqueTrackedExternalIssues`, pending inbox review count, healthy/blocked
checks, the latest schedule id/result status/action summary, and
`recommendedNextSchedule`. Historical blocked runs remain visible in
`blockedIntegrationRuns`, but the digest only reports
`attention_integration_blocked` when the latest summarized run is still blocked.
When a first-five / repeatable-order cycle is present,
`digest.operationsCycleReport.externalIssueStateSync` summarizes locally tracked
external blockers, reports whether their state is `open`, `closed`/`fixed`, or
still `unknown`, and exposes `dryRunDecision.canRerunCheckoutDryRun`. Sirin
should only rerun checkout dry-runs after this gate says it is ready, and the
dry-run boundary still requires stopping before payment confirmation.
The same cycle report also exposes `pendingOperatorApprovalRequests`,
`pendingOperatorApprovalSummary`, and per-handoff `approvalRequestStatus` /
`approvalRequestId` inside the review and handoff sections. Repeated operations
cycles can therefore distinguish "request approval" from "approval already
requested; wait for operator" instead of recreating the same handoff gate.
This is read-only and only reads Sirin app data.

#### `ops_external_issue_sync`
Synchronize GitHub issue state for external blockers referenced by Sirin-local
ops schedules. The tool reads tracked GitHub issue URLs from
`tracking/ops_schedule.jsonl`, calls GitHub's issue API read-only, and writes
the issue `state`, title, updated time, and sync timestamp back into the local
schedule snapshot. If GitHub REST is not authorized for a private repository,
Sirin falls back to the locally authenticated `gh issue view` CLI. It does not
close GitHub issues and does not mutate
AgoraMarket, AgoraMarketAPI, orders, payments, products, customers, or sellers.
Run this before relying on
`digest.operationsCycleReport.externalIssueStateSync.dryRunDecision`.
```json
{"name":"ops_external_issue_sync","arguments":{"taskType":"AGORA_MARKET_FIRST_ORDER_ACTIVATION","limit":50}}
```

#### `ops_schedule_update`
Update a local schedule entry status. This is used by the Web UI to pause,
resume, complete, or cancel local ops tasks.
```json
{"name":"ops_schedule_update","arguments":{"scheduleId":"OPS-20260613-0001","status":"PAUSED","note":"operator paused"}}
```

Allowed statuses: `SCHEDULED`, `RUNNING`, `PAUSED`, `DONE`, `CANCELLED`, and
`FAILED`. The update only changes Sirin-local metadata.

#### `ops_schedule_run_due`
Execute due Sirin-local operations schedule entries. The runner marks each due
`SCHEDULED` entry as `RUNNING` in an atomic claim transaction, invokes Sirin's
existing inspection capability without holding the schedule lock, then merges
the result into the latest local snapshot. A concurrent runner cannot claim the
same entry twice. Successful runs with
`issue_candidates` persist those candidates into the Sirin-local review inbox
used by `a2a_issue_candidates`.
```json
{"name":"ops_schedule_run_due","arguments":{"scheduleId":"OPS-20260613-0001","limit":1}}
```

`scheduleId` is optional; omit it to run the next due local schedule. `limit`
defaults to 1 and is capped at 10. `daily` and `weekly` schedules are
rescheduled after successful runs; `manual` and `once` schedules become `DONE`.
Failures become `FAILED` with `last_error`. A schedule status of `DONE` means
the local run completed, not that marketplace evidence was necessarily ready;
check `last_result_status` / `last_result.ops_report.reportStatus` for
`READY_FOR_REVIEW` or `BLOCKED_INTEGRATION`.
The runner remains read-only toward AgoraMarketAPI: it may call existing
read-only inspection/MCP tools but does not mutate orders, products, funds,
wallets, trading, Telegram, or GitHub.
After due schedules finish, `ops_schedule_run_due` also performs a best-effort
`ops_external_issue_sync` for the task types it just ran. Sync failures are
reported under top-level `externalIssueSync` and do not make the schedule run
fail; the sync only updates Sirin-local snapshots used by
`externalIssueStateSync`.

Sirin also starts a conservative local schedule loop with the daemon. It checks
once per minute and runs at most one due `SCHEDULED` entry per tick. Paused,
done, cancelled, failed, or future-dated entries are left untouched.

---

### Multi-Agent Squad (1 tool)

#### `squad_knowledge`
List lessons the PM has accumulated across task runs (persisted in SQLite).
Useful for inspecting what the squad has learned, or auditing before a new project.
```json
{"name":"squad_knowledge","arguments":{"limit":20}}
```
**Returns:**
```json
{
  "total": 42,
  "showing": 20,
  "lessons": [
    {"key": "flutter_headless...", "lesson": "Flutter CanvasKit requires browser_headless: false", "learned_at": "2026-04-24T10:00:00+08:00"}
  ]
}
```
`limit` defaults to 20. Ordered newest first.

---

### Test Runner (14 tools)

#### `list_tests`
Enumerate test YAMLs in `config/tests/`. Defaults to all tests; pass
`suite`, `tags` / `include_tags`, or `exclude_tags` to narrow the list before
queueing work.
```json
{"name":"list_tests","arguments":{"suite":"active-smoke"}}
```
**Returns:** `{count, suite, include_tags, exclude_tags, tests: [{id, name, url, goal, tags, max_iterations, timeout_secs, docs_refs}]}`

#### `run_test_async`
Fire-and-forget run of a YAML-defined test. Returns run_id immediately.
```json
{"name":"run_test_async","arguments":{
  "test_id":"wiki_smoke",
  "auto_fix": false
}}
```
**Returns:** `{run_id, test_id, auto_fix, status:"queued", poll_with:"get_test_result"}`

If `auto_fix: true`, failed runs trigger a `claude_session` spawn + verification
re-run (see the Failure Category table below).

#### `run_test_batch`
Fan-out N YAML tests in parallel onto independent chrome tabs (one
`session_id` per test). Returns N `run_id`s immediately; poll each separately.
```json
{"name":"run_test_batch","arguments":{
  "test_ids":["wiki_smoke","login_flow","checkout_smoke"],
  "max_concurrency": 3
}}
```
**Returns:** `{count, max_concurrency, runs:[{test_id, run_id}], status:"queued",
poll_each_with:"get_test_result"}`.

**`max_concurrency`:** clamped to `[1, 8]` (CDP isn't designed for hundreds of
simultaneous tabs). Default `3`. Tests beyond the cap queue on a Semaphore and
start as permits free.

**Restrictions:**
- Any unknown `test_id` aborts the entire batch (fail-fast).
- Each run gets `session_id = batch_<batch_id>_<idx>`; tabs are auto-closed when
  the run completes (success or failure).
- **No auto-fix in batch mode** — re-run individual failures with
  `run_test_async + auto_fix=true` if you want triage.
- Triage / persistence to SQLite still happen per run.

Use this for nightly / smoke suites; use `run_test_async` for one-off triage.

#### `run_adhoc_test`
Test any URL without pre-creating a YAML goal. The most important tool for
external Claude Code sessions that receive requests against arbitrary URLs.
```json
{"name":"run_adhoc_test","arguments":{
  "url":"https://example.com",
  "goal":"Verify the home page loads with the expected title.",
  "success_criteria":["Page contains 'Example Domain'"],
  "locale":"en",
  "max_iterations":10,
  "timeout_secs":90,
  "browser_headless":false,
  "llm_backend":"claude_cli",
  "fixture":{
    "setup":[
      {"action":"goto","target":"https://example.com/login"},
      {"action":"click","target":"#guest-btn"},
      {"action":"wait","target":".dashboard"}
    ],
    "cleanup":[
      {"action":"clear_state"}
    ]
  }
}}
```
**`browser_headless` (optional):** `false` required for Flutter CanvasKit /
WebGL targets (they won't paint in headless Chrome → screenshots come back
black). Default reads `SIRIN_BROWSER_HEADLESS` env (itself defaulting to
`true`).

**`llm_backend` (optional):** Switches the ReAct LLM driver for this run.
- `"claude_cli"` / `"claude"` — spawn `claude -p` subprocess (Max plan, no
  API key, much higher JSON-output reliability, ~3-5s per call overhead).
- `null` / omitted / any other value — use Sirin's main LLM config
  (Gemini / LM Studio / Ollama / Anthropic HTTP API).

⚠ **Known issue (2026-04-20):** `claude_cli` hangs on iteration-2+
ReAct prompts that include screenshot history (10-20KB), hitting the
600s subprocess watchdog. Use the default Gemini backend until the
follow-up "Investigate claude_cli ReAct hang" task lands a fix. The
dispatcher itself is correct — both the YAML field and this MCP arg
are wired through `resolve_llm_backend()` in `executor.rs`.

Resolution order: this arg → `TEST_RUNNER_LLM_BACKEND` env → main LLM config.

**`fixture` (optional):** `setup` steps run before the ReAct loop; `cleanup`
steps run unconditionally after the loop (even on timeout/error). Each step
supports `action`, `target`, `text`, and `timeout_ms` fields. A setup failure
aborts the run immediately with `error` status; cleanup errors are logged and
ignored.

Synthetic test_id format: `adhoc_<YYYYMMDD_HHMMSS_mmm>`. Results persist to
`test_runs` table with tag `adhoc`. Adhoc runs **skip** auto-fix verification
(no YAML to re-run).

#### `persist_adhoc_run`
Promote a successful ad-hoc exploration into a permanent regression test by
writing `config/tests/<test_id>.yaml`. Closes the explore → save → re-run loop:
the AI explores against an unknown URL, locks in what worked, and the result is
a YAML test that any future `run_test_async` call can re-execute as regression.
```json
{"name":"persist_adhoc_run","arguments":{
  "run_id":"run_20260418_143015_872_0",
  "test_id":"login_flow",
  "name":"Login flow regression",
  "tags":["smoke","auth"],
  "bump_iterations":true,
  "overwrite":false
}}
```
**Returns:** `{test_id, yaml_path, iterations_used, criteria_count, tags}`.

**Validation (returns Err on violation):**
- `test_id` must match `[a-z0-9_]+` — no uppercase, hyphens, dots
- `test_id` must NOT start with `adhoc_` (reserved for in-flight runs)
- `run_id` must be recoverable. **Two-tier lookup:**
  1. In-memory registry (fast, lives 1 hour after run completes)
  2. SQLite `test_runs` table (persistent — works for runs older than 1 hour
     as long as the goal was recorded; ad-hoc runs since v0.4.0 store
     `goal_json` for exactly this reason)
- run must be `Complete(passed)` — refuses queued / running / failed / error
- file must not exist unless `overwrite=true`

**Field carry-over from the original ad-hoc TestGoal:**
- `url`, `goal`, `success_criteria`, `locale`, `url_query`, `browser_headless`,
  `fixture`, `timeout_secs`, `retry_on_parse_error` — verbatim
- `name` — caller-supplied or auto-stripped of `Ad-hoc: ` prefix
- `tags` — caller-supplied OR original tags with `adhoc` removed and
  `adhoc-derived` added (so `list_tests` filters can distinguish)
- `max_iterations` — when `bump_iterations=true` (default), bumped to
  `max(iterations_used + 5, original)` so regression has slack for variance

After persist, subsequent regression runs use `run_test_async` with the new
`test_id` — the synthetic ad-hoc namespace stays clean.

#### `get_test_result`
Poll a run by id.
```json
{"name":"get_test_result","arguments":{"run_id":"run_..."}}
```
**Returns:** `{run_id, test_id, started_at, status, details}`

Status values:
- `queued` — spawned but not yet running
- `running` — details include `{step, current_action}`
- `passed` / `failed` / `timeout` / `error` — terminal states

For terminal states, `details` contains:
`{iterations, duration_ms, error, analysis, steps, has_screenshot, screenshot_error}`

#### `get_screenshot`
Fetch failure screenshot as base64 PNG.
```json
{"name":"get_screenshot","arguments":{"run_id":"run_..."}}
```
**Returns:** either `{mime, bytes_base64, size_bytes, url}` OR
`{bytes_base64: null, screenshot_error: "<reason>"}`.

#### `get_full_observation`
Retrieve full (un-truncated) tool output for a specific step. Useful when the
visible history shows `[truncated: ...]`.
```json
{"name":"get_full_observation","arguments":{
  "run_id":"run_...",
  "step": 3
}}
```
**Returns:** `{run_id, step, content, char_count}`

#### `list_recent_runs`
Query SQLite `test_runs` table. Omit `test_id` for cross-test view.
```json
{"name":"list_recent_runs","arguments":{"test_id":"login_smoke","limit":10}}
```
**Returns:** `{count, runs: [{id, test_id, started_at, duration_ms, status,
failure_category, ai_analysis, screenshot_path}]}`

#### `list_fixes`
Query auto-fix history.
```json
{"name":"list_fixes","arguments":{"test_id":"login_smoke"}}
```
**Returns:** `{count, fixes: [{id, test_id, run_id, category, triggered_at,
completed_at, outcome, claude_exit_code, claude_output, verification_run_id,
verified_at}]}`

Outcome values:
| Outcome | Meaning |
|---------|---------|
| `pending` | Spawned, claude_session in flight |
| `fix_attempted` | Claude returned exit=0, verification in progress |
| `verified` | Re-run passed after fix |
| `regressed` | Re-run still failed after fix → escalate |
| `failed` | claude_session exited non-zero (no verification attempted) |
| `skipped_dedupe` | Another fix was in flight, or 3 consecutive failures |

#### `config_diagnostics`
Self-check Sirin's LLM / router / vision / Chrome / Claude CLI.
```json
{"name":"config_diagnostics","arguments":{}}
```
**Returns:** `{count, errors, warnings, ok, issues: [{severity, category,
message, suggestion}], text_report}`

Use when tests are mysteriously failing across the board.

#### `diagnose`
Self-diagnostic snapshot for the **two-tier diagnostic workflow**: an external
AI (Tier 1) hits a bug, calls `diagnose`, and gets a single rich blob with
Sirin's version, build, host, browser, LLM and recent error log. It can then
decide:
- retry (transient)
- tell the user "you're on 0.3.0 but 0.3.2 fixes this — please update"
- file an issue using `report_issue_template.body` (already populated with
  the env block, so the user / Tier 2 dev gets reproduction context for free)

```json
{"name":"diagnose","arguments":{}}
```
**Returns:**
```json
{
  "identity": {
    "name": "sirin", "version": "0.3.2", "git_commit": "37cfaf5",
    "git_dirty": true, "build_identity": "37cfaf5-dirty",
    "build_date": "2026-04-18T02:59:04Z",
    "binary_path": "C:\\...\\sirin.exe",
    "platform": "windows-x86_64",
    "uptime_secs": 13, "rpc_port": 7711
  },
  "chrome": { "running": false, "reason": "no browser session open" },
  "llm":    { "provider": "gemini", "model": "models/gemini-2.5-flash", "vision_capable_hint": true },
  "update": { "state": "up_to_date", "current": "0.3.2", "latest": null,
              "release_notes_url": "https://github.com/Redandan/Sirin/releases" },
  "recent_errors": ["..."],
  "report_issue_template": {
    "title_hint": "[bug] <one-line summary>",
    "body":       "## Environment\n- Sirin version : 0.3.2 ...\n## Reproduction\n<! ... !>\n",
    "github_url": "https://github.com/Redandan/Sirin/issues/new"
  }
}
```
`llm.provider`, `llm.model`, and `llm.vision_capable_hint` come from the
process-wide effective runtime config after startup probing and `llm.yaml`
overrides. They intentionally use the same source as `config_diagnostics` and
the actual LLM caller rather than reconstructing identity from raw env vars.

Cost: ~5–20 ms (one CDP `Browser.getVersion` + log tail). Safe to call on
every error in the caller — Sirin doesn't cache, but cost is dominated by
one round-trip to Chrome.

#### `page_state`
Single-call page snapshot: URL + title + condensed AX tree summary + last 5
console messages + JPEG thumbnail. Use instead of four separate `browser_exec`
calls when you need situational awareness without a specific assertion.
```json
{"name":"page_state","arguments":{}}
```
**Returns:**
```json
{
  "url":         "https://app.example.com/dashboard",
  "title":       "Dashboard – Acme",
  "ax_summary":  "button:Logout, text:Welcome Alice, button:Settings ...",
  "screenshot_b64": "<JPEG thumbnail — null if browser not open>",
  "console_recent": ["[warn] Slow network request", "..."]
}
```
Use for quick orientation ("what page are we on?") before deeper exploration.
`ax_summary` is a compact one-liner; for full tree use `browser_exec(ax_tree)`.

#### Android Emulator geo tools
Sirin can control Android Emulator GPS through the local Android SDK/ADB.
These tools are for emulator movement simulation, not browser viewport
emulation. By default they only allow `emulator-*` devices; physical devices
require explicit `allowPhysicalDevice:true`.

End-to-end operator flow:
```json
{"name":"device_emulator_start","arguments":{
  "avd":"Sirin_Pixel_7_API_35",
  "timeoutSeconds":180,
  "noWindow":true
}}
```

```json
{"name":"device_app_launch","arguments":{
  "device":"emulator-5554",
  "package":"com.example.app"
}}
```

Capture the current App/emulator screen as evidence:
```json
{"name":"device_screenshot","arguments":{
  "device":"emulator-5554",
  "label":"before_roam"
}}
```

```json
{"name":"device_operator_login_start","arguments":{
  "device":"emulator-5554",
  "package":"com.example.app",
  "timeoutSeconds":300,
  "note":"Operator logs in manually in the emulator."
}}
```

After the user finishes login:
```json
{"name":"device_operator_login_confirm","arguments":{
  "loginId":"LOGIN-...",
  "note":"User login completed."
}}
```

Start a non-repeating city roam route for 10 minutes at 20 km/h:
```json
{"name":"device_geo_roam_start","arguments":{
  "device":"emulator-5554",
  "city":"taipei",
  "radiusMeters":1500,
  "roadProvider":"osrm",
  "speedKmh":20,
  "durationSeconds":600,
  "tickSeconds":1,
  "screenMonitor":true,
  "screenSampleEveryTicks":5
}}
```

List SDK/devices/AVDs:
```json
{"name":"device_list","arguments":{}}
```

Single location fix:
```json
{"name":"device_geo_fix","arguments":{
  "device":"emulator-5554",
  "lat":25.033964,
  "lng":121.564468
}}
```

Plan a 20 km/h, 10-minute route without executing ADB:
```json
{"name":"device_geo_route_plan","arguments":{
  "route":{
    "start":{"lat":25.033964,"lng":121.564468},
    "end":{"lat":25.064000,"lng":121.564468}
  },
  "speedKmh":20,
  "durationSeconds":600,
  "tickSeconds":1
}}
```

Start continuous GPS injection:
```json
{"name":"device_geo_route_start","arguments":{
  "device":"emulator-5554",
  "route":{
    "start":{"lat":25.033964,"lng":121.564468},
    "end":{"lat":25.064000,"lng":121.564468}
  },
  "speedKmh":20,
  "durationSeconds":600,
  "tickSeconds":1,
  "screenMonitor":true,
  "screenSampleEveryTicks":5,
  "screenRequireChange":false
}}
```

Monitor or stop:
```json
{"name":"device_geo_route_status","arguments":{"routeId":"GEO-..."}}
{"name":"device_geo_route_stop","arguments":{"routeId":"GEO-..."}}
```

Implementation note: Sirin uses `adb -s <device> emu geo fix <lng> <lat>`.
At `20 km/h`, each one-second tick advances about `5.56 m`; a 600-second run
simulates about `3.33 km`.
`device_geo_roam_plan` / `device_geo_roam_start` are road-based by default.
They first generate non-repeating seed waypoints around the requested
city/center, then call an OSRM-compatible road router and use the returned
GeoJSON road geometry as the route that gets interpolated into per-second GPS
fixes. Default router: `https://router.project-osrm.org`; override with
`roadRouterUrl` or `SIRIN_OSRM_BASE_URL`. If road routing fails, Sirin returns
an error by default. Use `roadProvider:"synthetic"` only for offline tests, or
`allowSyntheticFallback:true` when a non-road fallback is explicitly acceptable.
Built-in city centers: `taipei`, `new_taipei`, `taichung`, `kaohsiung`, and
`chiang_mai`;
pass `center:{lat,lng}` for other cities.

Screen evidence: `device_screenshot` writes PNG files under Sirin app data
`device_screenshots/` and returns `path`, `size_bytes`, and `sha256`.
`device_geo_route_start` / `device_geo_roam_start` accept
`screenMonitor:true` to capture screen evidence during movement. Route status
then includes `screen_samples`, `last_screenshot_path`,
`last_screenshot_sha256`, `last_screen_changed`, `screen_stale_ticks`, and the
latest `screen_evidence` samples. Set `screenRequireChange:true` when a route
should fail if consecutive screenshot hashes do not change for
`maxStaleScreenTicks` samples. The first version checks image-hash movement
evidence; semantic validation that a map marker stays on a visible road should
be layered on top of these screenshots.

#### Physical iPhone USB acceptance tools

Sirin is the only public control plane for the USB iPhone. Keep device,
information, capture and control evidence separate. The Driver uses passive
attach by default: status reads do not start tunnel, DDI, WDA, or forwards.
For read-only diagnosis, use:

```json
{"name":"ios_device_status","arguments":{}}
```

For one fixed-route acceptance, prefer one composite call. It performs fresh
safety checks, starts the link on demand, records action evidence, and releases
the lease plus Sirin-owned temporary WDA/forward/tunnel processes even when the
route action fails. Release is successful only after process exit and inactive
forward listeners are proven; an unproven cleanup blocks the next lease. The
private Driver remains passively alive. Reuse the same `run_id` only if the
reply is ambiguous and keep it bound to the same route. This cache is scoped to
the current Sirin process; a restart remains `MISSING_PROOF`, not permission to
invent a new retry ID:

```json
{"name":"ios_acceptance_run","arguments":{
  "run_id":"codex-remote-acceptance-20260827-1",
  "route":"codex_remote",
  "owner":"codex-acceptance"
}}
```

Capture and actions retain their normal approval gate. For multi-action flows,
acquire one lease before opening an app, returning Home or swiping, and reuse
the same `action_id` if a response is ambiguous:

```json
{"name":"ios_control_session_start","arguments":{
  "owner":"codex-acceptance",
  "policy":"acceptance_browse_only",
  "ttl_secs":300
}}
{"name":"ios_open_route","arguments":{
  "session_id":"IOSSESSION-...",
  "action_id":"codex-remote-open-1",
  "route":"codex_remote",
  "settle_ms":8000
}}
{"name":"ios_control_session_stop","arguments":{
  "session_id":"IOSSESSION-..."
}}
```

The fixed route values are `safari_store`, `telegram_bot`, and
`codex_remote`. The last value maps only to
`https://remote.purrtechllc.com/`; arbitrary URLs remain rejected. Use the
distributed `iphone-usb-validation` skill and its target-specific reference for
evidence classification. Opening the Safari route does not by itself prove
installed Home Screen PWA lifecycle, video progress or Windows-applied input.
If LAN listener state is unknown, exposed, or the required pre-existing inbound
block is absent, link activation fails closed. Neither the MCP tools nor the
Driver create firewall rules or change trust, signing, unlock, or network
settings. Acceptance also never mounts a missing Developer Disk Image because
the first mount after an iOS update may contact Apple TSS and changes device
state; it returns `NEEDS_HUMAN_DDI` for explicit operator preparation instead.

`supervised_control` and `ios_tap` remain separately gated by the Sirin host,
the Driver mode, a fresh `expected_frame_sha256`, and explicit task need. Do not
use these tools for passcodes, trust prompts, credentials, messaging, orders or
payment.

#### iOS developer location tools
Sirin can drive iOS developer location simulation without 3uTools by wrapping a
local CLI backend such as `pymobiledevice3` or libimobiledevice's
`idevicesetlocation`. This requires an owned/test iPhone connected over USB,
trusted by the computer, and any developer image / Developer Mode requirements
for the iOS version. Sirin redacts device identifiers in status/evidence and
does not store Apple ID/passwords.

Check local backend/device readiness:
```json
{"name":"ios_dev_location_status","arguments":{}}
```

Set a single simulated location:
```json
{"name":"ios_dev_location_set","arguments":{
  "backend":"auto",
  "lat":25.033964,
  "lng":121.564468,
  "targetApp":"AgoraMarket iOS",
  "note":"Sirin developer-location smoke."
}}
```

Reset/clear simulated location:
```json
{"name":"ios_dev_location_reset","arguments":{
  "backend":"auto",
  "note":"Testing done; reset developer simulated location."
}}
```

Query sessions:
```json
{"name":"ios_dev_location_session_status","arguments":{"sessionId":"IOSDEVLOC-..."}}
```

Probe whether Sirin can directly read the iPhone's current GPS coordinate:
```json
{"name":"ios_dev_location_current","arguments":{
  "note":"Use this before current-city roam."
}}
```

Expected current result on stock iOS developer/lockdown services is usually
`UNAVAILABLE_REQUIRES_APP_REPORT`: Sirin can detect the USB iPhone and can set
or clear developer simulated location, but the exposed services do not provide
the device's current GPS coordinate. In that case, report location from a
trusted test app/web page using this shape, then pass it to
`ios_dev_location_roam_plan` as `center` or `currentLocation`:
```json
{
  "lat": 18.788343,
  "lng": 98.985300,
  "accuracyMeters": 20,
  "source": "ios_app_or_web_geolocation",
  "reportedAt": "2026-07-08T15:00:00Z"
}
```

Plan current-city road roaming from the current/known location:
```json
{"name":"ios_dev_location_roam_plan","arguments":{
  "center":{"lat":18.788343,"lng":98.985300},
  "city":"chiang_mai",
  "radiusMeters":1500,
  "waypoints":8,
  "roadProvider":"osrm",
  "speedKmh":20,
  "durationSeconds":600,
  "tickSeconds":1
}}
```

Plan a road-following route before a real iPhone is connected:
```json
{"name":"ios_dev_location_route_plan","arguments":{
  "routeSource":"manual_road_polyline",
  "points":[
    {"lat":25.033964,"lng":121.564468},
    {"lat":25.043964,"lng":121.564468},
    {"lat":25.053964,"lng":121.574468},
    {"lat":25.064000,"lng":121.604468}
  ],
  "speedKmh":20,
  "durationSeconds":600,
  "tickSeconds":1
}}
```

Query route plans:
```json
{"name":"ios_dev_location_route_status","arguments":{"routeId":"IOSROUTE-..."}}
```

Start GPX playback for a planned route on a connected iPhone:
```json
{"name":"ios_dev_location_route_start","arguments":{
  "routeId":"IOSROUTE-...",
  "userspace":true
}}
```

Backend command shape:
- `pymobiledevice3 developer dvt simulate-location set --userspace -- <lat> <lng>`
- `pymobiledevice3 developer dvt simulate-location clear --userspace`
- `pymobiledevice3 developer dvt simulate-location play --userspace <route.gpx>`
- `idevicesetlocation -- <lat> <lng>`
- `idevicesetlocation reset`

Sirin's iOS route simulation uses `pymobiledevice3` GPX playback when available;
single-point set/reset remains available for smoke tests and cleanup. Route
planning is available without a device. The preferred iOS flow is
`ios_dev_location_roam_plan`: pass `center` / `currentLocation` or a supported
`city`, and Sirin generates non-repeating seed waypoints, calls an
OSRM-compatible road router, then samples the returned street geometry. Manual
`points` / `roadPolyline` remains available for app-screen road tracing,
Apple/Google map data, or a manually verified road path. Sirin intentionally
rejects bare start/end route inputs for iOS route planning because that would
create a straight-line path instead of road-following movement. At `20 km/h`, a
600-second plan covers about `3.33 km`; with `tickSeconds:1`, Sirin generates
601 location points.
Actual playback still requires an owned/test iPhone connected over USB, trusted
by the computer, and a working developer-location backend. Call
`ios_dev_location_reset` after verification so the phone returns to true
location.

Deprecated legacy 3uTools GUI PoC tools are retained for compatibility only:
```json
{"name":"ios_3utools_status","arguments":{}}
{"name":"ios_3utools_start","arguments":{}}
```

#### `browser_exec`
Imperative single-action browser control. Bypasses the full test goal flow.
```json
{"name":"browser_exec","arguments":{
  "action":"click",
  "target":"#submit-btn",
  "browser_headless":false
}}
```

**`browser_headless` (optional on every call):** overrides default on bind.
First call that sets this causes Sirin to launch (or relaunch) Chrome in
that mode. Subsequent calls without the flag reuse the current mode.
Changing the value between calls triggers a clean re-launch.

Supported `action` values:
| Action | Required args | Returns |
|--------|---------------|---------|
| `goto` | `target` (URL) | `{status, url}` |
| `go_back` | — | `{status, url}` — Chrome history back (closes #28); equivalent to `window.history.back()` then waits for navigation settle |
| `screenshot` | — | `{mime, bytes_base64, size_bytes, url}` |
| **`screenshot_analyze`** | `target` (analysis prompt) | `{analysis, prompt}` — Gemini Vision reads the current page.  Concurrent calls in batch mode are throttled by `GEMINI_CONCURRENCY` (default 3, see `ENV_REFERENCE.md`); empty Gemini responses auto-retry 2× with 2s/4s backoff |
| `click` | `target` (selector) | `{status, selector}` |
| `type` | `target` (selector), `text` | `{status, selector, length}` |
| `read` | `target` (selector) | `{selector, text}` |
| `eval` | `target` (JS expr) | `{result}` |
| `wait` | `target` (selector), `timeout` (ms) | `{status, selector}` |
| `exists` | `target` (selector) | `{selector, exists}` |
| `attr` | `target` (selector), `text` (attr name) | `{selector, attribute, value}` |
| `scroll` | `timeout` (y pixels, default 300) | `{status, y}` |
| `key` | `target` (key name) | `{status, key}` |
| `console` | `timeout` (limit) | `{messages}` |
| `network` | `timeout` (limit) | `{requests}` |
| `url` | — | `{url}` |
| `title` | — | `{title}` |
| `close` | — | `{status}` |

**Accessibility tree actions** (literal text, no vision approximation —
required for K14/K15-style exact comparisons of $7376.80, error
messages, token counts):

| Action | Required args | Returns |
|--------|---------------|---------|
| `enable_a11y` | — | `{status}` — call before `ax_tree` on Flutter Canvas apps |
| `ax_tree` | — (optional `include_ignored`) | `{count, nodes:[{node_id, backend_id, role, name, value, description, child_ids}]}` |
| `ax_find` | `role` and/or `name` (substring, case-insensitive); optional `name_regex` (Rust regex, no implicit anchoring), `not_name_matches` (array of substrings to exclude), `limit` (int, default 1); scroll params: `scroll` (bool, default false), `scroll_max` (int, default 10) | `{found, count, nodes:[...], scrolled_times?}` — always an array; `scroll:true` scrolls the page up to `scroll_max` times when not found immediately |
| `ax_snapshot` | optional `id` (string key; auto-generated if omitted) | `{snapshot_id, count}` — stores current AX tree in memory keyed by `snapshot_id` |
| `ax_diff` | `before_id`, `after_id` | `{added:[{...}], removed:[{...}], changed:[{node_id, before_name, after_name}]}` — machine-readable delta between two snapshots |
| `wait_for_ax_change` | `baseline_id`; optional `timeout_ms` (default 5000) | `{changed:true, diff:{added,removed,changed}}` — blocks until tree differs from baseline; error on timeout |
| `ax_value` | `backend_id` | `{backend_id, text}` — literal `value \|\| name` |
| `ax_click` | `backend_id` | `{status, backend_id}` — clicks element centre via `DOM.getBoxModel` |
| `ax_focus` | `backend_id` | `{status, backend_id}` |
| `ax_type` | `backend_id`, `text` | `{status, backend_id, length}` — focus + insertText |
| `ax_type_verified` | `backend_id`, `text` | `{backend_id, typed, actual, matched}` — types, waits 300ms, reads back via a11y; `matched = actual.contains(typed)` |

**Robustness actions** (test isolation, race-free assertions, popup tabs):

| Action | Required args | Returns |
|--------|---------------|---------|
| `clear_state` | — | `{status:"cleared"}` — wipes cookies + localStorage + sessionStorage + IndexedDB + caches; doesn't close Chrome |
| `wait_request` | `target` (URL substring), optional `timeout` (ms, default 10000) | `{request: {url, method, status, req_body, body, ts}}` — auto-installs network capture; eliminates click-then-read race |
| `wait_new_tab` | optional `timeout` (ms, default 10000) | `{status, active_tab}` — polls + auto-discovers via register_missing_tabs; switches active to newest |

**Condition wait actions** (block until state is reached — no manual sleep polling):

| Action | Required args | Returns |
|--------|---------------|---------|
| `wait_for_url` | `target` (substring or `/regex/`), optional `timeout_ms` (default 10000) | `{matched, url, elapsed_ms}` — errors on timeout |
| `wait_for_ax_ready` | optional `min_nodes` (default 20), `timeout_ms` (default 10000) | `{node_count, elapsed_ms}` — polls AX tree every 200ms; errors on timeout |
| `wait_for_network_idle` | optional `idle_ms` (stable window, default 500), `timeout_ms` (default 15000) | `{elapsed_ms, request_count}` — stable when capture count unchanged for `idle_ms`; errors on timeout |

**Assertion actions** (return `{passed:true}` or MCP error on failure):

| Action | Required args | Returns |
|--------|---------------|---------|
| `assert_ax_contains` | `role` and/or `name` | `{passed, found, node}` — errors with details if no matching AX node |
| `assert_url_matches` | `target` (substring or `/regex/`) | `{passed, url}` — errors if current URL doesn't match |

**Multi-session actions** (named Chrome tabs):

| Action | Required args | Returns |
|--------|---------------|---------|
| `list_sessions` | — | `{sessions: [{session_id, tab_index, url}]}` |
| `close_session` | `target` (session_id) | `{status, closed_session_id}` — adjusts other sessions' indices |

**`session_id` param (optional on ALL browser_exec actions):** routes the
call to a named Chrome tab.  First use of a new `session_id` opens a fresh
tab.  Omitting `session_id` uses the default tab (index 0).

**Flutter CanvasKit / Shadow DOM actions** (for WebGL canvas apps — all UI
rendered in `<canvas>`, standard DOM selectors don't work):

| Action | Required args | Returns |
|--------|---------------|---------|
| `enable_a11y` | — | `{status}` — triggers Flutter to build `flt-semantics-host` overlay; call before any shadow_* |
| `shadow_dump` | — | `{count, elements:[{role,label}]}` — list all accessible elements in `flt-semantics-host` |
| `shadow_find` | `role` and/or `name_regex` | `{found, x, y, label}` — find element in shadow DOM.  Internal retry: 5×600ms when `flt-semantics-host` is empty; on continued failure (119efdc) auto-calls `enable_flutter_semantics()` + 800ms wait + one final attempt — manual `enable_a11y` preamble is no longer required for fresh SPA routes |
| `shadow_click` | `role` and/or `name_regex` | `{status, label}` — click via JS PointerEvent (NOT CDP Input, which causes about:blank); inherits shadow_find auto-bootstrap |
| `shadow_type` | `role`, `name_regex`, `text` | `{status}` — shadow_click + CDP InsertText (non-Flutter DOM) |
| `shadow_type_flutter` | `role`, `name_regex`, `text` | `{status}` — shadow_click + flutter_type; preferred for Flutter textboxes |
| `flutter_type` | `text` | `{status, text}` — fires CDP keydown per char; **ASCII only** (CJK chars have no keycode, silently fail) |
| `flutter_enter` | — | `{status, result}` — dispatches Enter keydown to `flt-text-editing`; use after flutter_type to submit chat/form |

**Flutter CanvasKit pattern:**
```
shadow_click (buttons/tabs)  ← auto-bootstraps Flutter semantics on host empty (119efdc)
→ ax_find role=textbox (get backend_id) → ax_click → flutter_type → flutter_enter
After route change: wait ≥1000ms → continue (shadow_* will auto-recover)
```

(Pre-119efdc the canonical preamble was `enable_a11y → shadow_dump (inspect) → ...` —
still works, just no longer required for the host-empty failure mode.)

---

## Pre-Authorization (AuthZ)

Every `browser_exec` call passes through the AuthZ engine before execution.
By default Sirin ships in **permissive mode** — all calls pass through and an
audit entry is written.

### Modes

| Mode | Behaviour |
|------|-----------|
| `permissive` (default) | All calls allowed; audit log written |
| `selective` | Rules evaluated in order; unmatched calls allowed |
| `strict` | Rules evaluated in order; unmatched calls denied |

Configure in `config/authz.yaml` (created on first run if missing):

```yaml
mode: permissive          # permissive | selective | strict
audit_log: data/authz_audit.jsonl
rules:
  - url_glob: "https://internal.corp/*"
    action: deny
  - url_glob: "https://payments.stripe.com/*"
    action: ask
  - js_contains: "document.cookie"
    action: deny
```

### Decision pipeline (per `browser_exec` call)

1. Mode `permissive` → immediately allow (skip rules)
2. Rules evaluated in declaration order → first match wins
   - `allow` → proceed
   - `deny` → return error immediately
   - `ask` / `ask_with_learn` → emit notification to Live Monitor, wait ≤30s
3. No rule matched: `selective` → allow, `strict` → deny

**`ask` / `ask_with_learn` flow:** Sirin emits an authz-ask event visible in
the Monitor UI's yellow modal panel. The operator clicks **Allow** or **Deny**.
If no decision arrives within 30 seconds, the call is denied automatically.
`ask_with_learn` additionally saves the decision as a rule for future calls.

### Audit log rotation

`data/authz_audit.jsonl` auto-rotates at **10 MB**, keeping up to 5 backups
(`authz_audit.jsonl.1` … `.5`). No manual rotation needed.

---

## Live Monitor

The Monitor panel (`Sirin GUI → Monitor` tab) shows real-time browser activity
and gives the operator interactive control over running tests.

### Action Feed

Every browser_exec step emitted by a test or imperative call appears as a
timestamped event: `[tool] action → status`. The feed auto-scrolls to newest
entries while running.

### Screenshot Pane

Live JPEG thumbnail (500 ms interval, 80% quality) of the active Chrome
window. The **⏸ Pause stream** toggle freezes the thumbnail without stopping
the test.

### Control Bar

| Button | Effect on `browser_exec` calls |
|--------|-------------------------------|
| **Pause** | All future calls block at `gate()` until Resumed. In-flight call completes first. |
| **Step** | Unblocks exactly one queued call, then re-pauses automatically. |
| **Abort** | Signals abort — all subsequent `gate()` calls return an error, terminating the test run. |
| **Reset** | Clears abort flag and resumes. Use after inspecting an aborted session. |

Control state is shared between the GUI and MCP server via an atomic
`ControlState` singleton — these buttons work whether the test was triggered
from the GUI or from a `run_test_async` MCP call.

### Authz Modal

When an `ask` or `ask_with_learn` rule matches, a yellow panel appears in the
Monitor showing:

- Tool name and action
- The URL being requested
- **Allow** and **Deny** buttons

Clicking Allow/Deny resolves the 30s wait in the MCP server pipeline.

### Trace Replay

Load historical `.sirin/trace-*.ndjson` files to replay the action feed
offline. The replay dropdown shows available trace files newest-first with
human-readable timestamps derived from the filename. Screenshot pane is
disabled during replay.

---

## Workflow Patterns

### Pattern A — Test a known YAML goal
```
list_tests → run_test_async → poll get_test_result → done
```

### Pattern B — Test an ad-hoc URL
```
run_adhoc_test → poll get_test_result → if failed: get_screenshot
```

### Pattern C — Debug a failed run
```
get_test_result       → get error + analysis
get_screenshot        → see what the page looked like
get_full_observation  → un-truncated tool output per step
list_recent_runs      → is this test historically flaky?
list_fixes            → is an auto-fix already in progress?
```

### Pattern D — Diagnose Sirin itself
```
config_diagnostics → errors/warnings + structured text_report
```

### Pattern E — Imperative exploration
```
browser_exec(goto)       → navigate
browser_exec(console)    → JS errors
browser_exec(eval)       → inspect DOM
browser_exec(screenshot) → visual state
```

### Pattern F — Exact-string assertion (K14/K15) via accessibility tree

When asserting on numeric values, error messages, or specific copy
where vision LLM precision loss matters:

```
1. browser_exec({action:"goto", target:"https://app/wallet",
                 browser_headless:false})    # Flutter needs visible
2. browser_exec({action:"enable_a11y"})       # wake Flutter semantics
3. browser_exec({action:"ax_find", role:"text", name:"Total Assets"})
   → {found:true, nodes:[{backend_id: 142, ...}]}
4. browser_exec({action:"ax_value", backend_id: 142})
   → {text: "$7376.80"}                       # LITERAL, not "about 7377"
5. (perform an action)
6. browser_exec({action:"ax_value", backend_id: 142})
   → {text: "$7277.50"}
7. assert: 7376.80 - 7277.50 == 99.30          # exact diff
```

`ax_*` is faster than `screenshot_analyze` (no LLM call) and works on
Flutter CanvasKit (which has no real DOM but does expose semantics).

### Pattern G — Race-free network assertion (request body)

When the assertion is on what the client **sent**, not just received:

```
1. browser_exec({action:"click", target:"#transfer-btn"})  # fires POST
2. browser_exec({action:"wait_request",
                 target:"/api/wallet/transfer", timeout:5000})
   → {request:{url, method:"POST", status:200,
               req_body:'{"amount":"99.30",...}',  ← LITERAL
               body:'{"new_balance":"7277.50"}'}}
3. assert: parse req_body, amount === "99.30"
```

`wait_request` auto-installs the network capture, so no need to
prime with `install_capture` first.

### Pattern H — Test isolation between sequential runs

K-series tests run back-to-back share Chrome state by default
(K13's auth leaks into K14):

```
clear_state → goto → fresh login form, regardless of previous test
```

Doesn't restart Chrome. Wipes cookies (all domains), localStorage,
sessionStorage, IndexedDB, and Cache Storage.

### Pattern I — OAuth / popup tab handling

```
1. click → opens popup tab (Telegram OAuth, Google login, Stripe)
2. wait_new_tab → Sirin auto-discovers via register_missing_tabs
                  and switches `active` to the new tab
3. interact in popup
4. switch_tab(0) → back to original
```

Without `wait_new_tab`, the popup is invisible to Sirin (headless_chrome
doesn't auto-track tabs spawned by `window.open`).

### Pattern J — Quick page orientation with `page_state`

When you need situational awareness before deciding what to assert:

```
1. browser_exec({action:"goto", target:"https://app/dashboard"})
2. page_state()
   → {url:"https://app/dashboard", title:"Dashboard",
      ax_summary:"button:Logout, text:Balance $7376.80, ...",
      console_recent:[], screenshot_b64:"..."}
3. Read ax_summary to plan ax_find targets — no extra round-trips
```

Use `page_state` to orient at the start of an ad-hoc exploration or when
a paused test hands control back for inspection.

### Pattern K — Before/after diff with `ax_snapshot` + `ax_diff`

When an action should change specific UI elements and you want a
machine-readable delta rather than re-reading every value manually:

```
1. browser_exec({action:"ax_snapshot", id:"before_transfer"})
   → {snapshot_id:"before_transfer", count:84}

2. browser_exec({action:"click", target:"#transfer-btn"})
3. browser_exec({action:"wait_request", target:"/api/wallet/transfer"})

4. browser_exec({action:"ax_snapshot", id:"after_transfer"})
   → {snapshot_id:"after_transfer", count:84}

5. browser_exec({action:"ax_diff",
                 before_id:"before_transfer", after_id:"after_transfer"})
   → {added:[], removed:[],
      changed:[{node_id:"142", before_name:"$7376.80",
                              after_name:"$7277.50"}]}

6. assert changed[0].after_name == "$7277.50"
```

Faster than scanning the full `ax_tree` — only the delta is returned.

### Pattern K2 — Wait for async UI update with `wait_for_ax_change`

For async UIs where the change fires milliseconds after the action:

```
1. browser_exec({action:"ax_snapshot", id:"baseline"})
2. browser_exec({action:"click", target:"#submit"})
3. browser_exec({action:"wait_for_ax_change",
                 baseline_id:"baseline", timeout_ms:5000})
   → {changed:true,
      diff:{added:[], removed:[],
            changed:[{node_id:"22", before_name:"Pending",
                                   after_name:"Confirmed"}]}}
```

### Pattern M — Condition waits (no sleep polling)

Instead of inserting fixed-delay `wait` calls or polling externally:

```
1. browser_exec({action:"goto", target:"https://app/login"})
2. browser_exec({action:"click", target:"#login-btn"})
3. browser_exec({action:"wait_for_url",
                 target:"#/dashboard", timeout_ms:8000})
   → {matched:true, url:"https://app/#/dashboard", elapsed_ms:1240}
4. browser_exec({action:"wait_for_ax_ready",
                 min_nodes:20, timeout_ms:8000})
   → {node_count:47, elapsed_ms:600}
5. browser_exec({action:"assert_ax_contains", role:"text", name:"Welcome"})
   → {passed:true, found:true, node:{...}}
```

Wait for async data load to finish before asserting:
```
browser_exec({action:"wait_for_network_idle", idle_ms:800, timeout_ms:15000})
→ {elapsed_ms:2100, request_count:12}
```

### Pattern N — Multi-session (parallel Chrome tabs)

For tests that need two users, two contexts, or two tabs running together:

```
1. browser_exec({action:"goto",
                 target:"https://app/buyer",   session_id:"buyer"})
2. browser_exec({action:"goto",
                 target:"https://app/seller",  session_id:"seller"})
3. browser_exec({action:"ax_find", role:"button", name:"Buy",
                 session_id:"buyer"})
4. browser_exec({action:"ax_find", role:"button", name:"Sell",
                 session_id:"seller"})
5. browser_exec({action:"list_sessions"})
   → {sessions:[{session_id:"buyer",  tab_index:1, url:"..."},
                {session_id:"seller", tab_index:2, url:"..."}]}
6. browser_exec({action:"close_session", target:"buyer"})
```

Omitting `session_id` always targets the default tab (index 0).

### Pattern L — Fixture setup/cleanup

When a test needs the app in a specific state before the goal runs:

```json
run_adhoc_test({
  "url": "https://app.com/transfer",
  "goal": "Transfer $99.30 and verify balance decreases",
  "success_criteria": ["Balance shows $7277.50 after transfer"],
  "fixture": {
    "setup": [
      {"action":"goto",  "target":"https://app.com/login"},
      {"action":"click", "target":"#quick-login-test"},
      {"action":"wait",  "target":".dashboard"}
    ],
    "cleanup": [
      {"action":"clear_state"}
    ]
  }
})
```

Setup failure → test status `error`. Cleanup always runs (errors logged,
not surfaced). YAML test goals use the same `fixture:` key — see
`docs/test-runner-roadmap.md` for the full schema.

---

## Failure Classification (auto-fix)

When `auto_fix: true` and a test fails, triage runs an LLM classifier against
the failure context:

| Category | Auto-fix target | Meaning |
|----------|:---:|---------|
| `ui_bug` | → frontend repo | Visible rendering/interaction issue |
| `api_bug` | → backend repo | Network 4xx/5xx or bad response body |
| `flaky` | no spawn | <70% historical pass rate |
| `env` | no spawn | Browser/network infrastructure issue |
| `obsolete` | no spawn | Selector not found — test needs update |

Dedup rules (see `list_fixes` outcome values above):
- Any `pending` fix within 30 minutes for the same test → skipped
- Last 3 consecutive `failed` outcomes → skipped (circuit breaker)

---

## Common Gotchas

### Flutter / WebGL target? Set `browser_headless: false`
CanvasKit and WebGL content do not paint reliably in headless Chrome.
Symptom: screenshots (and therefore `screenshot_analyze`) return an
all-black PNG regardless of what the page should show. Fix: pass
`browser_headless: false` to `run_adhoc_test` / `browser_exec`, or set
`browser_headless: false` in the test YAML, or set the global env
`SIRIN_BROWSER_HEADLESS=false` before launching Sirin.

### Hash-only routes (e.g. `/#/admin/users`)
Fragment changes don't emit `Page.frameNavigated`. Sirin auto-detects
this case and uses `location.hash = ...` + short settle delay instead
of waiting for a navigation event. No user action needed — just works.

### Vision approximates numbers — use `ax_*` for exact values
`screenshot_analyze` describes a page ("balance is about 7377 USDT");
the accessibility tree returns the literal string ("$7376.80").  For
any test that compares amounts, IDs, error messages, or hash strings
exactly, use the `ax_*` actions instead.

### Flutter semantics tree collapses
Flutter Web auto-disables its semantics tree when no AT activity is
detected.  Symptom: `ax_tree` returns 1-2 nodes instead of dozens.

Two distinct situations look identical: (1) cold start — needs bootstrap;
(2) post-navigation teardown — Flutter rebuilding, tree self-recovers in ~1s.

Sirin's `get_full_tree` detects ≤2 nodes, **polls 3×400ms first** to allow
self-recovery, then calls `enable_a11y` (placeholder click only — **Tab×2
permanently removed** in Issue #20 because it triggered URL resets on
hash-route Flutter apps via keyboard event delivery).

Use `wait_for_ax_ready` after navigation to block until the tree is ready:
```json
{"action":"wait_for_ax_ready","min_nodes":20,"timeout_ms":8000}
```

### Sequential tests share state — call `clear_state` between them
Cookies, localStorage, sessionStorage, IndexedDB all persist across
test runs (Chrome stays alive between them, by design — speed).
If K13 logs in and K14 expects a logged-out home page, K14 will
fail unexpectedly.  Insert `browser_exec({action:"clear_state"})`
between tests, or as the first step of each test goal.

### `network` returns nothing right after a click — race
The fetch/XHR fires asynchronously; calling `network` immediately
after a click usually misses it.  Use `wait_request` instead — it
polls the capture array until a matching URL substring appears.

### `switch_tab(1)` "out of range" after OAuth click
headless_chrome doesn't auto-track tabs spawned by `window.open` /
`target="_blank"`.  Use `wait_new_tab` after the click — it calls
`register_missing_tabs` to force discovery and adopts the popup
into the singleton.

### `ax_type` typed but value didn't change
Flutter Canvas inputs sometimes drop characters; React controlled
inputs may transform values; masked inputs add formatting.  Use
`ax_type_verified` to read back the actual value and check
`matched` (substring of typed text) before asserting.

### Mode-switch race
Switching between `browser_headless: true` and `browser_headless: false`
between calls triggers a fresh Chrome launch. The first `navigate()`
after launch might hit "wait: The event waited for never came" because
CDP subscriptions haven't fully initialised. Sirin handles this with
a 600ms settle delay + 1 auto-retry — transparent to the caller but
shows up in server logs.

### Port 7700 stuck
If Sirin was killed abruptly on Windows, the port can linger in
TIME_WAIT / CLOSE_WAIT for ~2 minutes. Sirin auto-retries bind 3× with
2s backoff; if still failing, launch with `SIRIN_RPC_PORT=7701`.

### `ax_find` with `name_regex` — anchoring
`name_regex` is matched via Rust's `regex` crate (no implicit anchoring).
`"^Balance$"` matches exactly; `"Balance"` matches substring. Combine
with `not_name_matches` to exclude irrelevant nodes:
```json
{"action":"ax_find","role":"text","name_regex":"\\$[0-9]",
 "not_name_matches":["Total","Header"],"limit":5}
```

### `ax_snapshot` IDs are session-scoped
Snapshot IDs live in Sirin's in-process memory — they do not persist
across Sirin restarts. Take a fresh snapshot at the start of each test
session; don't store IDs across days.

### AuthZ Ask timeout
When an `ask` rule matches, Sirin waits up to 30 seconds for an operator
decision from the Monitor UI. If the Monitor tab is not open, the decision
never arrives and the call is denied. Either switch to `permissive` mode or
keep the Monitor visible when running guarded tests.

---

## Safety Guarantees

- Ad-hoc runs persist to SQLite but skip auto-fix verification (no YAML to re-run)
- Verification re-runs always use `auto_fix=false` to prevent recursion
- Browser singleton auto-recovers from dead CDP connections (health check
  + one-shot retry in `with_tab`)
- LLM parse errors trigger reprompt up to `retry_on_parse_error` times (YAML
  per-test, default 3) before aborting
- Observation truncation at 800 chars includes a hint pointing to
  `get_full_observation` for the full text
- AuthZ audit log auto-rotates at 10 MB (5 backups kept)
- AuthZ mode defaults to `permissive` — no accidental denial on fresh installs

---

## Related

- `.claude/skills/sirin-launch/SKILL.md` — lifecycle (start/stop/restart)
- `.claude/skills/sirin-test/SKILL.md` — test workflows incl. Flutter playbook
- `docs/test-runner-roadmap.md` — feature evolution history
