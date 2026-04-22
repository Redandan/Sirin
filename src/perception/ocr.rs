//! Local Windows-OCR text locator — a cheap, token-free alternative to the
//! vision LLM for finding text on the current page.  Used by the
//! `browser_exec ocr_find_text` MCP action and available to future locator
//! chains as a fallback when the LLM budget is tight.
//!
//! Shells out to `scripts/ocr_windows_find_text.ps1` which calls the WinRT
//! OCR engine and prints a JSON result.  Windows-only for now.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Find target text on the current browser screenshot using local Windows OCR.
///
/// Returns raw OCR JSON, including any matched bounding boxes and center points.
pub fn find_text_on_current_page(needle: &str, max_results: usize) -> Result<Value, String> {
    if needle.trim().is_empty() {
        return Err("'needle' cannot be empty".to_string());
    }

    let png = crate::browser::screenshot()?;
    let tmp_dir = crate::platform::app_data_dir().join("tmp").join("perception_ocr");
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("create tmp dir failed: {e}"))?;

    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
    let png_path = tmp_dir.join(format!("ocr_input_{stamp}.png"));
    std::fs::write(&png_path, &png)
        .map_err(|e| format!("write screenshot failed: {e}"))?;

    let script_path = resolve_script_path()?;
    let output = Command::new(resolve_powershell_exe())
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .arg("-ImagePath")
        .arg(&png_path)
        .arg("-Needle")
        .arg(needle)
        .arg("-MaxResults")
        .arg(max_results.to_string())
        .output()
        .map_err(|e| format!("launch powershell OCR failed: {e}"))?;

    let stdout = decode_powershell_text(&output.stdout);
    let stderr = decode_powershell_text(&output.stderr);

    if !output.status.success() {
        return Err(format!(
            "ocr command failed (status={}): stderr={} stdout={}",
            output.status,
            stderr.trim(),
            stdout.trim()
        ));
    }

    let json_line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| "ocr output was empty".to_string())?;

    let mut payload: Value = serde_json::from_str(json_line)
        .map_err(|e| format!("parse ocr json failed: {e}; raw={json_line}"))?;

    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "poc".to_string(),
            json!({
                "provider": "windows_local_ocr",
                "image_path": png_path.to_string_lossy(),
                "script_path": script_path.to_string_lossy(),
                "needle": needle,
                "max_results": max_results,
            }),
        );
        if !stderr.trim().is_empty() {
            obj.insert("stderr".to_string(), json!(stderr.trim()));
        }
    }

    Ok(payload)
}

fn resolve_powershell_exe() -> String {
    let win_ps = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    if std::path::Path::new(win_ps).exists() {
        return win_ps.to_string();
    }
    "powershell".to_string()
}

fn decode_powershell_text(bytes: &[u8]) -> String {
    if let Ok(s) = String::from_utf8(bytes.to_vec()) {
        return s;
    }

    if bytes.len() >= 2 {
        let mut u16s = Vec::with_capacity(bytes.len() / 2);
        let mut i = 0;
        while i + 1 < bytes.len() {
            u16s.push(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
            i += 2;
        }
        if let Ok(s) = String::from_utf16(&u16s) {
            return s;
        }
    }

    String::from_utf8_lossy(bytes).to_string()
}

fn resolve_script_path() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("scripts").join("ocr_windows_find_text.ps1"));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("scripts").join("ocr_windows_find_text.ps1"));
            candidates.push(exe_dir.join("..").join("..").join("scripts").join("ocr_windows_find_text.ps1"));
        }
    }

    candidates
        .into_iter()
        .find(|p| Path::new(p).exists())
        .ok_or_else(|| "cannot find scripts/ocr_windows_find_text.ps1".to_string())
}
