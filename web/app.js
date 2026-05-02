// Sirin web UI — single-file state + render glue.
//
// Entry point: `sirin()` factory used by `<div x-data="sirin()">` in index.html.
// All state, fetch logic, and small helpers live here. Alpine.js handles
// reactive bindings — when state.* changes, the DOM updates automatically.
//
// AI-friendly notes:
//   • Whole UI logic is one factory function; grep for any binding name
//     in index.html to find what writes it here.
//   • Mock data is inlined so the UI renders standalone (no Sirin daemon).
//     When the real /api/snapshot endpoint exists, fetchSnapshot() switches
//     to live data automatically.
//   • No build step. Edit + F5.

window.sirin = function () {
  return {
    // ── View state (UI shell) ────────────────────────────────────────
    view: 'dashboard',     // 'dashboard' | 'testing' | 'workspace:N'
    testTab: 'runs',       // 'runs' | 'coverage' | 'browser' (within Testing)
    modal: null,           // null | 'settings' | 'logs' | 'devsquad' | …
    gearOpen: false,
    paletteOpen: false,
    paletteQuery: '',
    paletteIdx: 0,

    // ── Backend snapshot ────────────────────────────────────────────
    state: {
      version:        '0.4.6',
      browser_open:   true,
      browser_url:    'https://redandan.github.io/?__test_role=buyer',
      browser_title:  'Agora Market',
      rpc_running:    true,
      tg_connected:   false,
      last_verdict:   { status: 'passed', test_id: 'agora_pickup_time_picker' },
      agents: [
        { id: 'a1', name: '助手 A', live_status: 'connected',  platform: 'telegram' },
        { id: 'a2', name: '助手 B', live_status: 'idle',       platform: 'telegram' },
      ],
      pending_counts: { a1: 0, a2: 0 },
      active_runs: [],
      recent_runs: [
        { test_id: 'agora_pickup_time_picker',  status: 'passed',  duration_ms: 12300, started_at: '2026-05-02T10:00Z' },
        { test_id: 'agora_pickup_time_picker',  status: 'passed',  duration_ms: 11800, started_at: '2026-05-02T09:50Z' },
        { test_id: 'adhoc_20260501_175918_664', status: 'passed',  duration_ms: 18100, started_at: '2026-05-01T17:59Z' },
        { test_id: 'adhoc_20260501_174802_927', status: 'timeout', duration_ms: 60000, started_at: '2026-05-01T17:48Z' },
        { test_id: 'adhoc_20260501_174045_819', status: 'failed',  duration_ms: 9200,  started_at: '2026-05-01T17:40Z' },
        { test_id: 'agora_webrtc_permission',   status: 'passed',  duration_ms: 7200,  started_at: '2026-05-01T17:35Z' },
        { test_id: 'agora_webrtc_permission',   status: 'passed',  duration_ms: 6900,  started_at: '2026-05-01T17:30Z' },
        { test_id: 'agora_order_checkout_e2e',  status: 'passed',  duration_ms: 23400, started_at: '2026-05-01T17:20Z' },
      ],
      coverage: {
        product:           'agora_market',
        version:           '1.1',
        total_features:    45,
        total_covered:     35,
        scripted:          30,
        discovered:        16,
        discovery_status:  'NotRun',
      },
    },

    // ── Lifecycle ──────────────────────────────────────────────────
    async init() {
      // Global keyboard shortcuts.
      window.addEventListener('keydown', (e) => {
        const isMeta = e.ctrlKey || e.metaKey;
        if (isMeta && (e.key === 'k' || e.key === 'K')) {
          e.preventDefault();
          this.paletteOpen = true;
        } else if (e.key === 'Escape') {
          if (this.paletteOpen) this.paletteOpen = false;
          else if (this.modal) this.modal = null;
          else if (this.gearOpen) this.gearOpen = false;
        }
      });

      // Auto-focus palette input when opened.
      this.$watch('paletteOpen', (open) => {
        if (open) {
          this.paletteQuery = '';
          this.paletteIdx = 0;
          requestAnimationFrame(() => this.$refs.paletteInput?.focus());
        }
      });

      // Live data polling. Falls back silently to mock if endpoint missing.
      this.fetchSnapshot();
      setInterval(() => this.fetchSnapshot(), 5000);
    },

    async fetchSnapshot() {
      try {
        const r = await fetch('/api/snapshot', { cache: 'no-store' });
        if (!r.ok) return;            // backend not ready — keep mock
        const data = await r.json();
        this.state = { ...this.state, ...data };
      } catch (_e) {
        // Network error → keep current state (probably running standalone).
      }
    },

    // ── Helpers ─────────────────────────────────────────────────────
    truncate(s, n) {
      if (!s) return '';
      return s.length <= n ? s : s.slice(0, n) + '…';
    },

    truncateUrl(u, n) {
      if (!u) return '';
      if (u.length <= n) return u;
      return '…' + u.slice(u.length - n + 1);
    },

    verdictGlyph(status) {
      switch (status) {
        case 'passed':  return '✓';
        case 'failed':  return '✗';
        case 'timeout': return '⌚';
        case 'error':   return '!';
        case 'running': return '▶';
        default:        return '·';
      }
    },

    funnelPct(cov, tier) {
      if (!cov) return 0;
      const max = Math.max(
        cov.discovered || 0,
        cov.total_features || 0,
        cov.total_covered || 0,
        cov.scripted || 0,
        1,
      );
      const n = tier === 'discovered' ? cov.discovered
              : tier === 'covered'    ? cov.total_covered
              : tier === 'scripted'   ? cov.scripted
              : 0;
      return Math.round((n / max) * 100);
    },

    // ── Command palette entries ────────────────────────────────────
    paletteEntries: [
      { group: 'TESTING',   label: 'Coverage Map',     action: 'go-coverage' },
      { group: 'TESTING',   label: 'Browser Monitor',  action: 'go-browser' },
      { group: 'AUTOMATION',label: 'Dev Squad',        action: 'open-devsquad' },
      { group: 'AUTOMATION',label: 'MCP Playground',   action: 'open-mcp' },
      { group: 'OPS',       label: 'AI Router',        action: 'open-ai-router' },
      { group: 'OPS',       label: 'Session & Tasks',  action: 'open-tasks' },
      { group: 'OPS',       label: 'Cost & KB Stats',  action: 'open-cost' },
      { group: 'SYSTEM',    label: 'Settings',         action: 'open-settings' },
      { group: 'SYSTEM',    label: 'System Logs',      action: 'open-logs' },
      { group: 'VIEW',      label: 'Go to Dashboard',  action: 'go-dashboard' },
    ],

    get filteredEntries() {
      const q = this.paletteQuery.trim().toLowerCase();
      if (!q) return this.paletteEntries;
      return this.paletteEntries.filter((e) =>
        e.label.toLowerCase().includes(q)
        || e.group.toLowerCase().includes(q)
      );
    },

    runPaletteEntry(entry) {
      if (!entry) return;
      switch (entry.action) {
        case 'go-dashboard': this.view = 'dashboard'; break;
        case 'go-coverage':  this.view = 'testing'; this.testTab = 'coverage'; break;
        case 'go-browser':   this.view = 'testing'; this.testTab = 'browser'; break;
        case 'open-devsquad':   this.modal = 'devsquad'; break;
        case 'open-mcp':        this.modal = 'mcp'; break;
        case 'open-ai-router':  this.modal = 'ai-router'; break;
        case 'open-tasks':      this.modal = 'tasks'; break;
        case 'open-cost':       this.modal = 'cost-kb'; break;
        case 'open-settings':   this.modal = 'settings'; break;
        case 'open-logs':       this.modal = 'logs'; break;
      }
      this.paletteOpen = false;
      this.paletteQuery = '';
      this.paletteIdx = 0;
    },
  };
};
