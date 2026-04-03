Help the user start the Sirin development environment.

The project requires two processes running simultaneously:
1. **Frontend** (Next.js): `npm run dev` in the project root — runs on http://localhost:3000
2. **Tauri dev** (Rust + WebView): `npm run tauri dev` in the project root — compiles Rust and opens the desktop window

Steps:
1. Check if `.env` file exists at the project root. If not, warn the user that Telegram listener will silently fail without `TG_API_ID`, `TG_API_HASH`, `TG_GROUP_IDS`.
2. Check if Ollama is running: run `curl -s http://localhost:11434/api/tags` and confirm the model `llama3.2` (or the one set in `OLLAMA_MODEL`) is available.
3. Run `cargo check 2>&1` first to verify there are no compile errors.
4. If all checks pass, instruct the user to run `npm run tauri dev` in a terminal.

Do NOT start processes automatically — just verify the prerequisites and give clear instructions.
