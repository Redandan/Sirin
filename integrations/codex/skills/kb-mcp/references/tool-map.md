# Sirin KB Tool Map

## Projects

- `sirin`: Sirin runtime, browser/device testing, integrations, operations.
- `agora-backend`: AgoraMarketAPI marketplace and infrastructure knowledge.
- `flutter`: AgoraMarket Flutter/CanvasKit frontend knowledge.

## Read routing

- Unknown topic key: `mcp__sirin__kb_search`.
- Exact topic key: `mcp__sirin__kb_get`.
- Sirin KB summary: `mcp__sirin__kb_stats`.
- Sirin unavailable: use the five-tool `agora_kb_readonly` fallback only.

Reads require no Telegram approval. If a client prompts because metadata is
stale, refresh the Sirin MCP tool catalog before changing authorization.

## Write routing

Use `mcp__sirin__kb_write` only after explicit user authority to persist
knowledge. Never use `mcp__codex_apps__agoramarket_kbwrite` for local AI work.

Required fields: `topic_key`, `title`, `content`, `domain`.

Optional fields:

- `project`: `sirin`, `agora-backend`, or `flutter`; default `sirin`.
- `layer`: `raw`, `topic`, or `brief`; default `topic`.
- `status`: `draft` or `confirmed`; default `confirmed`.
- `confidence`: `0.0` through `1.0`; default `0.9`.
- `tags`, `file_refs`: comma-separated strings.

The Sirin tool is an idempotent topic-key upsert through a fixed server-local
`kbWrite` path. It does not expose arbitrary upstream MCP tools, external-AI
bearers, deletion, or stale-marking.

After `accepted`, call `kb_get` for the exact project/topic and confirm the
returned version and expected content. Until then report `MISSING_PROOF`.

## Layer choice

- `raw`: bounded session or runtime evidence.
- `topic`: durable architecture, workflow, contract, or decision.
- `brief`: maintained overview for a domain.
