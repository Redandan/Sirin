# Researcher Subsystem — Architecture

> Source: `src/researcher/` | Last updated: 2026-04-19

---

## 1. Purpose

The researcher subsystem runs a **multi-step LLM research pipeline** triggered by a topic string and an optional URL. It is designed to be spawned as a background async task — the caller gets the completed `ResearchTask` back when the pipeline finishes, and the UI can poll intermediate progress in real time because every phase is persisted immediately after it completes.

The pipeline does four things that a plain LLM prompt cannot:

1. **Fetches real page content** (if a URL is provided) rather than relying on training data.
2. **Self-generates follow-up questions** so the depth of investigation scales beyond what the user specifies.
3. **Fans out to DDG web search** for each question in parallel, grounding answers in live results.
4. **Synthesises a structured final report** and writes it into the FTS5 memory store so other agents can recall it later.

A secondary behaviour fires every 5th successful research: the LLM is asked whether the persona's objectives should be updated based on accumulated findings. The proposal goes to a UI review slot — it is **never written directly** to `persona.yaml`.

---

## 2. Module Map

| File | Responsibility |
|------|----------------|
| `mod.rs` | Public entry point (`run_research`), `ResearchTask` / `ResearchStep` / `ResearchStatus` types, persona-objectives review slot (`pending_objectives_slot`, `take_pending_objectives`) |
| `fetch.rs` | Scraping HTTP client (`scraping_http`) + HTML-to-text extraction (`fetch_page_text`), capped at 4 000 chars |
| `pipeline.rs` | Five-phase async pipeline (`pipeline`), DDG parallel fan-out, synthesis; also `maybe_reflect_on_objectives` |
| `persistence.rs` | JSONL-backed store for `ResearchTask` via `JsonlLog`; `save_research` / `list_research` / `get_research` |

---

## 3. Pipeline Stages

```
caller
  │
  ▼
run_research(topic, url?)          ← mod.rs — creates ResearchTask, saves initial state
  │
  ├─ [URL given?] ──────────────────────────────────────────────────────────┐
  │                                                                          │
  ▼                                                                          ▼
Phase 1: fetch_page_text(url)                                           (skip)
  fetch.rs — reqwest GET → HTML → strip script/style/head → plain text
  capped at MAX_PAGE_TEXT (4 000 chars)
  step saved: { phase: "fetch", output: "已擷取 N 字元內容" }
  │                                                                          │
  └──────────────────────────────────────────────────────────────────────────┘
  │
  ▼
Phase 2: overview_prompt → call_prompt(llm)
  Input: page text (if fetched) OR raw topic
  Output: structured 【是什麼】【主要功能】【關鍵技術/實體】
  step saved: { phase: "overview", output: <analysis> }
  │
  ▼
Phase 3: questions_prompt → call_prompt(llm)
  Input: Phase 2 overview
  Output: exactly 4 numbered questions (Traditional Chinese)
  step saved: { phase: "questions", output: "Q1\nQ2\nQ3\nQ4" }
  │
  ▼
Phase 4: join_all(4 × async sub-tasks)          ← parallel fan-out
  Each sub-task:
    ├─ ddg_search(question)  →  top-3 results (title, snippet, url)
    └─ qa_prompt → call_prompt(llm)
       Input: question + search results
       Output: 3-5 sentence answer (Traditional Chinese)
  4 steps saved: { phase: "research_q1" … "research_q4", output: "Q: …\nA: …" }
  │
  ▼
Phase 5: synthesis_prompt → call_prompt(llm)
  Input: overview (first 800 chars) + all Q&A pairs
  Output: structured report
    【執行摘要】【核心發現】【詳細分析】【結論與建議】
  Report stored in FTS5 memory: memory_store(report[:2000], "research", "", "shared")
  task.final_report = Some(report)
  task.status = Done
  │
  ▼
events::publish(AgentEvent::ResearchCompleted { topic, task_id, success })
  │
  ├─ [done_count % 5 == 0] ────────────────────────────────────────────────►
  │                                                                          │
  ▼                                                                          ▼
return ResearchTask                                      maybe_reflect_on_objectives()
                                                           → proposal stored in
                                                             pending_objectives_slot
                                                           → NOT written directly
```

### Phase constants

| Constant | Value | Location |
|----------|-------|----------|
| `MAX_PAGE_TEXT` | 4 000 chars | `fetch.rs` |
| `MAX_CONTEXT` | 2 000 chars (fed to overview prompt) | `pipeline.rs` |
| Overview snippet for synthesis | 800 chars | `pipeline.rs` |
| Memory store snippet | 2 000 chars | `pipeline.rs` |
| Reflection cadence | every 5th Done task | `mod.rs` |

---

## 4. Storage

### 4.1 Research log — `research.jsonl`

**Path:** `{app_data_dir}/tracking/research.jsonl`  
(`%LOCALAPPDATA%\Sirin\tracking\research.jsonl` on Windows)

**Format:** one JSON line per `ResearchTask`. The `JsonlLog` helper upserts by `task.id`, so each `save_research` call rewrites the matching line in-place (or appends if new).

**Schema:**

```json
{
  "id":           "r-1776581377892",
  "topic":        "Rust async/await 底層工作原理",
  "url":          null,
  "status":       "done",          // "running" | "done" | "failed"
  "steps": [
    { "phase": "fetch",       "output": "已擷取 3841 字元內容" },
    { "phase": "overview",    "output": "【是什麼】..." },
    { "phase": "questions",   "output": "問題1\n問題2\n問題3\n問題4" },
    { "phase": "research_q1", "output": "Q: ...\nA: ..." },
    { "phase": "research_q2", "output": "Q: ...\nA: ..." },
    { "phase": "research_q3", "output": "Q: ...\nA: ..." },
    { "phase": "research_q4", "output": "Q: ...\nA: ..." },
    { "phase": "synthesis",   "output": "報告已生成 (2847 chars)" }
  ],
  "final_report": "【執行摘要】...",
  "started_at":   "2026-04-19T14:49:37Z",
  "finished_at":  "2026-04-19T14:53:18Z"
}
```

### 4.2 FTS5 memory store

After Phase 5 the first 2 000 chars of the final report are written into the shared FTS5 memory store (`src/memory/mod.rs`) with tag `"research"`. This makes research findings **searchable by all other agents** via `memory_store` queries.

### 4.3 Pending objectives slot

`pending_objectives_slot()` is an in-process `OnceLock<Mutex<Option<Vec<String>>>>`. The UI calls `take_pending_objectives()` on each refresh cycle. If a proposal exists, the UI displays a confirmation dialog — the user must approve before `persona.yaml` is written.

---

## 5. Notable Design Decisions

### 5.1 All phases persisted after each step
Every `task.steps.push(...)` is immediately followed by `save_research(task)`. This means the UI can display live progress (e.g. "Phase 3 / questions done") without polling the in-memory struct, and a crash mid-pipeline leaves a recoverable partial record.

### 5.2 Phase 4 fan-out is unconditionally parallel
All 4 question sub-tasks run via `futures_util::future::join_all`. A single question failure is logged and skipped; the pipeline only hard-fails if *all* questions fail. This maximises throughput under flaky DDG results.

### 5.3 Pure-Rust HTML extraction, no C parser
`fetch.rs` uses `regex` to strip `<script>`, `<style>`, `<noscript>`, and `<head>` blocks, then removes remaining tags. This avoids a C/C++ HTML parser dependency (e.g. `html5ever`) at the cost of not handling pathological HTML. The 4 000-char cap limits the impact of malformed pages.

### 5.4 Persona reflection is user-gated, not auto-write
`maybe_reflect_on_objectives` intentionally does **not** call `persona.save()`. Instead it parks the proposal in a `Mutex<Option<Vec<String>>>` and fires a `PersonaUpdated` event. The UI owns the confirmation flow. This prevents runaway objective drift without human oversight.

### 5.5 Shared HTTP clients via OnceLock singletons
`scraping_http()` (custom User-Agent, 60 s timeout) and `crate::llm::shared_http()` (LLM backend) are both `OnceLock` singletons. This avoids spinning up new connection pools per research task.

### 5.6 LLM language is hardcoded to Traditional Chinese
All prompts explicitly say "Respond in Traditional Chinese". This matches the primary user language of the product and keeps the overview/Q&A/report consistent regardless of which LLM backend is in use.

---

## 6. Known Limits / Future Work

| Limit | Detail | Possible fix |
|-------|--------|--------------|
| **No resumption** | A crashed pipeline is persisted with `status: running` but never automatically retried. `list_research()` will show stale running tasks. | Add a startup sweep (like `reset_stale_running` in the squad worker) that re-queues interrupted tasks. |
| **DDG search quality** | `ddg_search` returns at most 3 results per question with no reranking. Some questions return empty. | Integrate a proper search API (Serper, SerpApi) or increase result count + filter by domain. |
| **4 000-char page cap** | Long-form articles or SPAs with deferred content are truncated. | Add a `max_pages` option + pagination aware fetch; or use browser CDP for JS-rendered pages. |
| **No question deduplication** | Phase 3 can generate overlapping questions if the topic is narrow. | Add a similarity filter on the generated question list before Phase 4. |
| **Memory store gets first 2 000 chars only** | Long reports are truncated in the FTS5 store. | Store a summary instead of a raw truncation, or split into multiple store entries. |
| **`maybe_reflect_on_objectives` is fire-and-forget** | Any LLM error is logged and silently dropped. The UI may miss a legitimate objective update. | Add an error step to the task record so the UI can surface reflection failures. |
| **No URL validation** | `run_research` accepts any string as URL; DNS failures surface as a step error not a pre-flight check. | Validate URL scheme and reachability before spawning the pipeline. |

---

## Cross-references

- `src/memory/mod.rs` — FTS5 memory store that receives the final report
- `src/skills.rs` — `ddg_search` used in Phase 4
- `src/llm/mod.rs` — `call_prompt`, `shared_http`, `shared_llm`
- `src/persona/mod.rs` — `Persona::cached()` used in `maybe_reflect_on_objectives`
- `src/events.rs` — `AgentEvent::ResearchCompleted` + `PersonaUpdated`
- `docs/MCP_API.md` — MCP tool `run_research` (if exposed externally)
