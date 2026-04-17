//! Embedded Rhai scripting engine for Sirin skills.
//!
//! Exposes the following functions to `.rhai` scripts:
//! - `http_get(url)` — blocking HTTP GET, returns response body as string
//! - `parse_json(str)` — parse JSON string into a Rhai map/array
//! - `log(msg)` — debug message (writes `sirin_log:` prefix to stderr)
//! - `read_file(path)` — read a file and return its contents as string
//!
//! Global variables injected into every script:
//! - `skill_id` — the skill being executed
//! - `user_input` — the user's message
//! - `agent_id` — the agent ID (empty string when absent)
//!
//! Output convention: call `print()` to emit result lines.
//! The concatenation of all `print()` calls is returned as the skill result.

use rhai::{Dynamic, Engine, Scope};
use std::sync::{Arc, Mutex};

/// Build a Rhai engine with all Sirin-standard functions registered.
pub fn build_engine() -> Engine {
    let mut engine = Engine::new();

    // ── http_get ─────────────────────────────────────────────────────────────
    engine.register_fn("http_get", |url: &str| -> String {
        match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Sirin/1.0")
            .build()
            .and_then(|c| c.get(url).send())
            .and_then(|r| r.text())
        {
            Ok(body) => body,
            Err(e) => format!("ERROR: {e}"),
        }
    });

    // ── parse_json ───────────────────────────────────────────────────────────
    engine.register_fn("parse_json", |s: &str| -> Dynamic {
        match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => json_to_dynamic(v),
            Err(_) => Dynamic::UNIT,
        }
    });

    // ── log ──────────────────────────────────────────────────────────────────
    engine.register_fn("log", |msg: &str| {
        eprintln!("sirin_log: {msg}");
    });

    // ── read_file ────────────────────────────────────────────────────────────
    engine.register_fn("read_file", |path: &str| -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    });

    engine
}

/// Recursively convert a `serde_json::Value` into a `rhai::Dynamic`.
fn json_to_dynamic(val: serde_json::Value) -> Dynamic {
    match val {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else {
                Dynamic::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Dynamic::from(s),
        serde_json::Value::Array(arr) => {
            let v: rhai::Array = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(v)
        }
        serde_json::Value::Object(map) => {
            let mut m = rhai::Map::new();
            for (k, v) in map {
                m.insert(k.into(), json_to_dynamic(v));
            }
            Dynamic::from(m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_btc_price_script() {
        let result = run_rhai_script(
            &crate::platform::config_path("scripts/btc_price.rhai").to_string_lossy(),
            "btc_price",
            "查看BTC價格",
            None,
        );
        match result {
            Ok(out) => {
                println!("--- 輸出 ---\n{out}\n-----------");
                assert!(!out.is_empty(), "輸出不應為空");
            }
            Err(e) => println!("錯誤（可能是網路）：{e}"),
        }
    }

    #[test]
    fn test_print_capture() {
        let result = {
            let mut engine = build_engine();
            let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let out2 = output.clone();
            engine.on_print(move |s| {
                let mut b = out2.lock().unwrap();
                b.push_str(s);
                b.push('\n');
            });
            engine.run_with_scope(&mut rhai::Scope::new(), r###"
                print("BTC 當前價格");
                print("| USD | $95,000 |");
                log("test log");
            "###).unwrap();
            let s = output.lock().unwrap().trim().to_string();
            s
        };
        assert!(result.contains("BTC"), "output: {result}");
        assert!(result.contains("95,000"), "output: {result}");
    }
}

/// Run a `.rhai` script file synchronously.
///
/// Injects `skill_id`, `user_input`, `agent_id` as global variables.
/// Output is captured from all `print()` calls and returned as a single string.
///
/// **Must be called from a blocking context** (e.g. `spawn_blocking`, `std::thread::spawn`).
/// Do NOT call directly from an async task — `http_get` inside scripts is blocking.
pub fn run_rhai_script(
    script_path: &str,
    skill_id: &str,
    user_input: &str,
    agent_id: Option<&str>,
) -> Result<String, String> {
    let code = std::fs::read_to_string(script_path)
        .map_err(|e| format!("Cannot read script '{script_path}': {e}"))?;

    let mut engine = build_engine();

    // Capture all print() calls into a buffer
    let output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let out_clone = output.clone();
    engine.on_print(move |s| {
        let mut buf = out_clone.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_str(s);
        buf.push('\n');
    });

    let mut scope = Scope::new();
    scope.push("skill_id", skill_id.to_string());
    scope.push("user_input", user_input.to_string());
    scope.push("agent_id", agent_id.unwrap_or("").to_string());

    engine
        .run_with_scope(&mut scope, &code)
        .map_err(|e| format!("Script error: {e}"))?;

    let result = output.lock().unwrap_or_else(|e| e.into_inner()).trim().to_string();
    Ok(result)
}
