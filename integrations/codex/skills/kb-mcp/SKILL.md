---
name: kb-mcp
description: Use for project knowledge, prior decisions, architecture conventions, workflow rules, stale-doc checks, or KB updates across Sirin, AgoraMarketAPI/agora-backend, and Flutter/AgoraMarket. Local AI sessions route KB access through Sirin and never use the broad external-AI connector for ordinary KB work.
---

# KB MCP

## Ownership

Sirin is the only local control plane for KB access. The live KB remains hosted
by AgoraMarketAPI, but credentials stay behind Sirin or the server-local bridge.

Start by confirming `http://127.0.0.1:7700/mcp` is live. Use the Sirin tools:

- `kb_search` to discover entries.
- `kb_get` for an exact topic key.
- `kb_stats` for Sirin's KB summary.
- `kb_write` only when the user explicitly asks to save or update knowledge.

Read [references/tool-map.md](references/tool-map.md) when choosing project,
layer, status, or the write verification path.

## Authorization boundary

Routine Sirin KB reads and explicitly requested `kb_write` calls do not use
`getMcpAuthProbe` and do not require per-call Telegram approval. Never route a
local KB update through `mcp__codex_apps__agoramarket_kbwrite`; that is an
external-AI connector with a different approval policy.

Sirin `kb_write` uses a fixed server-local `kbWrite` path. The remote service
credential is consumed on the AgoraMarketAPI host and is never exposed to the
caller. If Sirin reports missing SSH configuration, policy drift, or transport
failure, fail closed and report the exact gap. Do not fall back to the broad
connector merely to make the write succeed.

This removes repetitive human approval, not the intent boundary: do not write
KB unless the user requested knowledge persistence or an established workflow
explicitly authorizes the write.

## Workflow

1. Search before writing so an existing durable topic is updated rather than
   duplicated.
2. Use project `sirin`, `agora-backend`, or `flutter` exactly.
3. Prefer `topic/confirmed` for durable decisions and `raw/draft` for bounded
   session evidence.
4. Include decision, rationale, evidence, file references, and rollback or
   stale criteria when relevant.
5. Treat `accepted` as queued upstream work. Read the exact topic back and
   confirm its version/content before reporting completion.

`KB_WRITE_TELEMETRY` controls only automatic test-run raw notes. It must remain
independent from explicitly requested manual `kb_write` calls.

## Fallback reads

If Sirin is unavailable, routine reads may use the dedicated
`agora_kb_readonly` MCP bridge. It exposes only `kbHealth`, `kbSearch`, `kbGet`,
`kbStaleCheck`, and `kbExport`. Never add mutation tools to that allowlist.

## Evidence

Separate live KB results, local source/configuration, inference, and
`MISSING_PROOF`. A local file or accepted asynchronous write is not proof that
the live KB entry changed.
