# Browser State Authority — Design Note

**Status:** active design • **Last updated:** 2026-04-18

## Problem (#23)

`browser::current_url()` and `browser::page_title()` returned **stale** values
in three real-world scenarios:

1. **`about:blank` reset** — `window.location.replace('about:blank')` fires no
   `Page.frameNavigated`; the headless_chrome `Tab` cache stays on the prior URL.
2. **Cross-origin redirect race** — fast 302 chains can land on the final page
   before all `Target.targetInfoChanged` events drain to our cache.
3. **SPA hash-only navigation** — Chrome does not emit `frameNavigated` for
   pure fragment changes; the cache only sees the original URL.

The symptom external test agents see is that `browser_exec.url` says
`https://app/login` while the rendered page is already on `https://app/dashboard`,
making the AI think login failed.

## Why we abandoned the Chrome extension fix

Original plan: ship a Manifest V3 companion extension that pushes
`chrome.tabs.*` and `chrome.webNavigation.*` events to Sirin via WebSocket.
These APIs are owned by the browser process and miss none of the above events.

**Blocker (verified 2026-04-18 against Chrome 147.0.7727.101):** Chrome no
longer honours `--load-extension=<path>` from the command line. The opt-out
feature flag `--disable-features=DisableLoadExtensionCommandLineSwitch`
(introduced in Chrome 122 to keep the legacy behaviour for one release cycle)
has been **removed entirely** by Chrome 147.

Verification chain — even a clean manual launch with every conceivable flag
combination yields zero loaded extensions:

```bash
chrome.exe --user-data-dir=<fresh> \
  --load-extension=C:\path\to\ext \
  --disable-extensions-except=C:\path\to\ext \
  --disable-features=DisableLoadExtensionCommandLineSwitch \
  about:blank
# → chrome.developerPrivate.getExtensionsInfo() returns ZERO unpacked extensions
```

`chrome://extensions/` shows only the user's pre-installed Web Store extensions;
the unpacked path is silently ignored. No console warning, no banner, no
diagnostic. Only Web Store-signed and component extensions load.

### Future paths if/when we revisit

| Path | Cost | Trade-off |
|------|------|-----------|
| Bundle Chrome for Testing (CfT) | +200 MB downloads, version sync chore | CfT preserves the legacy `--load-extension` behaviour |
| Pack & sign `.crx` + HKCU `ExtensionInstallForcelist` policy | Signing key management, MV3 SW gotchas | Affects user's own Chrome too (intrusive) |
| Modify `<user-data-dir>/Default/Preferences` to spoof `location: 5` (component) | Brittle across Chrome upgrades | Chrome verifies component extensions |

The companion extension scaffold (`ext/manifest.json`, `ext/background.js`,
`src/ext_server.rs`) is **kept in tree** as a stub. It costs nothing — without
the extension loading, `ext_server` simply never receives a connection and the
public API gracefully reports `connected: false`. When we re-enable extension
delivery (most likely via CfT bundling), only the launch logic needs to change.

## Current fix — raw-CDP authority

Replace cache-reads with live CDP calls in two places:

| Method | Old (cached)            | New (live)                                        |
|--------|-------------------------|---------------------------------------------------|
| `current_url()` | `tab.get_url()`     | `tab.evaluate("window.location.href", false)` |
| `page_title()`  | `tab.get_title()`   | `tab.evaluate("document.title", false)`       |

`Runtime.evaluate` reads from the live JS execution context — the same source
of truth the rendered page itself uses. It bypasses every `Tab` cache layer,
making it trivially correct for hash-only navigation, `about:blank` reset, and
cross-origin redirects.

### Fallback strategy

`Runtime.evaluate` can fail in two narrow cases:

- The page is in the middle of a navigation and has no execution context yet
  (~50 ms window).
- A DevTools agent is paused at a breakpoint.

Both fall back to the cached `tab.get_url()`/`tab.get_title()`, which is no
worse than the current behaviour.

## Trade-off accepted

- **Correctness over latency** — each call now adds one CDP round-trip
  (~1-3 ms on localhost). Acceptable: these methods are called O(1) per agent
  step, not in hot loops.
- **No new dependencies** — the existing `tab.evaluate` API is enough.
- **Coverage estimate** — covers ~80 % of the original #23 scenarios. The
  remaining 20 % (popup target migration, `Target.targetCrashed` recovery)
  require Chrome process-level introspection that only an extension or a
  bundled CfT browser can provide.

## Validation

`cargo test --bin sirin browser::tests::current_url_*` covers:

- Hash-only navigation reflects in `current_url()` immediately
- `about:blank` reset is observed within one call
- `document.title` change via JS is reflected in `page_title()`

Plus the existing `browser_lifecycle` ignored E2E.

## References

- crbug.com/40279754 — Chrome 122 deprecation of `--load-extension`
- Chrome source: `chrome/browser/extensions/extension_management.cc` —
  `kDisableLoadExtensionCommandLineSwitch` removal
- Issue #23 (Sirin) — original stale-URL report
