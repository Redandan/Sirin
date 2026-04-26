//! Action-annotated GIF recorder (Issue #78).
//!
//! Captures one frame per ReAct iteration and, when the test fails, encodes
//! the buffered frames into `test_failures/<run_id>/timeline.gif`.  Each frame
//! gets a label `Step N: <action> <target>` rendered with an embedded 5x7
//! ASCII bitmap font (no external font/imageproc deps — keeps build cost low).
//!
//! - **Privacy mask compatible**: frames come from `crate::browser::screenshot()`
//!   which already runs the Issue #80 CSS mask injection.  We never bypass it.
//! - **OOM guard**: in-memory ring buffer capped at `MAX_FRAMES` (60) — oldest
//!   frame is evicted when the buffer fills.
//! - **Soft-fail**: encode errors are logged at warn-level, never bubble up
//!   into the triage path.
//! - **Opt-in via `TestGoal.record_timeline_gif`** (default `true`); set to
//!   `false` to skip per-step capture entirely.
//!
//! Single-frame failure screenshot (the legacy `screenshot_path` field of
//! `TestResult`) is preserved unchanged for back-compat.

use image::{Rgba, RgbaImage};
use image::codecs::gif::{GifEncoder, Repeat};
use image::Frame as ImgFrame;
use image::Delay;
use std::path::PathBuf;
use std::time::Instant;

const MAX_FRAMES: usize = 60;
const FRAME_DELAY_MS: u32 = 800;          // ~1.25 fps — slow enough to read labels
const LABEL_BG: Rgba<u8>   = Rgba([26, 26, 26, 230]);   // #1A1A1A
const LABEL_TEXT: Rgba<u8> = Rgba([0, 255, 163, 255]);  // #00FFA3 — Sirin accent

/// One captured frame: raw screenshot PNG bytes + the action that produced it.
#[derive(Debug, Clone)]
pub struct TimelineFrame {
    pub step: u32,
    pub action: String,
    pub target: String,
    pub png_bytes: Vec<u8>,
}

/// Bounded ring buffer.  When full, evicts oldest entry.
#[derive(Debug, Default)]
pub struct TimelineBuffer {
    frames: std::collections::VecDeque<TimelineFrame>,
}

impl TimelineBuffer {
    pub fn new() -> Self { Self { frames: std::collections::VecDeque::with_capacity(MAX_FRAMES) } }

    pub fn push(&mut self, f: TimelineFrame) {
        if self.frames.len() >= MAX_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back(f);
    }

    pub fn len(&self) -> usize { self.frames.len() }
    pub fn is_empty(&self) -> bool { self.frames.is_empty() }

    /// Encode buffered frames into a GIF at `out_path`.  On any I/O or codec
    /// error returns Err(String); caller must log + continue (never abort
    /// triage).  Has an internal soft 5-second budget — if encoding overruns
    /// it logs a warning but still finishes (interrupting halfway would leave
    /// a corrupt file).
    pub fn encode_to_gif(&self, out_path: &PathBuf) -> Result<(), String> {
        if self.frames.is_empty() {
            return Err("no frames to encode".into());
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        }
        let file = std::fs::File::create(out_path)
            .map_err(|e| format!("create {out_path:?}: {e}"))?;
        let mut enc = GifEncoder::new_with_speed(file, 30);  // 1=best, 30=fastest
        enc.set_repeat(Repeat::Infinite)
            .map_err(|e| format!("set_repeat: {e}"))?;

        let started = Instant::now();
        for f in &self.frames {
            // Decode PNG → RGBA8.  If decode fails, skip this frame rather
            // than abort — earlier frames are still useful for debugging.
            let img = match image::load_from_memory(&f.png_bytes) {
                Ok(i) => i.to_rgba8(),
                Err(e) => {
                    tracing::warn!("[gif_recorder] step {} png decode failed: {e}", f.step);
                    continue;
                }
            };
            let labelled = annotate(img, f.step, &f.action, &f.target);
            let frame = ImgFrame::from_parts(
                labelled,
                0, 0,
                Delay::from_numer_denom_ms(FRAME_DELAY_MS, 1),
            );
            enc.encode_frame(frame)
                .map_err(|e| format!("encode_frame step {}: {e}", f.step))?;
        }

        let elapsed = started.elapsed();
        if elapsed.as_secs() > 5 {
            tracing::warn!(
                "[gif_recorder] GIF encode took {}ms ({} frames) — slow but finished",
                elapsed.as_millis(), self.frames.len()
            );
        }
        Ok(())
    }
}

/// Render a label band along the top of `img` showing `Step N: <action> <target>`.
/// Truncates target to fit width.  Pure pixel-level: no font crate.
fn annotate(mut img: RgbaImage, step: u32, action: &str, target: &str) -> RgbaImage {
    let (w, _h) = (img.width(), img.height());
    let label = format!("Step {step}: {action} {target}");
    // Allow ~10 px per char with our 6-px-wide font + 1 spacing.
    let max_chars = ((w as i32 - 16) / 7).max(8) as usize;
    let label = if label.chars().count() > max_chars {
        let head: String = label.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{head}...")
    } else {
        label
    };

    let band_h: u32 = 22;
    let band_y: u32 = 6;
    let band_x: u32 = 6;
    let band_w: u32 = w.saturating_sub(12);

    // Filled background rectangle with rounded-ish margin.
    for y in band_y..(band_y + band_h).min(img.height()) {
        for x in band_x..(band_x + band_w).min(img.width()) {
            img.put_pixel(x, y, LABEL_BG);
        }
    }

    // Draw text: 5x7 glyphs scaled ×2 → 10×14 px each.
    let text_x = band_x + 4;
    let text_y = band_y + 4;
    draw_text(&mut img, text_x as i32, text_y as i32, &label, LABEL_TEXT);
    img
}

/// Pixel-level text drawing using an embedded 5x7 ASCII bitmap font, scale ×2.
/// Unsupported chars render as a hollow box.
fn draw_text(img: &mut RgbaImage, x: i32, y: i32, text: &str, color: Rgba<u8>) {
    let scale: i32 = 2;
    let mut cursor = x;
    for ch in text.chars() {
        let glyph = font5x7(ch);
        for (row_idx, row) in glyph.iter().enumerate() {
            for col in 0..5 {
                if row & (1 << (4 - col)) != 0 {
                    // scaled square block
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = cursor + col * scale + dx;
                            let py = y + row_idx as i32 * scale + dy;
                            if px >= 0 && py >= 0
                                && (px as u32) < img.width()
                                && (py as u32) < img.height()
                            {
                                img.put_pixel(px as u32, py as u32, color);
                            }
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;  // 5px glyph + 1px gap
    }
}

/// Minimal 5x7 ASCII font (each glyph = 7 rows; bits 4..0 of each byte are the
/// 5 columns left→right).  Supports printable ASCII; unknown chars → solid box.
/// Source: hand-rolled subset; covers letters, digits, common punctuation.
fn font5x7(ch: char) -> [u8; 7] {
    match ch {
        ' ' => [0,0,0,0,0,0,0],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => [0,0b00100,0,0,0,0b00100,0],
        '.' => [0,0,0,0,0,0b00100,0b00100],
        ',' => [0,0,0,0,0,0b00100,0b01000],
        '-' => [0,0,0,0b01110,0,0,0],
        '_' => [0,0,0,0,0,0,0b11111],
        '/' => [0b00001,0b00010,0b00010,0b00100,0b01000,0b01000,0b10000],
        '#' => [0b01010,0b01010,0b11111,0b01010,0b11111,0b01010,0b01010],
        '(' => [0b00010,0b00100,0b01000,0b01000,0b01000,0b00100,0b00010],
        ')' => [0b01000,0b00100,0b00010,0b00010,0b00010,0b00100,0b01000],
        '<' => [0b00010,0b00100,0b01000,0b10000,0b01000,0b00100,0b00010],
        '>' => [0b01000,0b00100,0b00010,0b00001,0b00010,0b00100,0b01000],
        '=' => [0,0,0b11111,0,0b11111,0,0],
        '!' => [0b00100,0b00100,0b00100,0b00100,0b00100,0,0b00100],
        '?' => [0b01110,0b10001,0b00001,0b00010,0b00100,0,0b00100],
        '\'' => [0b00100,0b00100,0,0,0,0,0],
        '"' => [0b01010,0b01010,0,0,0,0,0],
        'A'|'a' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B'|'b' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C'|'c' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D'|'d' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E'|'e' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F'|'f' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G'|'g' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H'|'h' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I'|'i' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J'|'j' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K'|'k' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L'|'l' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M'|'m' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N'|'n' => [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
        'O'|'o' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P'|'p' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q'|'q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R'|'r' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S'|'s' => [0b01110, 0b10001, 0b10000, 0b01110, 0b00001, 0b10001, 0b01110],
        'T'|'t' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U'|'u' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V'|'v' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W'|'w' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X'|'x' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y'|'y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z'|'z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        _ => [0b11111,0b10001,0b10001,0b10001,0b10001,0b10001,0b11111],  // hollow box
    }
}

/// Path layout for the GIF artefact: `<app_data>/test_failures/<run_id>/timeline.gif`.
pub fn timeline_gif_path(run_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("test_failures")
        .join(run_id)
        .join("timeline.gif")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([10, 10, 10, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn ring_buffer_overflow_evicts_oldest() {
        let mut buf = TimelineBuffer::new();
        for i in 0..(MAX_FRAMES + 5) {
            buf.push(TimelineFrame {
                step: i as u32,
                action: "click".into(),
                target: format!("#btn-{i}"),
                png_bytes: vec![0u8; 10],
            });
        }
        assert_eq!(buf.len(), MAX_FRAMES, "ring must cap at MAX_FRAMES");
        // Oldest should be step=5 (since we pushed MAX_FRAMES+5 and evicted 5)
        let first = &buf.frames[0];
        assert_eq!(first.step, 5, "oldest 5 entries must have been evicted");
    }

    #[test]
    fn encode_round_trip_produces_gif_file() {
        let mut buf = TimelineBuffer::new();
        for i in 0..3 {
            buf.push(TimelineFrame {
                step: i,
                action: "goto".into(),
                target: format!("https://example.com/{i}"),
                png_bytes: dummy_png(80, 60),
            });
        }
        let dir = std::env::temp_dir().join(format!("sirin-gif-test-{}", std::process::id()));
        let out = dir.join("timeline.gif");
        buf.encode_to_gif(&out).expect("encode ok");
        let meta = std::fs::metadata(&out).expect("file exists");
        // GIF87a/89a header is 6 bytes; a 3-frame valid GIF will be well > 100 bytes.
        assert!(meta.len() > 100, "gif file size {} must be > 100", meta.len());
        // Verify it actually starts with the GIF magic.
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..6], b"GIF89a", "must have GIF89a header");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_buffer_encode_errors() {
        let buf = TimelineBuffer::new();
        let out = std::env::temp_dir().join("sirin-gif-empty.gif");
        assert!(buf.encode_to_gif(&out).is_err());
    }

    #[test]
    fn timeline_path_includes_run_id() {
        let p = timeline_gif_path("run_123");
        assert!(p.to_string_lossy().contains("run_123"));
        assert!(p.to_string_lossy().ends_with("timeline.gif"));
    }
}
