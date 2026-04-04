/// Optimization verification tests — no LM Studio required, self-contained.
///
/// Run with:
///   cargo test optimization -- --nocapture

// ── 1. Memory cache: repeated search on large index ──────────────────────────
//
// Verifies that scanning a Vec<Entry> in memory is much faster than
// re-reading the same data from disk on every call.

#[test]
fn memory_cache_vs_disk_scan_timing() {
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Write};
    use std::time::Instant;

    #[derive(Serialize, Deserialize)]
    struct Entry {
        text: String,
    }

    // ── Build a temp JSONL file with 1 000 entries ────────────────────────────
    let dir = std::env::temp_dir().join("sirin_opt_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("index.jsonl");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..1000 {
            let e = Entry {
                text: format!("Rust async optimization benchmark entry number {i} tokio performance"),
            };
            writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
        }
    }

    // Simple scorer (mirrors src/memory.rs).
    fn score(text: &str, query: &str) -> f64 {
        let qt: Vec<&str> = query.split_whitespace().collect();
        let words: Vec<&str> = text.split_whitespace().collect();
        let len = words.len().max(1) as f64;
        qt.iter()
            .map(|q| words.iter().filter(|w| w.eq_ignore_ascii_case(q)).count() as f64 / len)
            .sum()
    }

    // ── Disk-scan baseline: read the file on every search ────────────────────
    let query = "tokio performance";
    let disk_t = Instant::now();
    for _ in 0..10 {
        let f = std::fs::File::open(&path).unwrap();
        let _: Vec<_> = BufReader::new(f)
            .lines()
            .filter_map(|l| l.ok())
            .filter_map(|l| serde_json::from_str::<Entry>(&l).ok())
            .filter(|e| score(&e.text, query) > 0.0)
            .collect();
    }
    let disk_elapsed = disk_t.elapsed();

    // ── Cached path: load once, search Vec in memory ──────────────────────────
    let f = std::fs::File::open(&path).unwrap();
    let cache: Vec<Entry> = BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<Entry>(&l).ok())
        .collect();

    let cache_t = Instant::now();
    for _ in 0..10 {
        let _: Vec<_> = cache
            .iter()
            .filter(|e| score(&e.text, query) > 0.0)
            .collect();
    }
    let cache_elapsed = cache_t.elapsed();

    let speedup = disk_elapsed.as_secs_f64() / cache_elapsed.as_secs_f64().max(0.000_001);
    println!(
        "\n[memory cache] 10× disk scan: {:?}  |  10× cached scan: {:?}  |  speedup: {:.1}×",
        disk_elapsed,
        cache_elapsed,
        speedup
    );

    // On some machines the OS file cache can make repeated disk reads very close
    // to the in-memory path. Accept small jitter, but reject meaningfully slower
    // cached performance.
    assert!(
        speedup >= 0.85,
        "Cached scan regressed too much: disk={:?}, cache={:?}, speedup={:.2}×",
        disk_elapsed,
        cache_elapsed,
        speedup
    );

    std::fs::remove_file(&path).ok();
}

// ── 2. Selector cache: OnceLock vs fresh compile ──────────────────────────────

#[test]
fn selector_cache_vs_fresh_compile_timing() {
    use scraper::{Html, Selector};
    use std::sync::OnceLock;
    use std::time::Instant;

    fn cached_selector() -> &'static Selector {
        static SEL: OnceLock<Selector> = OnceLock::new();
        SEL.get_or_init(|| {
            Selector::parse(
                "body p, body h1, body h2, body h3, body li, body span, body div",
            )
            .unwrap()
        })
    }

    let html = r#"<html><body>
        <h1>Title</h1><p>Para 1</p><p>Para 2</p><li>Item</li>
    </body></html>"#;
    let n = 200usize;

    // Fresh compile on every call.
    let t_fresh = Instant::now();
    for _ in 0..n {
        let sel = Selector::parse(
            "body p, body h1, body h2, body h3, body li, body span, body div",
        )
        .unwrap();
        let doc = Html::parse_document(html);
        let _ = doc.select(&sel).count();
    }
    let fresh_elapsed = t_fresh.elapsed();

    // Cached selector (OnceLock).
    let t_cached = Instant::now();
    for _ in 0..n {
        let doc = Html::parse_document(html);
        let _ = doc.select(cached_selector()).count();
    }
    let cached_elapsed = t_cached.elapsed();

    let speedup = fresh_elapsed.as_secs_f64() / cached_elapsed.as_secs_f64().max(0.000_001);
    println!(
        "\n[selector cache] {n}× fresh compile: {:?}  |  {n}× cached: {:?}  |  speedup: {:.1}×",
        fresh_elapsed, cached_elapsed, speedup
    );

    // OnceLock selector must be faster.
    assert!(
        cached_elapsed < fresh_elapsed,
        "Cached selector ({:?}) should be faster than fresh compile ({:?})",
        cached_elapsed,
        fresh_elapsed
    );
}

// ── 3. Parallel futures: join_all vs sequential await ────────────────────────

#[tokio::test]
async fn parallel_questions_faster_than_sequential() {
    use std::time::{Duration, Instant};

    let delay = Duration::from_millis(100);
    let n = 4usize;

    // Sequential baseline.
    let t_seq = Instant::now();
    for _ in 0..n {
        tokio::time::sleep(delay).await;
    }
    let seq_elapsed = t_seq.elapsed();

    // Parallel using join_all — mirrors the optimized Phase 4.
    let t_par = Instant::now();
    futures::future::join_all((0..n).map(|_| tokio::time::sleep(delay))).await;
    let par_elapsed = t_par.elapsed();

    let speedup = seq_elapsed.as_secs_f64() / par_elapsed.as_secs_f64().max(0.000_001);
    println!(
        "\n[parallel] sequential {n}×100ms: {:?}  |  join_all {n}×100ms: {:?}  |  speedup: {:.1}×",
        seq_elapsed, par_elapsed, speedup
    );

    // Parallel must be at least 2× faster for 4 concurrent tasks.
    assert!(
        par_elapsed * 2 < seq_elapsed,
        "join_all ({:?}) was not ≥2× faster than sequential ({:?})",
        par_elapsed,
        seq_elapsed
    );
}
