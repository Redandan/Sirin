# LLM Layer — Architecture Overview

> Source: `src/llm/` (mod.rs · backends.rs · probe.rs)

---

## 1. Purpose

`src/llm/` is the single integration point for all language-model I/O in Sirin.
It provides:

- A **unified call API** (`call_prompt`, `call_coding_prompt`, `call_router_prompt`,
  `call_large_prompt`, `call_prompt_stream`, `call_prompt_messages`, `call_vision`)
  that dispatches to the correct wire transport based on runtime configuration.
- **Four named model roles** (chat / router / coding / large) each backed by a
  dedicated `OnceLock` singleton so the selection logic runs exactly once.
- A **fleet probe** that queries the active backend at startup, classifies every
  discovered model by capability, and auto-assigns roles — eliminating manual config
  for most local setups.
- **Multimodal / vision** support that flows image bytes through the same dispatch
  path without leaking into callers.

No agent or UI module imports `reqwest`, constructs model names, or hard-codes
endpoints. All of that is encapsulated here.

---

## 2. Module Map

| File | Responsibility |
|------|----------------|
| `src/llm/mod.rs` | Core types (`LlmConfig`, `LlmBackend`, `LlmMessage`, `MessageRole`), four process-wide `OnceLock` singletons, UI-persisted config (`LlmUiConfig`), all public `call_*` entry points, vision helpers, base64 encoder |
| `src/llm/backends.rs` | HTTP wire types and transport functions — `call_ollama`, `call_openai`, `call_openai_messages`, `stream_ollama`, `stream_openai`; 429 retry with exponential back-off |
| `src/llm/probe.rs` | Model discovery (`list_ollama_models`, `list_lmstudio_models`), capability classification (`classify_model_capabilities`), role-assignment logic (`best_for_role`, `assign_fleet_role`), `AgentFleet`, `probe_and_build_fleet` |

---

## 3. Backend Abstraction

### The `LlmBackend` enum

```rust
pub enum LlmBackend {
    Ollama,
    LmStudio,   // also covers plain OpenAI-compatible endpoints
    Gemini,     // Google — OpenAI-compat endpoint at generativelanguage.googleapis.com
    Anthropic,  // Claude — OpenAI-compat endpoint at api.anthropic.com/v1
}
```

From the call-site's perspective the backend is invisible.  `call_prompt` (and
all its siblings) match on `llm.backend` and route to one of two transports:

```
Ollama   →  backends::call_ollama      POST /api/generate   (Ollama native)
others   →  backends::call_openai*     POST /chat/completions (OpenAI-compat)
```

Gemini and Anthropic reuse the OpenAI-compatible transport — they expose the
same `/chat/completions` shape, so no extra backend is needed.

### Adding a new backend

1. Add a variant to `LlmBackend`.
2. Add an env-var branch in `LlmConfig::from_env()`.
3. Decide whether the wire format is Ollama-native or OpenAI-compatible:
   - **Ollama-native** → add a `call_<name>` function in `backends.rs`.
   - **OpenAI-compat** → add the new variant to the `LlmBackend::LmStudio | LlmBackend::Gemini | LlmBackend::Anthropic` arms in `mod.rs` (no new transport function needed).
4. Add capability detection to `classify_model_capabilities` in `probe.rs` if the
   backend exposes a model-list endpoint.

### Config resolution order

```
env vars  →  apply_yaml_overrides()  →  LlmConfig singleton
```

`config/llm.yaml` (written by the Settings UI) can override any env-var field.
Blank YAML fields are silently skipped so existing `.env` values remain active.
A process restart is required for changes to take effect.

### Environment variables

| Variable | Default | Notes |
|----------|---------|-------|
| `LLM_PROVIDER` | `ollama` | `ollama` / `lmstudio` / `openai` / `gemini` / `anthropic` |
| `OLLAMA_BASE_URL` | `http://localhost:11434` | |
| `OLLAMA_MODEL` | `llama3.2` | |
| `LM_STUDIO_BASE_URL` | `http://localhost:1234/v1` | also `OPENAI_BASE_URL` |
| `LM_STUDIO_MODEL` | `llama3.2` | also `OPENAI_MODEL` |
| `LM_STUDIO_API_KEY` | *(empty)* | also `OPENAI_API_KEY` |
| `GEMINI_MODEL` | `gemini-2.0-flash` | requires `GEMINI_API_KEY` |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-6` | requires `ANTHROPIC_API_KEY` |
| `ROUTER_MODEL` | *(falls back to main)* | small/fast model for intent classification |
| `ROUTER_LLM_PROVIDER` | *(same as main)* | keep routing local when main is remote |
| `CODING_MODEL` | *(falls back to main)* | dedicated coding model |
| `LARGE_MODEL` | *(falls back to main)* | large reasoning model |

---

## 4. Fleet Probe

### Startup sequence

```
main.rs
  └─ probe_and_build_fleet(&client)          // probe.rs
       ├─ LlmConfig::from_env()              // read env vars
       ├─ list_{ollama,lmstudio}_models()    // HTTP GET with 2–5 s timeout
       │    └─ (empty?) auto_probe_local_backends()
       │         ├─ try Ollama  :11434
       │         └─ try LM Studio :1234/v1
       ├─ classify_model_capabilities()      // per model
       ├─ assign_fleet_role() × 3            // router / coding / large
       └─ → AgentFleet
  └─ init_agent_fleet(fleet)                 // store in OnceLock
  └─ init_shared_llm(fleet.to_llm_config())
```

### Capability classification (`classify_model_capabilities`)

Each model is classified by pattern-matching its name and checking its reported
byte size.  The rules in priority order:

| Capability | Detection rule |
|------------|---------------|
| `Embedding` | name contains `embed`, `nomic`, `all-minilm`, `bge-`, starts with `e5-` — stop, not generative |
| `Vision` | name contains `vision`, `llava`, `bakllava`, `moondream`, `minicpm-v`, `qwen-vl`, `qwen2.5-vl`, `qwen2-vl`, `internvl`, `cogvlm`, `gemma-3/4`, `phi-3.5-vision`, `phi-4` |
| `Code` | name contains `qwen2.5-coder`, `codellama`, `starcoder`, `deepseek-coder`, `devstral`, `coder` (excl. `decoder`), `code-` |
| `Large` | name suffix `:70b`/`-70b`/`72b`/`65b`/`34b`/`32b`, or `mixtral`, `opus`; OR size ≥ 20 GB |
| `Fast` | name contains `tinyllama`, `phi3-mini`, `smollm`, `tinydolphin`, small `qwen`/`gemma:2b`; OR size < 4 GB |
| `Chat` | all generative models (always added) |

### Role assignment priority

For each role (router → `Fast`, coding → `Code`, large → `Large`):

1. Env var set **and** model confirmed in the classified list → use it.
2. Env var set **but** model not found → warn, fall back to chat model.
3. Env var absent → `best_for_role` auto-selects:
   - `Fast`: smallest by byte size (minimise load time).
   - `Code`: priority list (`qwen2.5-coder` → `deepseek-coder` → `devstral` → …).
   - `Large`: largest by byte size (maximise capability).

All three role slots must differ from `chat_model` — `best_for_role` excludes
the chat model by name/base-name.

### Non-fatal degradation

- Backend unreachable → auto-fallback to Ollama → LM Studio.
- No local service found → minimal fleet (no classified models), GUI still launches.
- Remote backends (Gemini, Anthropic) skip auto-probe since they never return an
  empty model list while reachable.

---

## 5. Vision / Multimodal

### Call path

```rust
call_vision(client, llm, prompt, image_base64, mime)
```

- **Ollama** (llava, moondream, minicpm-v, …): sends `{ images: [base64] }` in
  the `/api/generate` body alongside the prompt.
- **All others** (LM Studio, Gemini, Anthropic): constructs an `OpenAiMessage`
  with a `content` array:

  ```json
  [
    { "type": "text",      "text": "<prompt>" },
    { "type": "image_url", "image_url": { "url": "data:image/png;base64,…" } }
  ]
  ```

  The message is forwarded to `call_openai_messages` — no extra transport needed.

### Screenshot shortcut

```rust
analyze_screenshot(client, llm, prompt)
```

Captures the current browser page via `browser::screenshot()` (spawned on the
blocking thread pool), base64-encodes the PNG with the local `base64_encode_bytes`
helper (no external crate dependency), then delegates to `call_vision`.

### Limitations

Vision only works with models that support it.  The fleet probe marks
vision-capable models with `ModelCapability::Vision` but does not automatically
promote them to the main chat role — callers that need vision must pass an
appropriate `LlmConfig` (e.g., obtained via `LlmConfig::for_override`).

---

## 6. Notable Design Decisions

### No LangChain / LLM framework

Sirin's LLM layer is intentionally thin.  Frameworks like LangChain add
abstraction layers, dependency weight, and version-churn risk.  The actual
surface area needed (single-turn, multi-turn, stream, vision, role dispatch) fits
comfortably in ~900 lines of plain Rust.  Keeping it in-tree means the team
controls every behaviour — retry policy, logging granularity, role fallback logic.

### Probe on startup, not per-call

Model availability is checked once at process start and stored in an `OnceLock`.
Per-call probing would add latency to every request and create race conditions
under concurrent agent load.  The trade-off is that adding a new Ollama model
requires a restart — acceptable given that model changes are infrequent.

### `OnceLock` singletons for all four configs

`shared_llm`, `shared_router_llm`, `shared_large_llm`, and `shared_fleet` are
each a `OnceLock<Arc<…>>`.  This gives:

- **Thread safety**: initialised once, read-only after that.
- **Zero-cost subsequent reads**: `Arc::clone` on an already-set `OnceLock` is
  just an atomic increment.
- **No global `Mutex`**: eliminates a class of deadlocks that came up when the
  previous design used `RwLock`.

### Router isolation (`ROUTER_LLM_PROVIDER`)

When the main backend is a cloud service (Gemini, Anthropic), intent
classification calls would otherwise incur API costs and network latency on every
message.  `ROUTER_LLM_PROVIDER` lets operators keep routing calls on a local
Ollama/LM Studio instance while main responses flow through the cloud backend.

### 429 retry with exponential back-off

Both `call_openai_messages` and `stream_openai` retry up to 3 times on HTTP 429,
honouring `Retry-After` when present, defaulting to 30 s → 60 s → 120 s.  This
is enough to ride out Gemini Flash's burst limits without manual intervention.

### Ollama `keep_alive: -1` for the router model

`call_router_prompt` passes `keep_alive: json!(-1)` to Ollama so the small
routing model stays resident in VRAM between calls.  Without this, Ollama unloads
the model after 5 minutes of inactivity and the next router call pays a 1–3 s
reload penalty.

---

## 7. Known Limits / Future Work

| Area | Current state | Possible improvement |
|------|--------------|----------------------|
| **Streaming multi-turn** | `call_prompt_stream` only accepts a single prompt string | Extend `stream_openai` to accept `Vec<OpenAiMessage>` |
| **Vision model auto-selection** | Vision-capable models are classified but never auto-promoted to any role slot | Add a `vision` role to `AgentFleet`; auto-select when screenshot analysis is requested |
| **Ollama multi-turn** | `call_prompt_messages` serialises the full history as `ROLE: content\n\n` flat string | Use Ollama's `/api/chat` endpoint for native multi-turn support |
| **Dynamic reconfiguration** | Changing `llm.yaml` or env vars requires a process restart | Add a `reload_llm_config()` that clears the `OnceLock` singletons (requires `OnceLock` → `RwLock` swap) |
| **Embedding support** | `ModelCapability::Embedding` is detected and logged but no embedding call function exists | Add `call_embed(text) → Vec<f32>` backed by Ollama's `/api/embeddings` |
| **Token counting** | No pre-call token budget estimation | Integrate a tokeniser (e.g., `tiktoken-rs`) to warn when prompts approach context limits |
| **Cloud remote `list_models`** | Gemini/Anthropic backends return empty model lists from `list_lmstudio_models` (API shape differs) | Call provider-specific model-list endpoints; populate classified models for cloud backends |
