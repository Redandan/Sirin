# Memory Subsystem — Architecture Overview

> Source: `src/memory/` (3 files, ~1 500 lines total)
> Related: [`docs/architecture/persona.md`](./persona.md) · [`docs/MCP_API.md`](../MCP_API.md) · [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md)

---

## 1. Purpose

The memory subsystem provides three independent persistence layers for Sirin's
agents. Each layer targets a different latency, scope, and query pattern:

| Layer | What it stores | Query model | Backend |
|-------|----------------|-------------|---------|
| **FTS5 store** | Long-term knowledge: research reports, code summaries, LLM observations | Full-text search (BM25) + recency list | SQLite FTS5 @ `memories.db` |
| **Codebase index** | Architecture-aware file summaries + symbol lists for the local repo | TF-scored keyword search | JSONL @ `codebase_index.jsonl` |
| **Context ring-log** | Recent user↔assistant dialogue turns per peer | Tail read (last N entries) | JSONL @ `sirin_context_{agent}_{peer}.jsonl` |

The three layers share the `{app_data}/memory/` and `{app_data}/tracking/`
directories but have no shared state — each is safe to read/write concurrently
with the others.

---

## 2. Module Map

| File | Lines | Responsibility |
|------|------:|----------------|
| `mod.rs` | 365 | SQLite FTS5 store: `memory_store`, `memory_search`, `memory_list_recent`; JSONL → SQLite migration on first startup; agent-memory isolation |
| `codebase.rs` | ~1 050 | Project file traversal, symbol extraction, TF scoring, `search_codebase`, `list_project_files`, `inspect_project_file_range`, `ensure_codebase_index` / `refresh_codebase_index` |
| `context.rs` | 97 | Per-peer ring-log: `append_context`, `load_recent_context`, `collect_reply_samples` |

---

## 3. Schema

### 3a. FTS5 memory store — `memories.db`

```sql
-- Shared (global) memories — visible to all callers including anonymous MCP
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
    USING fts5(text, source, timestamp, tokenize='unicode61');

-- Per-agent confidential memories — only visible to the owning agent
CREATE TABLE IF NOT EXISTS agent_memories (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    text           TEXT    NOT NULL,
    source         TEXT    NOT NULL,
    timestamp      TEXT    NOT NULL,     -- RFC 3339
    owner_agent_id TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_am_owner ON agent_memories(owner_agent_id);
```

**Routing logic in `memory_store(text, source, owner_agent_id, visibility)`:**

| `owner_agent_id` | `visibility` | Target table |
|------------------|--------------|--------------|
| `""` (empty) | any | `memories_fts` (shared) |
| non-empty | `"shared"` | `memories_fts` (shared) |
| non-empty | `"confidential"` | `agent_memories` (private) |

### 3b. Codebase index — `codebase_index.jsonl`

One JSON object per line (one per indexed file):

```json
{
  "path":    "src/memory/mod.rs",
  "kind":    "rust-source",
  "summary": "Persistent memory for Sirin.",
  "symbols": ["memory_db", "memory_store", "memory_search", "memory_list_recent"],
  "text":    "File: src/memory/mod.rs\nKind: rust-source\nRole: ...\nSymbols: ...\n\nExcerpt:\n..."
}
```

- `kind` values: `rust-source`, `cargo-config`, `documentation`, `yaml-config`,
  `frontend-source`, `javascript-source`, `json-config`, `text`
- `symbols`: up to 12 extracted from first 240 lines (functions, structs, enums,
  traits, mods — Rust and TS/JS)
- `text`: pre-formatted search blob including path, kind, role hint, symbols, and
  1 600-char file excerpt; scored by `score_entry` at query time

### 3c. Per-peer context — `sirin_context_{agent_id}_{peer_id}.jsonl`

One JSON object per conversation turn:

```json
{
  "timestamp":       "2026-04-19T09:10:17Z",
  "user_msg":        "你好，幫我查一下 Telegram 訊息",
  "assistant_reply": "已收到，正在查詢……"
}
```

Filename pattern:

| `agent_id` | `peer_id` | Filename |
|------------|-----------|----------|
| `"助手1"` | `123456` | `sirin_context_助手1_123456.jsonl` |
| `"助手1"` | `None` | `sirin_context_助手1.jsonl` |
| `None` | `123456` | `sirin_context_123456.jsonl` |
| `None` | `None` | `sirin_context.jsonl` |

---

## 4. Query Patterns

### 4a. Full-text search (FTS5 store)

```
memory_search(query, limit, caller_agent_id)           mod.rs:207
        │
        ├── sanitize_fts5_query(query)
        │     └── wrap each whitespace token in double-quotes
        │           (prevents FTS5 syntax injection)
        │
        ├── SELECT text FROM memories_fts
        │     WHERE memories_fts MATCH ?  ORDER BY rank LIMIT ?
        │     (BM25 ranking — lower rank = more relevant)
        │
        └── if caller_agent_id non-empty:
              ├── SELECT text FROM agent_memories
              │     WHERE owner_agent_id = ? AND text LIKE ?   (own private)
              └── for each peer in meeting::readable_owners(caller_agent_id):
                    SELECT text FROM agent_memories WHERE owner_agent_id = ? AND text LIKE ?
                    (shared via active meeting)
```

### 4b. Recency list (FTS5 store)

```
memory_list_recent(limit, caller_agent_id)             mod.rs:170
        │
        ├── SELECT text FROM memories_fts
        │     ORDER BY rowid DESC LIMIT ?
        │
        └── if caller_agent_id non-empty:
              SELECT text FROM agent_memories
                WHERE owner_agent_id = ? ORDER BY id DESC LIMIT ?
              → merge + truncate to limit
```

### 4c. Codebase search

```
search_codebase(query, limit)                          codebase.rs:634
        │
        ├── ensure_codebase_index()
        │     └── refresh if codebase_index.jsonl is stale (> 10 min old)
        │
        ├── tokenize(query)  →  lowercase tokens, CJK split per character
        │
        ├── for each JSONL line:
        │     score_entry(entry.text, query_terms)
        │         = Σ term_freq(term) / doc_len   (TF, no IDF)
        │
        ├── sort by score DESC, take limit
        │
        └── top-1 result: append 1-hop call-graph context
              (callers / callees from code_graph module)
```

### 4d. Per-peer context load

```
load_recent_context(limit, peer_id, agent_id)          context.rs:55
        │
        └── JsonlLog::read_last_n(limit)
              reads the tail of sirin_context_{agent_id}_{peer_id}.jsonl
              returns Vec<ContextEntry> newest-last
```

Chat and coding agents call `load_recent_context` at the start of every request
to inject recent dialogue into the LLM system prompt.

---

## 5. Notable Design Decisions

| Decision | Alternative considered | Reason |
|----------|------------------------|--------|
| **SQLite FTS5 for long-term memory** | JSONL append-only (legacy format) | BM25 relevance ranking; atomic writes; no full-scan on search; JSONL backup migrated to SQLite on first startup for existing installs |
| **`OnceLock<Mutex<Connection>>` for DB** | Per-call `Connection::open` | SQLite WAL + single-writer; re-opening per call is slow and risks "database is locked" errors under concurrent agents; global Mutex serialises writes |
| **`unicode61` tokenizer in FTS5** | Default (ascii) | Handles CJK, accented Latin, and full-width characters correctly without custom code |
| **`sanitize_fts5_query` wraps tokens in quotes** | Pass raw query | FTS5 MATCH syntax treats `(`, `)`, `AND`, `OR`, `NOT`, `*`, `^` as operators — raw user queries would cause parse errors or unexpected boolean logic |
| **JSONL ring-log for codebase index** (overwrite on refresh) | SQLite table | Full repo scan rebuilds the whole index; a single sequential overwrite is simpler and faster than diffing an existing table |
| **Per-file per-peer context JSONL** | Single shared context table | Files are independent → no cross-peer lock contention; `collect_reply_samples` can scan the directory by filename prefix without a query planner |
| **CJK tokenization: one character per token** | Word segmentation library (jieba etc.) | Zero dependencies; single CJK character is the minimal meaningful unit for TF scoring; avoids a large optional dependency |
| **`ensure_codebase_index` with 10-min staleness** | Always refresh | Rebuilding the index blocks the calling request; 10 min is short enough that edits are reflected promptly, long enough to avoid per-call rebuilds |
| **`agent_memories` LIKE search (not FTS5)** | Second FTS5 virtual table | `agent_memories` is expected to be small (single-agent private notes); LIKE is sufficient and avoids maintaining a second FTS5 index |
| **Meeting-shared reads via `meeting::readable_owners`** | Explicit caller list | Decouples memory from meeting topology; memory module only reads the owner list, doesn't manage meeting state |

---

## 6. Known Limits / Future Work

### Current limits

- **Global Mutex serialises all DB access** — FTS5 search blocks concurrent
  `memory_store` calls. Under heavy multi-agent load (N=4 workers), all agents
  queue on the same lock. No read-write split (SQLite WAL is not fully exploited).

- **`memories_fts` grows unboundedly** — no eviction, no TTL, no `trim_to_max`
  equivalent for the SQL store. Long-running instances will accumulate entries
  indefinitely; query latency increases as the table grows.

- **Codebase index is a full overwrite** — every `refresh_codebase_index()` call
  re-scans the entire repo and rewrites the JSONL. On large repos (>10 000 files)
  this can take several seconds; there is no incremental/delta update.

- **`codebase_index.jsonl` excluded dirs are hardcoded** — `.git`, `target`,
  `node_modules`, `.next`, `dist`, `build`. Any project with non-standard build
  output directories (e.g., `out/`, `artifacts/`) will be indexed unnecessarily.

- **TF scoring (no IDF)** — `score_entry` computes raw term frequency without
  inverse-document-frequency weighting. Common terms in many files (e.g., `use`,
  `pub`) score the same as rare terms (`memory_store`). Result quality degrades
  on generic queries.

- **Per-peer context has no size cap** — `append_context` never trims the file.
  Active peers with long conversation histories accumulate entries; `read_last_n`
  remains O(file size) because JSONL has no random-access tail index.

- **`collect_reply_samples` scans the directory linearly** — reads every
  `sirin_context_{agent_id}_*.jsonl` file on each call; not cached; can be slow
  when many peers have been active.

- **`agent_memories` visibility tied to `meeting::readable_owners`** — if the
  meeting module is not initialised (headless / no meeting context), cross-agent
  memory sharing silently returns empty results.

### Planned / future work

- **Eviction policy for `memories_fts`**: keep the N most-recent entries or
  entries within the last X days; expose as a Sirin MCP tool.
- **Incremental codebase index**: track file mtimes; only re-index changed files.
- **IDF weighting**: pre-compute document frequencies at index-build time and
  apply them in `score_entry` for better relevance ranking.
- **Context ring-log size cap**: auto-trim per-peer JSONL to a configurable max
  (e.g., 500 turns) on each `append_context` call.
- **Async DB access**: replace the global blocking `Mutex` with `tokio::sync::Mutex`
  and `Connection` in WAL mode to allow concurrent FTS5 reads.
