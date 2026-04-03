Fix the compilation errors in src/skills.rs.

The current errors are:
- `no method named 'query' found for struct 'RequestBuilder'` at src/skills.rs:26
- Type annotation errors at src/skills.rs:24

Root cause: The `ddg_search` function uses `reqwest::Client` but the client variable type cannot be inferred, and `.query()` is not available on the builder in this context.

Fix approach:
1. Read src/skills.rs to see the current code
2. In `ddg_search()`, the `reqwest::Client::new()` should be typed explicitly as `reqwest::Client`
3. The `.query()` method requires the `reqwest` crate with the correct feature — replace with manual URL construction using `format!("https://duckduckgo.com/html/?q={}", urlencoding::encode(query))` OR add `let client: reqwest::Client = reqwest::Client::new();` with explicit type annotation
4. Check if `urlencoding` crate is in Cargo.toml; if not, use `percent_encoding` or manually encode the query string
5. After fixing, run `cargo check 2>&1` to confirm the fix works

Make the minimal change necessary to fix the compile errors without refactoring the function's behavior.
