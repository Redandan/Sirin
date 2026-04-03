Integrate the web search capability into the Telegram message handling pipeline.

Goal: When a Telegram message arrives, the agent should be able to search the web and include results in the LLM prompt before generating a reply.

Current state:
- `skills::ddg_search(query)` exists in src/skills.rs but is never called from telegram.rs
- `generate_ai_reply()` in src/telegram.rs builds a static prompt with only persona + user_text

Implementation plan:
1. **Decide when to search**: Add a simple heuristic in `run_listener()` — if the message contains a question mark `?` or words like "什麼", "如何", "為什麼", "what", "how", "why", "when", trigger a search
2. **Extract search query**: Use the user message text directly as the search query (trimmed to 100 chars)
3. **Call `ddg_search()`**: This is an async function; call it with `ddg_search(&text).await`
4. **Inject results into prompt**: Modify `build_ai_reply_prompt()` in telegram.rs to accept an optional `search_results: Option<&str>` parameter, and append a `Web search results:` block to the prompt when present
5. **Format search results**: Take top 3 results, format as `- [title]: snippet (url)`

Files to modify:
- `src/skills.rs` — ensure `ddg_search` is `pub async fn` and fix any compile errors first
- `src/telegram.rs` — modify `build_ai_reply_prompt`, `generate_ai_reply`, and the main loop in `run_listener`

Read both files before making changes. Run `cargo check` after each file modification.
