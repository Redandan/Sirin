# Persona Subsystem — Architecture Overview

> Source: `src/persona/` (3 files, ~770 lines total)
> Related: [`docs/MCP_API.md`](../MCP_API.md) · [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md)

---

## 1. Purpose

The Persona subsystem defines the agent's identity and runtime behaviour policy.
It answers three questions for every incoming signal:

- **Who am I?** — name, tone, objectives, response voice (loaded from `persona.yaml`)
- **Should I care?** — ROI threshold gate (`determine_action_tier`)
- **What do I do about it?** — action tier decision: Ignore / LocalProcess / Escalate

Every part of Sirin that emits a reply, decides escalation, or records an event
reads from this subsystem. The `Persona::cached()` pattern ensures this is a
zero-I/O hot path after the first call.

---

## 2. Module Map

| File | Lines | Responsibility |
|------|------:|----------------|
| `mod.rs` | 270 | Config structs (`Persona`, `Identity`, `RoiThresholds`, `ResponseStyle`, `CodingAgentConfig`); YAML loading; process-wide `RwLock` cache |
| `behavior.rs` | 176 | `BehaviorEngine::evaluate()` — action tier classification + tone-aware response draft |
| `task_tracker.rs` | 324 | Append-only `TaskEntry` JSONL event log; atomic status rewrites; UI read tail |

---

## 3. Decision Flow

### 3a. Reading the Persona (hot path)

```
caller (telegram / researcher / followup / coding_agent / …)
        │
        ▼
Persona::cached()                                    mod.rs:177
        │
        ├── OnceLock first call
        │     └── Persona::load()
        │           └── fs::read_to_string(platform::config_path("persona.yaml"))
        │                 → serde_yaml::from_str → Persona
        │           on error → default_fallback() (hardcoded minimal YAML)
        │
        └── subsequent calls
              └── RwLock::read().clone()   ← no I/O, concurrent-safe
```

Callers that edit the persona (UI persona editor) must call `Persona::reload_cache()`
(`mod.rs:186`) after writing the new YAML. In-flight requests that already called
`cached()` use the old clone until their next `cached()` call.

### 3b. Evaluating an incoming message

```
Incoming signal (Telegram message, market event, etc.)
        │  estimated_value: f64  (caller-supplied USD estimate of event value)
        ▼
BehaviorEngine::evaluate(msg, estimated_value, &persona)    behavior.rs:76
        │
        ├── objective_match(text)           mod.rs:207
        │     └── case-insensitive substring scan of persona.objectives list
        │           → sets high_priority = true when any objective string found
        │
        ├── determine_action_tier(estimated_value, &persona)
        │     │                                              behavior.rs:34
        │     ├── value < min_usd_to_notify          → Ignore
        │     ├── value ≤ min_usd_to_call_remote_llm → LocalProcess
        │     └── value > min_usd_to_call_remote_llm → Escalate
        │
        ├── generate_response_draft(msg, &persona)    behavior.rs:44
        │     └── tone-branching:
        │           Brief    → "已收到，重點：{64 chars}…"  (+「高優先」if high_priority)
        │           Detailed → structured multi-line with priority + next-step
        │           Casual   → colloquial + priority-conditional phrasing
        │
        └── BehaviorDecision {
              draft, high_priority, matched_objective, tier, reason
            }
```

### 3c. Recording the decision to the task log

```
caller receives BehaviorDecision
        │
        ▼
TaskEntry::behavior_decision(persona, value, &decision)  task_tracker.rs:78
        │
        │  maps tier → initial status:
        │    Ignore       → "DONE"
        │    LocalProcess → "FOLLOWING"
        │    Escalate     → "PENDING", trigger_remote_ai = true
        │
        ▼
TaskTracker::record(&entry)
        └── JsonlLog::append() → {app_data}/tracking/task.jsonl
```

The UI reads `TaskTracker::read_last_n(50)` on every refresh to populate the
task board. The `followup` worker calls `update_statuses()` to flip entries
from PENDING → FOLLOWING → DONE as actions resolve.

### 3d. Status lifecycle

```
PENDING   ← Escalate tier, waiting for remote LLM / human action
    │
    ▼ followup worker picks up
FOLLOWING ← LocalProcess tier, or in-progress escalation
    │
    ▼ action resolves
DONE      ← Ignore tier (immediate), or completed action
```

---

## 4. Configuration

### 4a. File location

Resolved via `platform::config_path("persona.yaml")`:

| Mode | Path |
|------|------|
| Production (Windows) | `%LOCALAPPDATA%\Sirin\config\persona.yaml` |
| Production (macOS) | `~/Library/Application Support/Sirin/config/persona.yaml` |
| Test builds (`#[cfg(test)]`) | `./config/persona.yaml` (repo-relative) |

**Rule**: never hardcode the path — always call `platform::config_path("persona.yaml")`.

### 4b. Full schema with defaults

```yaml
identity:
  name: "助手1"                        # displayed in Telegram greeting + task log
  professional_tone: brief             # brief | detailed | casual

response_style:
  voice: "自然、親切、口吻"             # injected into Telegram LLM system prompt
  ack_prefix: "收到你的訊息。"          # prepended to auto-reply ack messages
  compliance_line: "我會一步一步協助你完成。"  # appended to instruction-following prompts

objectives:                            # list of strings; substring-matched against
  - "Monitor Agora"                    # incoming message text to set high_priority
  - "Maintain VIPs"

roi_thresholds:
  min_usd_to_notify: 5.0               # below → ActionTier::Ignore (silent drop)
  min_usd_to_call_remote_llm: 25.0     # above → ActionTier::Escalate (remote LLM)
                                       # between → ActionTier::LocalProcess

version: "1.0"
description: "Proactive Senior Architect AI agent with ROI awareness"

coding_agent:
  enabled: true
  project_root: "."                    # root for file-operation sandboxing; relative
                                       # to process cwd
  auto_approve_reads: true             # skip UI confirmation for read ops
  auto_approve_writes: false           # UI confirmation dialog before any file write
  allowed_commands:                    # exact-prefix allowlist for shell execution
    - "cargo check"
    - "cargo test"
    - "cargo build --release"
  max_iterations: 10                   # ReAct loop cap per coding task
  max_file_write_bytes: 102400         # 100 KB per-write cap

disable_remote_ai: false               # true → suppress Escalate tier from calling
                                       # the large/remote model backend
```

### 4c. Struct map (Rust ↔ YAML)

| Rust type | YAML key | Purpose |
|-----------|----------|---------|
| `Persona` | root | Top-level container; deserialized once at startup |
| `Identity` | `identity` | Name + tone enum |
| `ProfessionalTone` | `identity.professional_tone` | `Brief` / `Detailed` / `Casual` |
| `ResponseStyle` | `response_style` | Voice / ack / compliance strings injected into prompts |
| `RoiThresholds` | `roi_thresholds` | Two float thresholds controlling the action tier |
| `CodingAgentConfig` | `coding_agent` | ReAct coding agent permissions + limits |

All fields have `serde` defaults so a minimal YAML (just `identity` + `roi_thresholds`) is valid.

---

## 5. Notable Design Decisions

| Decision | Alternative considered | Reason |
|----------|------------------------|--------|
| **`OnceLock<RwLock<Persona>>`** (process-global cache) | `Arc<Mutex<Persona>>` threaded through callers | No refcount plumbing needed — any module calls `Persona::cached()` without a reference chain; `RwLock` allows many concurrent readers (Telegram, researcher, task_tracker all read simultaneously without contention) |
| **ROI thresholds in YAML** | Hardcoded constants or env vars | Operator-tunable without recompile; UI can edit + reload live; policy is separate from code |
| **`objective_match` as case-insensitive substring** | NLP / embedding similarity | Zero latency, no LLM call on hot path; objectives are short operator-defined strings where exact/partial match is sufficient |
| **`ProfessionalTone` as exhaustive enum** | Free-form string the LLM interprets | Compile-time exhaustiveness; no prompt-engineering drift; three tones cover all current use cases |
| **`disable_remote_ai` flag on Persona** | Env var only | Per-persona override enables fully-local deployment without touching the global LLM config |
| **`TaskTracker` independent of `BehaviorEngine`** | Inline logging inside `behavior.rs` | `BehaviorEngine::evaluate` is pure (data-in → data-out, no I/O); `TaskTracker` is `Clone`-able so it can be shared across worker threads; the two concerns are independently testable |
| **Status as string (`"PENDING"` etc.)** | Rust enum in `TaskEntry` | JSONL stays human-readable; UI filters by string without importing an enum; `followup` worker can write status updates without a full re-deserialize cycle |
| **`default_fallback()` on cache miss** | Propagate error upward | Prevents a missing `persona.yaml` from crashing the whole process; agent degrades gracefully with minimal defaults instead of panicking |

---

## 6. Known Limits / Future Work

### Current limits

- **`objective_match` is naive substring** — no stemming, no CJK word-boundary
  awareness. A Chinese objective `"監控 Agora"` won't match `"監控agora"` (space
  difference). Callers must keep objectives short and space-normalized.

- **Single persona per process** — `PERSONA_CACHE` is a process-wide singleton.
  The per-agent path in `src/telegram/mod.rs` spawns multiple agent listeners but
  they all share the same persona. Per-agent personas require a scoped cache.

- **`estimated_value` is caller-supplied with no validation** — callers that
  pass `0.0` (e.g., when value estimation is not implemented) always hit
  `ActionTier::Ignore`, silently dropping signals. There is no "unknown value"
  sentinel that bypasses the ROI gate.

- **`allowed_commands` is prefix-match only** — `"cargo check"` also matches
  `"cargo check --all-targets --message-format json"`. Commands that share a
  prefix with an allowed entry can bypass the allowlist.

- **`TaskTracker` path is caller-managed** — there is no global default path.
  Different call sites could construct trackers pointing to different JSONL files;
  the UI only reads the path the UI service module specifies.

- **`trim_to_max` is not called automatically** — `task.jsonl` grows unboundedly
  until the operator manually trims or Sirin restarts.

### Planned / future work

- **Per-agent persona**: `agents.yaml` specifies a `persona_path` per agent;
  loaded into an agent-scoped `RwLock<Persona>` alongside the global cache.
- **Semantic objective matching**: embed objectives at startup and cosine-
  similarity match incoming messages — improves CJK recall significantly.
- **ROI estimator service**: move `estimated_value` computation out of individual
  callers into a shared estimator so value signals are consistent and auditable.
- **Exact / wildcard `allowed_commands`**: replace prefix match with
  `glob`-style patterns (e.g., `"cargo check *"`).
- **File-watcher hot reload**: watch `persona.yaml` with `notify` crate;
  auto-call `reload_cache()` on write events without requiring a UI action.
