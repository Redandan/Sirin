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

    // Testing → Runs filter state
    runFilter:     'all',  // 'all' | 'passed' | 'failed'
    runTextFilter: '',
    lastLaunch:    null,   // status string after launch attempt

    // MCP Playground modal state
    mcp_tools:    [],     // [{name, description, inputSchema}, …]
    mcp_loading:  false,
    mcp_query:    '',
    mcp_selected: null,
    mcp_args:     '{}',
    mcp_result:   null,
    mcp_running:  false,

    // Workspace chat state — keyed by agent.id so per-agent histories persist
    // while the user navigates between agents.
    chat_history: {},     // { [agent_id]: [{role:'user'|'agent', text:string}, …] }
    chat_input:   '',
    chat_sending: false,

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
        groups: [
          {
            id: 'buyer_browse', name: 'Buyer：商品瀏覽 & 搜尋', role: 'buyer',
            covered: 4, total: 4,
            features: [
              { id: 'list',   name: '商品列表頁', status: 'partial',   test_ids: ['agora_market_smoke'] },
              { id: 'search', name: '關鍵字搜尋', status: 'confirmed', test_ids: ['agora_search_keyword'] },
              { id: 'detail', name: '商品詳情頁（名稱/價格/SKU）', status: 'confirmed', test_ids: ['agora_order_checkout_e2e'] },
              { id: 'cart',   name: '加入購物車 & 購物車頁', status: 'confirmed', test_ids: ['agora_cart_add_remove'] },
            ],
          },
          {
            id: 'buyer_checkout', name: 'Buyer：結帳 & 訂單', role: 'buyer',
            covered: 4, total: 4,
            features: [
              { id: 'place',    name: '下單確認頁 → 真實下單', status: 'confirmed', test_ids: ['agora_c2c_place_order'] },
              { id: 'flow',     name: '購買確認流程（SKU 選擇 → 確認）', status: 'confirmed', test_ids: ['agora_checkout_dry'] },
              { id: 'orders',   name: '訂單管理頁', status: 'confirmed', test_ids: ['agora_buyer_order_view'] },
              { id: 'receipt',  name: '確認收貨流程', status: 'confirmed', test_ids: ['agora_c2c_buyer_confirm_receipt'] },
            ],
          },
          {
            id: 'buyer_wallet', name: 'Buyer：錢包 & 儲值', role: 'buyer',
            covered: 3, total: 4,
            features: [
              { id: 'balance', name: '錢包餘額顯示',   status: 'confirmed', test_ids: ['agora_buyer_wallet'] },
              { id: 'topup',   name: '儲值 UI（金額選擇/創建儲值按鈕）', status: 'confirmed', test_ids: ['agora_c2c_wallet_deposit'] },
              { id: 'history', name: '錢包交易記錄',   status: 'confirmed', test_ids: ['agora_c2c_wallet_transactions'] },
              { id: 'withdraw',name: '提現流程',       status: 'missing',   test_ids: [] },
            ],
          },
          {
            id: 'seller_orders', name: 'Seller：訂單管理', role: 'seller',
            covered: 0, total: 3,
            features: [
              { id: 'pending',  name: '待出貨訂單',   status: 'missing', test_ids: [] },
              { id: 'shipping', name: '出貨流程',     status: 'missing', test_ids: [] },
              { id: 'history',  name: '已完成訂單',   status: 'missing', test_ids: [] },
            ],
          },
        ],
      },

      // Available test files (test_ids) for the launcher dropdown.
      test_ids: [
        'agora_market_smoke',
        'agora_search_keyword',
        'agora_order_checkout_e2e',
        'agora_cart_add_remove',
        'agora_c2c_place_order',
        'agora_checkout_dry',
        'agora_buyer_order_view',
        'agora_c2c_buyer_confirm_receipt',
        'agora_buyer_wallet',
        'agora_c2c_wallet_deposit',
        'agora_pickup_time_picker',
        'agora_webrtc_permission',
      ],
      selected_test_id: 'agora_market_smoke',

      // ── Modal mock data ─────────────────────────────────────────────
      // config_check output — Settings modal lists these cards.
      config_issues: [
        { severity: 'error',   category: 'Router',      message: "Router backend 'lmstudio' not reachable at http://localhost:1234/v1", suggestion: 'Start lmstudio or remove ROUTER_LLM_PROVIDER from .env to use cloud model for routing.' },
        { severity: 'info',    category: 'Roles',       message: 'No dedicated coding model — using main model for code tasks',         suggestion: 'Set CODING_MODEL in .env or coding_model in llm.yaml for better cost control.' },
      ],
      config_ok_count: 8,

      persona_name: '助手 A',
      llm_main:     'gemini-2.5-flash',
      llm_router:   'deepseek-chat (fallback gemini)',

      // System logs (Datadog explorer mock)
      log_lines: [
        { level: 'info',   ts: '12:34:01', text: '[mcp] tools/list responded with 90 tools' },
        { level: 'info',   ts: '12:34:03', text: '[telegram] connection lost, reconnecting in 5s' },
        { level: 'warn',   ts: '12:34:08', text: '[browser] CDP transport latency 1240ms (>800ms threshold)' },
        { level: 'info',   ts: '12:34:15', text: '[test_runner] agora_pickup_time_picker queued' },
        { level: 'info',   ts: '12:34:18', text: '[test_runner] agora_pickup_time_picker step 1/7 — goto target' },
        { level: 'error',  ts: '12:34:42', text: '[browser] enable_a11y timeout, retrying' },
        { level: 'info',   ts: '12:34:45', text: '[test_runner] agora_pickup_time_picker step 2/7 — wait 3000ms' },
        { level: 'info',   ts: '12:34:51', text: '[test_runner] agora_pickup_time_picker PASSED in 12.3s' },
      ],

      // Dev Squad (multi_agent::team_dashboard) mock
      team_dashboard: {
        worker_running: false,
        queued: 0, running: 0, done: 12, failed: 1,
        pm:        { role: 'pm',       session_id: '540231e5-a0d1', turns: 3 },
        engineer:  { role: 'engineer', session_id: '5f3382b8-26b9', turns: 2 },
        tester:    { role: 'tester',   session_id: null,            turns: 0 },
      },
      token_usage: {
        cost_per_hour: 0.00,
        api_calls: 0,
        tokens_per_min: 0,
        cache_hit_pct: 0,
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

      // Lazy-load MCP tools list when the modal first opens; cached
      // afterward so re-opens are instant.
      this.$watch('modal', (v) => {
        if (v === 'mcp' && this.mcp_tools.length === 0) this.loadMcpTools();
      });
    },

    // ── MCP Playground ──────────────────────────────────────────────
    async loadMcpTools() {
      this.mcp_loading = true;
      try {
        const r = await fetch('/mcp', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/list' }),
        });
        const data = await r.json();
        this.mcp_tools = data.result?.tools || [];
      } catch (_) {
        this.mcp_tools = [];
      } finally {
        this.mcp_loading = false;
      }
    },

    get mcpFilteredTools() {
      const q = this.mcp_query.trim().toLowerCase();
      const all = this.mcp_tools;
      const list = !q ? all
        : all.filter(t =>
            t.name.toLowerCase().includes(q)
            || (t.description || '').toLowerCase().includes(q));
      return list.slice(0, 80);  // virtualize if grows past
    },

    selectMcpTool(t) {
      this.mcp_selected = t;
      this.mcp_args     = '{}';
      this.mcp_result   = null;
    },

    // ── Workspace chat ──────────────────────────────────────────────
    async sendChat() {
      const ag = this.currentAgent;
      const msg = this.chat_input.trim();
      if (!ag || !msg || this.chat_sending) return;
      const history = this.chat_history[ag.id] = this.chat_history[ag.id] || [];
      history.push({ role: 'user', text: msg });
      this.chat_input  = '';
      this.chat_sending = true;
      try {
        const r = await fetch('/api/chat', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ agent_id: ag.id, message: msg }),
        });
        const data = await r.json();
        if (data.error) throw new Error(data.error);
        history.push({ role: 'agent', text: data.reply || '(empty reply)' });
      } catch (e) {
        history.push({ role: 'agent', text: '✗ ' + (e.message || e) });
      } finally {
        this.chat_sending = false;
      }
    },

    async runMcpTool() {
      if (!this.mcp_selected || this.mcp_running) return;
      this.mcp_running = true;
      this.mcp_result  = null;
      try {
        let args;
        try { args = JSON.parse(this.mcp_args || '{}'); }
        catch (e) { throw new Error('args JSON 解析失敗: ' + e.message); }
        const r = await fetch('/mcp', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            jsonrpc: '2.0', id: 2, method: 'tools/call',
            params: { name: this.mcp_selected.name, arguments: args },
          }),
        });
        const data = await r.json();
        if (data.error) {
          this.mcp_result = '✗ ' + data.error.message;
        } else {
          this.mcp_result = data.result?.content?.[0]?.text
            || JSON.stringify(data.result, null, 2);
        }
      } catch (e) {
        this.mcp_result = '✗ ' + (e.message || e);
      } finally {
        this.mcp_running = false;
      }
    },

    async fetchSnapshot() {
      try {
        const r = await fetch('/api/snapshot', { cache: 'no-store' });
        if (!r.ok) return;            // backend not ready — keep mock
        const data = await r.json();
        // snapshot_tick increments every successful fetch — used as the
        // cache-buster for /api/browser_screenshot so <img> refreshes in
        // sync with the JSON poll without becoming a flicker storm.
        this.state = {
          ...this.state,
          ...data,
          snapshot_tick: (this.state.snapshot_tick || 0) + 1,
        };
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

    // GitHub-Actions-style "X minutes ago" / "Y hours ago".
    // Input: RFC3339 string. Returns short human-readable interval.
    timeAgo(rfc) {
      if (!rfc) return '';
      const ms = Date.now() - new Date(rfc).getTime();
      if (isNaN(ms) || ms < 0) return '';
      const s = Math.floor(ms / 1000);
      if (s < 60)   return s + 's ago';
      const m = Math.floor(s / 60);
      if (m < 60)   return m + 'm ago';
      const h = Math.floor(m / 60);
      if (h < 24)   return h + 'h ago';
      const d = Math.floor(h / 24);
      return d + 'd ago';
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

    modalLabel(m) {
      if (!m) return '';
      const map = {
        settings:    'SETTINGS',
        logs:        'LOGS',
        devsquad:    'DEV SQUAD',
        mcp:         'MCP PLAYGROUND',
        'ai-router': 'AI ROUTER',
        tasks:       'SESSION & TASKS',
        'cost-kb':   'COST & KB',
      };
      return map[m] || m.toUpperCase();
    },

    memberRoleName(role) {
      return ({ pm: 'PM', engineer: 'Engineer', tester: 'Tester' })[role] || role;
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

    // ── Workspace derived state ────────────────────────────────────
    get currentAgent() {
      const m = this.view.match(/^workspace:(\d+)$/);
      if (!m) return null;
      return this.state.agents[parseInt(m[1], 10)] || null;
    },

    // ── Testing → Runs derived state + actions ────────────────────
    get filteredRuns() {
      const f = this.runFilter;
      const q = this.runTextFilter.trim().toLowerCase();
      return this.state.recent_runs.filter((r) => {
        if (f === 'passed' && r.status !== 'passed') return false;
        if (f === 'failed' && !['failed', 'error', 'timeout'].includes(r.status)) return false;
        if (q && !r.test_id.toLowerCase().includes(q)) return false;
        return true;
      });
    },

    async launchTest(id) {
      if (!id) return;
      this.lastLaunch = '⏳ launching…';
      try {
        const body = JSON.stringify({
          jsonrpc: '2.0', id: 1, method: 'tools/call',
          params: { name: 'run_test_async', arguments: { test_id: id } },
        });
        const r = await fetch('/mcp', { method: 'POST', headers: {'Content-Type':'application/json'}, body });
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const data = await r.json();
        if (data.error) throw new Error(data.error.message);
        const text = data.result?.content?.[0]?.text || '';
        const m = text.match(/run_id["\s:]+([\w_]+)/);
        this.lastLaunch = '✓ launched ' + (m ? m[1] : id);
        // Force quick refresh so the active-runs section picks up the new run.
        this.fetchSnapshot();
      } catch (e) {
        this.lastLaunch = '✗ ' + (e.message || e);
      }
      // Auto-clear after 6 s.
      setTimeout(() => { this.lastLaunch = null; }, 6000);
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
