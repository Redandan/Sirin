//! Screenshot hash caching for vision LLM analysis.
//! Avoids re-analyzing identical screenshots via SHA256 hashing.
//! Expected token savings: 60-80% of vision-analysis calls per test.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Screenshot cache: maps SHA256(png_bytes) → vision LLM analysis result.
/// Stored in-memory for the lifetime of a test run.
#[derive(Debug, Clone)]
pub struct ScreenshotCache {
    /// Map of hex-encoded SHA256 hashes to cached vision results
    cache: HashMap<String, String>,
    /// Statistics tracking
    hits: u32,
    misses: u32,
}

impl ScreenshotCache {
    /// Create a new, empty screenshot cache.
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Hash PNG bytes using SHA256.
    fn hash_png(png_bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(png_bytes);
        let result = hasher.finalize();
        format!("{:x}", result)
    }

    /// Check if we've seen this screenshot before.
    /// Returns cached result if found (hit).
    /// Returns None if not in cache (miss).
    pub fn get(&mut self, png_bytes: &[u8]) -> Option<String> {
        let hash = Self::hash_png(png_bytes);
        if let Some(cached) = self.cache.get(&hash) {
            self.hits += 1;
            return Some(cached.clone());
        }
        self.misses += 1;
        None
    }

    /// Store the vision result for this screenshot.
    pub fn put(&mut self, png_bytes: &[u8], result: String) {
        let hash = Self::hash_png(png_bytes);
        self.cache.insert(hash, result);
    }

    /// Get cache statistics.
    pub fn stats(&self) -> (u32, u32) {
        (self.hits, self.misses)
    }

    /// Reset statistics (called at end of test).
    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }
}

impl Default for ScreenshotCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_hit() {
        let mut cache = ScreenshotCache::new();
        let png = b"test png data";
        let result = "analyzed result".to_string();

        cache.put(png, result.clone());
        let cached = cache.get(png);

        assert_eq!(cached, Some(result));
        assert_eq!(cache.stats(), (1, 0));
    }

    #[test]
    fn test_cache_miss() {
        let mut cache = ScreenshotCache::new();
        let png1 = b"test png 1";
        let png2 = b"test png 2";

        cache.put(png1, "result 1".to_string());
        let cached = cache.get(png2);

        assert_eq!(cached, None);
        assert_eq!(cache.stats(), (0, 1));
    }

    #[test]
    fn test_hash_consistency() {
        let png = b"consistent png data";
        let hash1 = ScreenshotCache::hash_png(png);
        let hash2 = ScreenshotCache::hash_png(png);

        assert_eq!(hash1, hash2);
    }
}
