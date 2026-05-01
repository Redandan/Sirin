//! sirin-call — thin CLI wrapper for the Sirin MCP API.
//!
//! Avoids shell-escaping pain (especially CJK/Unicode payloads in bash curl).
//!
//! # Usage
//!
//! ```
//! # Key=value syntax:
//! sirin-call browser_exec action=url
//! sirin-call browser_exec action=ax_find role=button name=登入
//! sirin-call browser_exec action=wait_for_url target="#/home"
//!
//! # Pipe stdin JSON (handles any Unicode, no shell escaping needed):
//! echo '{"action":"ax_find","role":"button","name":"購買"}' | sirin-call browser_exec
//!
//! # List tools:
//! sirin-call --list
//! ```
//!
//! Connects to `http://127.0.0.1:$SIRIN_RPC_PORT/mcp` (default port 7700).

use std::io::Read;

fn main() {
    // On Windows/Git-Bash, broken-pipe (downstream exits before sirin-call
    // finishes writing) does not send SIGPIPE but makes the write() syscall
    // fail with ERROR_BROKEN_PIPE.  Rust's default handler panics on broken-
    // pipe writes to stdout; instead we want to exit(0) silently so the
    // process does not linger as a zombie in ps.
    //
    // The idiomatic fix is to use `std::io::ErrorKind::BrokenPipe` on every
    // println!/writeln! failure, but the simplest cross-platform workaround
    // is to reset SIGPIPE to SIG_DFL on Unix and handle ERROR_BROKEN_PIPE on
    // Windows via the `signal-hook` / `ctrlc` approach.  For our lightweight
    // wrapper we just suppress the broken-pipe panic via a custom panic hook.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        if msg.contains("BrokenPipe") || msg.contains("broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print_usage();
        std::process::exit(0);
    }

    let port = std::env::var("SIRIN_RPC_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(7700);
    let endpoint = format!("http://127.0.0.1:{port}/mcp");

    // ── Special meta commands ─────────────────────────────────────────────────
    if args[0] == "--list" {
        let resp = mcp_call(&endpoint, "tools/list", serde_json::json!({}));
        match resp {
            Ok(v) => {
                if let Some(tools) = v["result"]["tools"].as_array() {
                    for t in tools {
                        let name = t["name"].as_str().unwrap_or("?");
                        let desc = t["description"].as_str().unwrap_or("");
                        let first_line = desc.lines().next().unwrap_or(desc);
                        println!("  {name:<26} {first_line}");
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                }
            }
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // ── Normal tool call ──────────────────────────────────────────────────────
    let tool_name = args[0].clone();

    let arguments: serde_json::Value = if args.len() == 1 {
        // No key=value args → try reading JSON from stdin
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        let input = input.trim();
        if input.is_empty() {
            serde_json::json!({})
        } else {
            match serde_json::from_str(input) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error: invalid JSON on stdin: {e}");
                    eprintln!("Input was: {input}");
                    std::process::exit(1);
                }
            }
        }
    } else {
        // Parse key=value pairs from CLI args
        let mut obj = serde_json::Map::new();
        for kv in &args[1..] {
            if let Some((k, v)) = kv.split_once('=') {
                // Try to parse value as JSON (numbers, booleans, arrays, objects);
                // fall back to plain string so callers don't need extra quoting.
                let val = serde_json::from_str::<serde_json::Value>(v)
                    .unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
                obj.insert(k.to_string(), val);
            } else {
                eprintln!("Warning: ignoring malformed argument (expected key=value): {kv}");
            }
        }

        // Stdin can supplement key=value args for complex nested fields.
        // Only read if there's actually something piped (non-interactive).
        let stdin_extra = String::new();
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            // Only read stdin if it's a pipe (not a terminal)
            if unsafe { libc_isatty(fd) } == 0 {
                let _ = std::io::stdin().read_to_string(&mut stdin_extra);
            }
        }
        let stdin_extra = stdin_extra.trim();
        if !stdin_extra.is_empty() {
            if let Ok(extra) = serde_json::from_str::<serde_json::Value>(stdin_extra) {
                if let Some(extra_obj) = extra.as_object() {
                    for (k, v) in extra_obj {
                        obj.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }

        serde_json::Value::Object(obj)
    };

    let params = serde_json::json!({
        "name": tool_name,
        "arguments": arguments,
    });

    match mcp_call(&endpoint, "tools/call", params) {
        Ok(response) => {
            // Extract text content if present, else print full response
            if let Some(content) = response["result"]["content"].as_array() {
                for item in content {
                    if let Some(text) = item["text"].as_str() {
                        // Try to pretty-print if it's valid JSON
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
                            println!("{}", serde_json::to_string_pretty(&parsed)
                                .unwrap_or_else(|_| text.to_string()));
                        } else {
                            println!("{text}");
                        }
                        return;
                    }
                }
            }
            // Fallback: print full response
            println!("{}", serde_json::to_string_pretty(&response)
                .unwrap_or_else(|_| response.to_string()));
        }
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Is Sirin running? Try: .\\scripts\\dev-relaunch.sh");
            std::process::exit(1);
        }
    }
}

// ── HTTP helper ───────────────────────────────────────────────────────────────

fn mcp_call(
    endpoint: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let resp = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .map_err(|e| format!("POST {endpoint}: {e}"))?;

    let text = resp.text().map_err(|e| format!("read response: {e}"))?;

    serde_json::from_str(&text).map_err(|e| format!("parse response JSON: {e}\nRaw: {text}"))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn print_usage() {
    eprintln!("sirin-call — Sirin MCP CLI wrapper");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  sirin-call <tool> [key=value ...]     # key=value syntax");
    eprintln!("  echo '<json>' | sirin-call <tool>     # stdin JSON (Unicode-safe)");
    eprintln!("  sirin-call --list                     # list available tools");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  sirin-call browser_exec action=url");
    eprintln!("  sirin-call browser_exec action=wait_for_url target=\"#/home\"");
    eprintln!("  sirin-call browser_exec action=ax_find role=button name=登入");
    eprintln!("  echo '{{\"action\":\"ax_find\",\"role\":\"button\",\"name\":\"購買\"}}' | sirin-call browser_exec");
    eprintln!();
    eprintln!("Env:");
    eprintln!("  SIRIN_RPC_PORT  MCP server port (default 7700)");
}

// isatty is only used on Unix for the stdin-pipe detection.
// On Windows we skip that path entirely since the #[cfg(unix)] block is absent.
#[cfg(unix)]
extern "C" {
    fn isatty(fd: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: std::os::raw::c_int) -> std::os::raw::c_int {
    isatty(fd)
}
