# Telegram Subsystem — Architecture Overview

> Source: `src/telegram/` (2 200 lines across 8 modules)  
> Related: [`docs/MCP_API.md`](../MCP_API.md) · [`docs/ENV_REFERENCE.md`](../ENV_REFERENCE.md) · [`docs/ARCHITECTURE.md`](../ARCHITECTURE.md)

---

## 1. Purpose

The Telegram subsystem connects Sirin to the Telegram MTProto API using the
[`grammers`](https://crates.io/crates/grammers-client) crate. It monitors a
configurable set of group chats and private DMs, routes every eligible message
through Sirin's agent pipeline (planner → router → chat/coding/research), and
sends back a streaming or single-shot LLM reply. Two concurrency paths exist:
a **legacy single-agent listener** (one phone number / one session) and a
**per-agent listener** (multiple agents each with their own session and persona,
declared in `config/agents.yaml`). Authentication is fully UI-driven — no
terminal stdin required — making it safe inside the egui GUI process.

---

## 2. Module Map

| File | Lines | Responsibility |
|------|------:|----------------|
| `mod.rs` | 788 | Entry points (`run_listener`, `run_agent_listener`), MTProto connection loop, auth flow, update dispatch |
| `config.rs` | 251 | `TelegramConfig` struct, env-var parser (`from_env`), per-agent channel parser (`from_agent_channel`), `${VAR}` resolver |
| `filter.rs` | 81 | `filter_message()` — 7-rule gate that decides Skip vs Handle for each incoming update |
| `handler.rs` | 122 | `prepare_reply_plan()` — routes message through planner/router, builds `ReplyPlan` |
| `reply.rs` | 215 | Streaming LLM reply via progressive Telegram message edits; fallback single-shot send |
| `commands.rs` | 438 | Built-in command dispatch (todo CRUD, task queries, research intent detection) |
| `language.rs` | 145 | CJK detection, mixed-language heuristics, Chinese fallback generation |
| `llm.rs` | 160 | `build_ai_reply_prompt()` — assembles the full prompt from persona + context blocks |

---

## 3. Data Flow

### 3a. Single-agent path (`run_listener_once`)

```
Telegram MTProto network
        │  Update event (new message)
        ▼
 run_listener_once()           mod.rs:126
        │
        ├── filter_message()   filter.rs
        │       7 rules →  Skip(reason) or Handle { text, is_private, peer_bare_id }
        │
        ├─[Handle]──► prepare_reply_plan()   handler.rs
        │                   │
        │                   ├── commands::execute_user_request()    commands.rs
        │                   │       (todo / task / research commands)
        │                   │
        │                   └── agents::router_agent::run_router_via_adk()
        │                               │  LLM route decision
        │                               └─► Chat / Coding / Research agent
        │
        └─[ReplyPlan ready]──► send_streaming_reply() / send_final_reply()  reply.rs
                                        │
                                        └─► Telegram message edit (streaming)
                                            or single-shot send
```

### 3b. Per-agent path (`run_agent_listener_once`)

Same pipeline but each agent declaration in `config/agents.yaml` provides its
own `TelegramChannelConfig` → separate `TelegramConfig`, separate
`SqliteSession`, separate `Persona`, separate disabled-skill list, and a
per-agent `agent_id` threaded into memory isolation calls.

```
config/agents.yaml
  └── agents[N].channels.telegram
          │
          ▼
TelegramConfig::from_agent_channel()   config.rs:216
          │
          ▼
  run_agent_listener_once()            mod.rs:382
          │  (same filter → handler → reply chain as above,
          │   but with agent_id + agent_disabled_skills injected)
          ▼
  per-agent memory context             memory::context::append_context(agent_id)
```

### 3c. Authentication flow

```
  Sirin startup
       │
       ▼
  SqliteSession::open(session_path())   ← data/sirin.session (persisted)
       │
       ▼
  client.is_authorized()?
       ├── yes ──► skip auth, proceed to update loop
       └── no
              │
              ├─[TG_REQUIRE_LOGIN=0]──► set_disconnected(); listener exits
              │                         (retry loop will try again later)
              │
              └─[TG_REQUIRE_LOGIN=1]
                     │
                     ▼
             client.request_login_code(phone)
                     │
                     ▼
             auth.request_code(300s timeout)  ← UI provides code via TelegramAuthState
                     │
                     ├─[no 2FA]──► client.sign_in(code)
                     │
                     └─[2FA]──────► auth.request_password(hint, 300s)
                                         └─► client.check_password(token, pwd)
```

---

## 4. Configuration

### 4a. Environment variables (`.env` or system env)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `TG_API_ID` | ✅ | — | Integer App API ID from https://my.telegram.org |
| `TG_API_HASH` | ✅ | — | Hex App API hash from https://my.telegram.org |
| `TG_PHONE` | for auth | — | Phone number for non-interactive sign-in (e.g. `+886912345678`) |
| `TG_REQUIRE_LOGIN` | ❌ | `false` | Set `1` to abort startup until auth succeeds |
| `TG_AUTO_REPLY` | ❌ | `false` | Enable AI auto-reply (`1`/`true`/`yes`/`on`) |
| `TG_AUTO_REPLY_TEXT` | ❌ | `{ack_prefix} 我會先幫你處理這件事。` | Ack message template |
| `TG_REPLY_PRIVATE` | ❌ | `true` | Reply to private DMs |
| `TG_REPLY_GROUPS` | ❌ | `false` | Reply to group/channel messages |
| `TG_GROUP_IDS` | ❌ | `""` | Comma-separated group/channel IDs to monitor; empty = all |
| `TG_STARTUP_MSG` | ❌ | `Sirin started at {time}` | Message sent to self on startup; empty = disabled |
| `TG_STARTUP_TARGET` | ❌ | — | Username to receive startup message (e.g. `myuser`) |
| `TG_DEBUG_UPDATES` | ❌ | `true` | Verbose update diagnostics in logs |

### 4b. Per-agent channel config (`config/agents.yaml`)

Each agent entry under `agents[N].channels.telegram` maps to
`TelegramChannelConfig` in `src/agent_config.rs`. Fields support `${VAR}`
placeholders resolved at startup via `config::resolve_env_refs()`.

```yaml
agents:
  - id: assistant1
    name: 助手1
    channels:
      telegram:
        api_id: "${TG_API_ID}"
        api_hash: "${TG_API_HASH}"
        phone: "${TG_PHONE}"
        auto_reply: true
        reply_private: true
        reply_groups: false
        group_ids: []
        startup_msg: "助手1 上線"
        session_path: ""   # empty → data/sirin.session (default)
```

### 4c. Session file

`data/sirin.session` (resolved via `platform::app_data_dir().join("sirin.session")`).  
SQLite format managed by `grammers_session::storages::SqliteSession`.  
Persists across restarts — no re-auth needed unless session expires.

### 4d. Persona influence

`Persona::cached()` is read in `llm::build_ai_reply_prompt()` to inject:
- `persona_name` → system prompt greeting
- `response_style.voice` → tone instruction
- `response_style.compliance_line` → instruction-following style
- Per-agent: `agent_disabled_skills` list gates which router branches are available

---

## 5. Notable Design Decisions

| Decision | Alternative considered | Reason |
|----------|------------------------|--------|
| **MTProto via `grammers`** (not Telegram Bot API) | Bot API (`teloxide`) | MTProto enables user-account listening — can monitor groups without bot admin rights; real-time vs polling |
| **UI-driven auth** (not stdin) | `stdin().read_line()` | egui GUI process has no terminal; stdin blocks the async runtime; UI channel with 300 s timeout keeps the app responsive |
| **`SqliteSession` storage** | File-based session | Atomic writes, no corruption on crash; survives Sirin restart without re-auth |
| **Legacy + per-agent dual paths** | Single unified path | Backward compat — existing single-phone setups continue to work; per-agent path adds isolation without a breaking rename |
| **`catch_up: false` on update stream** | Default `true` | Prevents bulk auto-replies to backlog messages on reconnect; combined with `listener_started_at` timestamp check in `filter.rs` |
| **`filter_message` as standalone fn** | Inline in loop | Shared between both listener paths; testable in isolation |
| **Streaming reply via progressive edit** | Sending new message per chunk | Looks like live typing; user sees response start within <1 s; single-message thread stays clean |
| **`${VAR}` resolver in config** | Direct env read | Allows agents.yaml to store references instead of credentials; safe to commit the config file |

---

## 6. Known Limits & Future Work

### Current limits

- **Single MTProto connection per agent** — if a user sends messages faster than the LLM replies, messages queue in memory (no backpressure). Under heavy load the listener can fall behind.
- **No message deduplication** — if Sirin crashes mid-reply and restarts, the same message may be processed again (mitigated by `listener_started_at` guard, but edge cases exist near the crash boundary).
- **`TG_DEBUG_UPDATES` defaults to `true`** — produces verbose logs even in production; a future PR should flip the default to `false`.
- **Streaming reply** (`send_streaming_reply`) is marked `#[allow(dead_code)]` and `STREAM_EDIT_EVERY_TOKENS` is `#[allow(dead_code)]` — the feature was built but is not wired into the main reply path. Currently `send_final_reply` is used exclusively.
- **No rate-limit handling** — MTProto `FLOOD_WAIT` errors from Telegram are not caught/retried; the listener will exit and the retry loop will reconnect.
- **2FA hint display** — the hint string from `password_token.hint()` is logged but not surfaced to the egui UI; users must check logs if they forget their 2FA hint.

### Planned / future work

- Wire `send_streaming_reply` into `reply.rs` as the default path (currently unused).
- Handle `FLOOD_WAIT` with exponential backoff instead of full reconnect.
- Surface Telegram 2FA password hint in the UI auth dialog (`TelegramAuthState`).
- Add per-peer rate limiting so one active chat can't starve others.
- Structured observability: expose per-message latency and reply token counts via the metrics subsystem.
