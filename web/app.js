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

// ── Dashboard widget catalog (v0.5.5) ────────────────────────────
//
// Each entry describes one composable widget the user can drop into the
// dashboard via edit mode. `cols` is span-on-a-4-column-grid:
//   • 4 = full-width (run lists, busy live cards)
//   • 2 = half-width (visual cards: coverage funnel, browser preview)
//   • 1 = quarter-width (single-number KPI cells)
// AI-friendly: the catalog is a flat array — adding a widget = one entry
// here + one <template x-if="wid === 'X'"> block in index.html. No build,
// no separate component file.
const WIDGET_CATALOG = [
  { id: 'active_runs',     label: 'Active runs',          cols: 4 },
  { id: 'recent_runs',     label: 'Recent runs (8)',      cols: 4 },
  { id: 'coverage',        label: 'Coverage 3-tier',      cols: 2 },
  { id: 'browser',         label: 'Browser preview',      cols: 2 },
  { id: 'kpi_pass_rate',   label: 'KPI: Pass rate',       cols: 1 },
  { id: 'kpi_runs_today',  label: 'KPI: Runs today',      cols: 1 },
  // #279 tier 2: split into LLM vs replay so the mixed-mode average
  // doesn't camouflage the LLM cost regression.  Old `kpi_avg_duration`
  // kept for backward compat with persisted dashboard layouts.
  { id: 'kpi_avg_duration',label: 'KPI: Avg duration',    cols: 1 },
  { id: 'kpi_avg_duration_split', label: 'KPI: Avg (LLM / replay)', cols: 1 },
  { id: 'kpi_queue_drain', label: 'KPI: Queue drain ETA', cols: 1 },
  { id: 'kpi_cost_hour',   label: 'KPI: Cost / hour',     cols: 1 },
  { id: 'kpi_active_agents',label:'KPI: Active agents',   cols: 1 },
  { id: 'kpi_running_tests',label:'KPI: Running tests',   cols: 1 },
  { id: 'kpi_replay_rate',  label: 'KPI: Replay rate',    cols: 1 },
];

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

    // Workspace 待確認 / 設定 cache
    pending:      {},     // { [agent_id]: PendingReplyView[] }
    agent_detail: {},     // { [agent_id]: AgentDetailView }
    new_obj_text: '',     // input for "+ Add objective"
    save_status:  null,   // last persona / behavior save toast

    // ── Dashboard widget layout (v0.5.5) ────────────────────────────
    // dashboard_layout: ordered list of widget IDs the user has chosen.
    // Persisted to localStorage as `sirin_dashboard_layout_v1`. The
    // backing widget catalog is `WIDGET_CATALOG` (declared below the
    // factory). Use addWidget / removeWidget / moveWidget to mutate.
    dashboard_layout:    ['active_runs', 'recent_runs', 'coverage', 'browser'],
    dashboard_edit_mode: false,
    new_widget_id:       '',

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

      // #279 click-to-expand: which run_id (if any) is currently expanded
      // in the dashboard / testing run lists.  null = no row expanded.
      // Toggling the same row again collapses.
      expandedRunId: null,
      // Detail payload for the currently-expanded row.  Populated by
      // toggleRunDetail() via /mcp `get_test_result` (terminal runs) or
      // `live_trace` (running runs).  Shape: { run_id, history[], final_answer, error, fetched_at }.
      expandedRunDetail: null,
      expandedRunLoading: false,

      // Kibana-style log view state for the run detail page.
      // expandedStepIters: array of step.iter values currently expanded
      //   (Alpine isn't reactive on Set mutations — use Array + indexOf).
      // logSearch: free-text filter across thought/action/observation.
      // logTypeFilter: 'all' | 'browser' | 'llm-heavy' | 'wait' | 'analyze'.
      expandedStepIters: [],
      logSearch: '',
      logTypeFilter: 'all',

      // Sidebar collapse — persisted in localStorage.  Loaded in init().
      sidebarCollapsed: false,

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
      // #279 — touch Kibana log view state so Alpine binds them as
      // reactive properties at init time rather than lazily-on-template-
      // render.  Without this, accessing this.logSearch from
      // filteredLogSteps() before the run-detail section ever rendered
      // would throw "Cannot read properties of undefined (reading 'trim')".
      this.logSearch = '';
      this.logTypeFilter = 'all';
      this.expandedStepIters = [];

      // Sidebar collapse pref — restore from localStorage so layout
      // survives F5.  Saved on every toggle by toggleSidebar().
      try {
        this.sidebarCollapsed = localStorage.getItem('sirin_sidebar_collapsed') === '1';
      } catch (_) { /* localStorage may be blocked */ }

      // Restore saved Dashboard layout (v0.5.5). Falls back to default
      // 4-widget layout when localStorage is empty or contains a value
      // our catalog no longer recognizes (e.g. a removed widget id).
      try {
        const raw = localStorage.getItem('sirin_dashboard_layout_v1');
        if (raw) {
          const parsed = JSON.parse(raw);
          if (Array.isArray(parsed) && parsed.length > 0) {
            const valid_ids = WIDGET_CATALOG.map(w => w.id);
            this.dashboard_layout = parsed.filter(id => valid_ids.includes(id));
            if (this.dashboard_layout.length === 0) {
              this.dashboard_layout = ['active_runs','recent_runs','coverage','browser'];
            }
          }
        }
      } catch (_) {}

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

      // v0.5.4: prefer WebSocket push (2s tick from /ws). Falls back
      // automatically to 5s HTTP polling if the WS handshake fails or
      // the connection drops, so static-server previews still work.
      this.fetchSnapshot();   // initial hydrate (also lets UI render
                              // even if WS takes a sec to connect)
      this.connectWs();

      // Lazy-load slow modal data when each modal first opens.
      // (Settings / Logs / Dev Squad each have their own dedicated
      //  endpoints since v0.5.4 — config_check probes lmstudio over the
      //  network, etc., too slow for the 2s WebSocket push tick.)
      this.$watch('modal', (v) => {
        if (v === 'mcp' && this.mcp_tools.length === 0) this.loadMcpTools();
        if (v === 'settings') this.loadHealth();
        if (v === 'logs')     this.loadLogs();
        if (v === 'devsquad') this.loadTeamDashboard();
      });

      // When user navigates to a workspace, fetch the agent's full detail
      // (objectives + behavior settings) and pending replies. Cached per
      // agent so toggling between them is instant on subsequent visits.
      this.$watch('view', (v) => {
        const m = v.match(/^workspace:(\d+)$/);
        if (!m) return;
        const idx = parseInt(m[1], 10);
        const agent = this.state.agents[idx];
        if (!agent) return;
        if (!this.agent_detail[agent.id]) this.loadAgentDetail(agent.id);
        if (!this.pending[agent.id])      this.loadPending(agent.id);
        // v0.5.3: hydrate persisted chat thread (if not already cached)
        if (!this.chat_history[agent.id]) this.loadChatHistory(agent.id);
      });
    },

    // ── Dashboard widget layout actions (v0.5.5) ───────────────────
    get widgetCatalog() { return WIDGET_CATALOG; },

    /// Catalog entries the user hasn't already added — feeds the picker.
    get availableWidgets() {
      return WIDGET_CATALOG.filter(w => !this.dashboard_layout.includes(w.id));
    },

    widgetCols(id) {
      return WIDGET_CATALOG.find(w => w.id === id)?.cols || 4;
    },

    widgetLabel(id) {
      return WIDGET_CATALOG.find(w => w.id === id)?.label || id;
    },

    addWidget(id) {
      if (!id) return;
      if (this.dashboard_layout.includes(id)) return;
      this.dashboard_layout = [...this.dashboard_layout, id];
      this.new_widget_id = '';
      this.persistDashboardLayout();
    },

    removeWidget(id) {
      this.dashboard_layout = this.dashboard_layout.filter(w => w !== id);
      this.persistDashboardLayout();
    },

    moveWidget(id, delta) {
      const i = this.dashboard_layout.indexOf(id);
      if (i < 0) return;
      const j = i + delta;
      if (j < 0 || j >= this.dashboard_layout.length) return;
      const next = [...this.dashboard_layout];
      [next[i], next[j]] = [next[j], next[i]];
      this.dashboard_layout = next;
      this.persistDashboardLayout();
    },

    resetDashboardLayout() {
      this.dashboard_layout = ['active_runs','recent_runs','coverage','browser'];
      this.persistDashboardLayout();
    },

    persistDashboardLayout() {
      try {
        localStorage.setItem('sirin_dashboard_layout_v1',
          JSON.stringify(this.dashboard_layout));
      } catch (_) {}
    },

    // ── KPI computations (pure, derive from snapshot state) ────────
    // All getters tolerate missing fields so they render safely even
    // before the first /api/snapshot fetch lands.

    /// Pass rate over the visible recent_runs window (8 by default).
    /// 0 when no completed runs in the window.
    get kpi_pass_rate() {
      const runs = (this.state.recent_runs || [])
        .filter(r => ['passed','failed','timeout','error'].includes(r.status));
      if (runs.length === 0) return 0;
      const passed = runs.filter(r => r.status === 'passed').length;
      return passed / runs.length;
    },

    /// Tests started today (UTC date prefix match — coarse but cheap).
    get kpi_runs_today() {
      const today = new Date().toISOString().slice(0, 10);
      return (this.state.recent_runs || [])
        .filter(r => (r.started_at || '').startsWith(today)).length;
    },

    /// Average duration in seconds over recent completed runs.
    get kpi_avg_duration() {
      const runs = (this.state.recent_runs || [])
        .filter(r => r.duration_ms != null);
      if (runs.length === 0) return 0;
      const total = runs.reduce((s, r) => s + r.duration_ms, 0);
      return (total / runs.length) / 1000;
    },

    /// #279 tier 2 — split duration averages so the dashboard shows LLM
    /// vs replay without the mixed value camouflaging cost regressions.
    /// `null` for whichever side has no completed runs in the visible
    /// window (denominator must be ≥ 1).
    get kpi_avg_duration_llm() {
      const runs = (this.state.recent_runs || [])
        .filter(r => r.duration_ms != null && r.is_replay === false);
      if (runs.length === 0) return null;
      const total = runs.reduce((s, r) => s + r.duration_ms, 0);
      return (total / runs.length) / 1000;
    },
    get kpi_avg_duration_replay() {
      const runs = (this.state.recent_runs || [])
        .filter(r => r.duration_ms != null && r.is_replay === true);
      if (runs.length === 0) return null;
      const total = runs.reduce((s, r) => s + r.duration_ms, 0);
      return (total / runs.length) / 1000;
    },

    /// #279 tier 2 — queue drain ETA card.  Returns `null` when the
    /// queue is empty so the widget can render "—" gracefully.
    /// Backend already exposes `queue.estimated_drain_secs` (queued_count
    /// × median_test_secs); we just bucket into minutes for display.
    get kpi_queue_drain_min() {
      const q = this.state.queue;
      if (!q || (q.queued_count || 0) === 0) return null;
      const secs = q.estimated_drain_secs || 0;
      return Math.max(1, Math.ceil(secs / 60));
    },
    get kpi_queue_count() {
      return this.state.queue?.queued_count || 0;
    },
    get kpi_queue_median_secs() {
      return this.state.queue?.median_test_secs_last_50 || 0;
    },

    /// #279 tier 2 — coarse trend signal for pass_rate / avg_duration
    /// without backend support: split the recent_runs window in half,
    /// compare new half vs old half.  Returns +N (improving) / -N
    /// (regressing) / null (not enough data — need ≥ 4 completed runs).
    /// Pure UI hint; precise 7d trends would need backend stats.
    _trendDeltaPct(extractor) {
      const completed = (this.state.recent_runs || [])
        .filter(r => ['passed', 'failed', 'timeout', 'error'].includes(r.status));
      if (completed.length < 4) return null;
      const half = Math.floor(completed.length / 2);
      const newer = completed.slice(0, half);
      const older = completed.slice(completed.length - half);
      const a = extractor(newer);
      const b = extractor(older);
      if (a == null || b == null || b === 0) return null;
      return Math.round(((a - b) / b) * 100);
    },
    get kpi_pass_rate_trend_pp() {
      // Trend in PERCENTAGE POINTS (not %), because pass_rate is itself a %.
      const completed = (this.state.recent_runs || [])
        .filter(r => ['passed', 'failed', 'timeout', 'error'].includes(r.status));
      if (completed.length < 4) return null;
      const half = Math.floor(completed.length / 2);
      const rate = (rs) => rs.length === 0 ? null
        : rs.filter(r => r.status === 'passed').length / rs.length;
      const a = rate(completed.slice(0, half));
      const b = rate(completed.slice(completed.length - half));
      if (a == null || b == null) return null;
      return Math.round((a - b) * 100);
    },
    get kpi_avg_duration_trend_pct() {
      // Negative = faster (good).  Returns % change relative to older half.
      return this._trendDeltaPct((rs) => {
        const valid = rs.filter(r => r.duration_ms != null);
        if (valid.length === 0) return null;
        return valid.reduce((s, r) => s + r.duration_ms, 0) / valid.length;
      });
    },

    /// Cost burn rate ($/hr) — pulled from team_dashboard if present.
    get kpi_cost_per_hour() {
      return this.state.token_usage?.cost_per_hour || 0;
    },

    /// Agents currently in 'connected' state.
    get kpi_active_agents() {
      return (this.state.agents || [])
        .filter(a => a.live_status === 'connected').length;
    },

    /// Currently running test count (sometimes > 1 with run_test_batch).
    get kpi_running_tests() {
      return (this.state.active_runs || []).length;
    },

    /// #279 tier 2 — if any active run is stuck (idle ≥ 60s), return
    /// the most stuck one for the top-bar warning banner.  null when
    /// nothing is currently stuck (banner hidden).
    get topbarStuckRun() {
      const stuck = (this.state.active_runs || [])
        .filter(r => r.idle_secs != null && r.idle_secs >= 60);
      if (stuck.length === 0) return null;
      // Show the worst offender first.
      stuck.sort((a, b) => (b.idle_secs || 0) - (a.idle_secs || 0));
      return stuck[0];
    },

    /// #266: Replay-success rate over the last 7d (numerator =
    /// passed-via-replay; denominator = all non-adhoc runs).  0–1 ratio.
    get kpi_replay_rate() {
      return this.state.replay_health?.success_rate_replay || 0;
    },

    /// #266: Drift in percentage points (last 7d minus prior 7d).  Returns
    /// null when there isn't enough history for the comparison window —
    /// callers render "—" or hide the sub-line in that case.
    get kpi_replay_drift_pp() {
      const d = this.state.replay_health?.drift;
      return (d == null) ? null : d * 100;
    },

    /// #266: True when drift dropped > 10 percentage points week-over-week.
    /// Used to flip the KPI card border to yellow (warning).
    get kpi_replay_drift_warning() {
      const pp = this.kpi_replay_drift_pp;
      return pp != null && pp <= -10;
    },

    /// #266: Map test_id → ReplayStatus for fast lookup from Coverage view.
    /// Returns one of "healthy" | "stale_fallback" | "llm_only" | "untested"
    /// (or null if the snapshot hasn't loaded the array yet).
    replayStatusFor(testId) {
      const arr = this.state.test_replay_statuses || [];
      const hit = arr.find(t => t.test_id === testId);
      return hit ? hit.status : null;
    },

    /// #266: Emoji glyph for a replay status, used as a chip prefix in the
    /// Coverage view's test_ids list.
    replayStatusGlyph(status) {
      switch (status) {
        case 'healthy':         return '✅';
        case 'stale_fallback':  return '⚠️';
        case 'llm_only':        return '🤖';
        case 'untested':        return '○';
        default:                return '·';
      }
    },

    // ── On-demand modal loaders (v0.5.4) ────────────────────────────
    async loadHealth() {
      try {
        const r = await fetch('/api/health'); if (!r.ok) return;
        const d = await r.json();
        this.state.config_issues   = d.config_issues   || [];
        this.state.config_ok_count = d.config_ok_count || 0;
      } catch (_) {}
    },
    async loadLogs() {
      try {
        const r = await fetch('/api/logs'); if (!r.ok) return;
        this.state.log_lines = await r.json();
      } catch (_) {}
    },
    async loadTeamDashboard() {
      try {
        const r = await fetch('/api/team_dashboard'); if (!r.ok) return;
        const d = await r.json();
        this.state.team_dashboard = d.team_dashboard;
        this.state.token_usage    = d.token_usage;
      } catch (_) {}
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

    // ── Workspace 設定 (agent detail + behavior + objectives) ────────
    async loadAgentDetail(agent_id) {
      try {
        const r = await fetch(`/api/agent/${encodeURIComponent(agent_id)}`);
        if (!r.ok) return;
        this.agent_detail[agent_id] = await r.json();
      } catch (_) {}
    },

    async agentAction(agent_id, payload) {
      const r = await fetch(`/api/agent/${encodeURIComponent(agent_id)}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      const data = await r.json();
      if (data.error) throw new Error(data.error);
      // Refresh detail + parent snapshot so any list (sidebar / dashboard) reflects.
      await this.loadAgentDetail(agent_id);
      this.fetchSnapshot();
      return data;
    },

    async saveBehavior(detail) {
      this.save_status = '⏳ 儲存中…';
      try {
        await this.agentAction(detail.id, {
          action:    'behavior',
          enabled:   detail.human_behavior_enabled,
          min_delay: Number(detail.min_reply_delay),
          max_delay: Number(detail.max_reply_delay),
          max_hour:  Number(detail.max_per_hour),
          max_day:   Number(detail.max_per_day),
        });
        this.save_status = '✓ 已儲存';
      } catch (e) { this.save_status = '✗ ' + e.message; }
      setTimeout(() => { this.save_status = null; }, 3500);
    },

    async addObjective(agent_id) {
      const text = this.new_obj_text.trim();
      if (!text) return;
      try {
        await this.agentAction(agent_id, { action: 'objective_add', text });
        this.new_obj_text = '';
      } catch (e) { this.save_status = '✗ ' + e.message; }
    },

    async removeObjective(agent_id, index) {
      try { await this.agentAction(agent_id, { action: 'objective_remove', index }); }
      catch (e) { this.save_status = '✗ ' + e.message; }
    },

    async toggleAgent(agent_id, enabled) {
      try { await this.agentAction(agent_id, { action: 'toggle', enabled }); }
      catch (e) { this.save_status = '✗ ' + e.message; }
    },

    // ── Settings modal: persona save ────────────────────────────────
    async savePersonaName() {
      this.save_status = '⏳ 儲存中…';
      try {
        const r = await fetch('/api/persona/name', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ name: this.state.persona_name }),
        });
        const data = await r.json();
        if (data.error) throw new Error(data.error);
        this.save_status = '✓ 已儲存';
      } catch (e) { this.save_status = '✗ ' + e.message; }
      setTimeout(() => { this.save_status = null; }, 3500);
    },

    // ── Workspace 待確認 ────────────────────────────────────────────
    async loadPending(agent_id) {
      try {
        const r = await fetch(`/api/pending/${encodeURIComponent(agent_id)}`);
        if (!r.ok) return;
        this.pending[agent_id] = await r.json();
      } catch (_) {}
    },

    async pendingAction(agent_id, reply_id, action, text) {
      try {
        const body = { reply_id, action };
        if (text !== undefined) body.text = text;
        const r = await fetch(`/api/pending/${encodeURIComponent(agent_id)}`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
        });
        const data = await r.json();
        if (data.error) throw new Error(data.error);
        // Refresh list + snapshot (pending_count badge)
        await this.loadPending(agent_id);
        this.fetchSnapshot();
      } catch (e) { this.save_status = '✗ ' + e.message; }
    },

    // ── Workspace chat ──────────────────────────────────────────────
    // v0.5.3: hydrate persisted thread from SQLite. Empty array on first
    // visit means "no history yet" — keep it as [] (truthy length check).
    async loadChatHistory(agent_id) {
      try {
        const r = await fetch(`/api/chat/${encodeURIComponent(agent_id)}`);
        if (!r.ok) return;
        const msgs = await r.json();
        // Map the API shape {role, text, created_at} into our existing
        // {role, text} bubble shape; created_at is preserved for tooltips.
        this.chat_history[agent_id] = msgs.map(m => ({
          role: m.role, text: m.text, created_at: m.created_at,
        }));
      } catch (_) {
        this.chat_history[agent_id] = [];
      }
    },

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
        this.applySnapshot(data);
      } catch (_e) {
        // Network error → keep current state (probably running standalone).
      }
    },

    // Apply a snapshot from EITHER /api/snapshot fetch OR /ws message —
    // unifies state update so both paths look identical to the UI.
    applySnapshot(data) {
      this.state = {
        ...this.state,
        ...data,
        snapshot_tick: (this.state.snapshot_tick || 0) + 1,
      };
    },

    // ── WebSocket push channel (v0.5.4) ────────────────────────────
    // Connects to /ws, applies each pushed snapshot. On close (network
    // glitch or server restart) silently switches to 5s HTTP polling and
    // tries to reconnect every 5s — so the UI degrades gracefully and
    // recovers automatically without user action.
    _ws:           null,
    _ws_connected: false,
    _poll_timer:   null,
    connectWs() {
      try {
        const proto = location.protocol === 'https:' ? 'wss' : 'ws';
        const ws = new WebSocket(`${proto}://${location.host}/ws`);
        this._ws = ws;
        ws.addEventListener('open', () => {
          this._ws_connected = true;
          if (this._poll_timer) {
            clearInterval(this._poll_timer);
            this._poll_timer = null;
          }
        });
        ws.addEventListener('message', (ev) => {
          try { this.applySnapshot(JSON.parse(ev.data)); }
          catch (_) { /* ignore malformed frames */ }
        });
        ws.addEventListener('close', () => {
          this._ws_connected = false;
          this._ws = null;
          // Fallback: poll while disconnected, retry WS in 5 s.
          if (!this._poll_timer) {
            this._poll_timer = setInterval(() => this.fetchSnapshot(), 5000);
          }
          setTimeout(() => this.connectWs(), 5000);
        });
        ws.addEventListener('error', () => { /* close fires next */ });
      } catch (_) {
        // WebSocket constructor threw (very rare). Just poll.
        if (!this._poll_timer) {
          this._poll_timer = setInterval(() => this.fetchSnapshot(), 5000);
        }
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
      if (s < 60)   return s + ' 秒前';
      const m = Math.floor(s / 60);
      if (m < 60)   return m + ' 分鐘前';
      const h = Math.floor(m / 60);
      if (h < 24)   return h + ' 小時前';
      const d = Math.floor(h / 24);
      return d + ' 天前';
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

    verdictText(status) {
      // i18n — Chinese status text for run rows.  Used in place of the
      // raw `status.toUpperCase()` so the dashboard's PASSED/FAILED/etc
      // badges read 通過 / 失敗 / 逾時 / 錯誤 / 執行中 / 爭議.
      switch (status) {
        case 'passed':   return '通過';
        case 'failed':   return '失敗';
        case 'timeout':  return '逾時';
        case 'error':    return '錯誤';
        case 'running':  return '執行中';
        case 'queued':   return '排隊中';
        case 'disputed': return '爭議';
        default:         return status;
      }
    },

    perfHintText(hint) {
      switch (hint) {
        case 'healthy_fast_path': return '快路徑';
        case 'llm_latency_tail': return 'LLM尾延遲';
        case 'action_loop_or_ui_drift': return '動作迴圈';
        case 'long_tail_needs_trace_review': return '長尾待檢';
        case 'normal_range': return '正常';
        default: return hint || '';
      }
    },

    perfHintBadgeClass(hint) {
      switch (hint) {
        case 'healthy_fast_path': return 'badge-soft';
        case 'normal_range': return 'badge-soft badge-soft-info';
        case 'llm_latency_tail':
        case 'action_loop_or_ui_drift':
        case 'long_tail_needs_trace_review':
          return 'badge-soft badge-soft-warn';
        default:
          return 'badge-soft';
      }
    },

    // ── #279 tier 1: active-runs sub-phase / idle / ETA helpers ────
    /// True when the executor's current sub-phase has been stuck > 60 s.
    /// Used to flip a stuck red banner + dim the card border.
    isRunStuck(run) {
      if (!run || run.idle_secs === null || run.idle_secs === undefined) return false;
      return run.idle_secs >= 60;
    },

    subphaseLabel(kind) {
      switch (kind) {
        case 'llm_call':       return 'LLM 呼叫中';
        case 'browser_action': return '瀏覽器動作中';
        case 'replay':         return '腳本重播中';
        case 'verify':         return '判定驗證中';
        default:               return kind || '—';
      }
    },

    subphaseDotClass(kind) {
      switch (kind) {
        case 'llm_call':       return 'subphase-dot-llm';
        case 'browser_action': return 'subphase-dot-browser';
        case 'replay':         return 'subphase-dot-replay';
        case 'verify':         return 'subphase-dot-verify';
        default:               return '';
      }
    },

    formatSubphaseAge(ms) {
      if (ms === null || ms === undefined) return '';
      if (ms < 1000)   return ms + ' ms';
      if (ms < 60000)  return (ms / 1000).toFixed(1) + ' s';
      const m = Math.floor(ms / 60000);
      const s = Math.floor((ms % 60000) / 1000);
      return m + 'm ' + s + 's';
    },

    formatEta(secs) {
      if (secs === null || secs === undefined) return '';
      if (secs < 60)   return secs + 's';
      const m = Math.floor(secs / 60);
      const s = secs % 60;
      return s === 0 ? (m + 'm') : (m + 'm' + s + 's');
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
      // #279 — merge active_runs (running / queued) at the TOP so a
      // dashboard widget click → toggleRunDetail() can find the active
      // run in the history list and the expand panel renders below it.
      // De-dupe by run_id so a recently-completed run that's still in
      // both lists doesn't appear twice.
      const active = (this.state.active_runs || []).map(r => ({
        ...r,
        // active rows lack duration_ms / cost; fill with neutrals
        duration_ms: null,
        pass_rate: null,
      }));
      const seen = new Set(active.map(r => r.run_id).filter(Boolean));
      const recent = (this.state.recent_runs || []).filter(r => !r.run_id || !seen.has(r.run_id));
      const all = [...active, ...recent];
      return all.filter((r) => {
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

    // #279 — open the run detail full-page view for `run`.  Sets
    // view='run:<run_id>' (rendered by the dedicated section in index.html)
    // AND fetches the trace data in parallel.  Replaces the older inline
    // expand panel (UX feedback: full page > cramped inline strip).
    //
    // Fetch path:
    //   - terminal runs (passed/failed/timeout/error/disputed) → get_test_result
    //     (now has SQLite fallback for runs > 1h old, see commit db15271)
    //   - running / queued                                      → live_trace
    async openRunDetail(run) {
      if (!run || !run.run_id) {
        // Legacy row without run_id (pre-Phase-A history, ~2157 rows in
        // the typical SQLite).  No way to fetch step trace for these.
        // Show a brief toast so the click isn't silently swallowed.
        this.lastLaunch = '⚠ 此筆紀錄沒有 run_id（pre-Phase-A 老資料），無法查看詳細';
        setTimeout(() => { this.lastLaunch = null; }, 4000);
        return;
      }
      this.view = 'run:' + run.run_id;
      this.expandedRunId = run.run_id;
      this.expandedRunDetail = { test_id: run.test_id, run_id: run.run_id };
      this.expandedRunLoading = true;
      try {
        const isLive = ['running', 'queued'].includes(run.status);
        const tool = isLive ? 'live_trace' : 'get_test_result';
        const args = isLive ? { run_id: run.run_id, last_n: 10 }
                            : { run_id: run.run_id };
        const body = JSON.stringify({
          jsonrpc: '2.0', id: 1, method: 'tools/call',
          params: { name: tool, arguments: args },
        });
        const r = await fetch('/mcp', { method: 'POST', headers: {'Content-Type':'application/json'}, body });
        if (!r.ok) throw new Error('HTTP ' + r.status);
        const env = await r.json();
        if (env.error) throw new Error(env.error.message);
        const text = env.result?.content?.[0]?.text || '';
        const data = JSON.parse(text);
        // get_test_result returns { status, details: { iterations, error, analysis, ... } }
        // live_trace returns { status, recent_steps[], current_subphase, ... }
        const steps = data.recent_steps || [];
        const det = data.details || {};
        this.expandedRunDetail = {
          run_id: run.run_id,
          test_id: data.test_id || run.test_id,
          status: data.status || run.status,
          subphase: data.current_subphase,
          subphase_age_ms: data.subphase_age_ms,
          idle_secs: data.idle_secs,
          replay_mode: data.replay_mode,
          iterations: det.iterations ?? run.iterations,
          duration_ms: det.duration_ms ?? run.duration_ms,
          analysis: det.analysis ?? run.analysis,
          error: det.error,
          steps,
        };
      } catch (e) {
        this.expandedRunDetail = {
          test_id: run.test_id,
          run_id: run.run_id,
          error: '無法載入詳情：' + (e.message || e)
        };
      } finally {
        this.expandedRunLoading = false;
      }

      // #279 — auto-refresh while the test is still running so the
      // detail page acts like a live debugger (subphase ticks up,
      // new steps append, idle-warning surfaces).  Stops polling
      // when status becomes terminal OR when user navigates away.
      this._stopRunDetailPoll();
      const det = this.expandedRunDetail || {};
      const isLive = ['running', 'queued'].includes(det.status);
      if (isLive) {
        this._run_detail_poll = setInterval(() => {
          // User left the page — stop polling.
          if (this.view !== 'run:' + run.run_id) {
            this._stopRunDetailPoll();
            return;
          }
          this._refreshRunDetail(run);
        }, 3000);
      }
    },

    // Re-fetch run detail data without resetting the loading flag (so the
    // page doesn't flicker into "載入中…" each tick).  Stops polling on
    // terminal status.
    async _refreshRunDetail(run) {
      try {
        const isLive = ['running', 'queued'].includes(run.status) || ['running', 'queued'].includes(this.expandedRunDetail?.status);
        const tool = isLive ? 'live_trace' : 'get_test_result';
        const args = isLive ? { run_id: run.run_id, last_n: 30 } : { run_id: run.run_id, last_n: 30 };
        const r = await fetch('/mcp', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call',
                                 params: { name: tool, arguments: args } }),
        });
        if (!r.ok) return;
        const env = await r.json();
        if (env.error) return;
        const data = JSON.parse(env.result?.content?.[0]?.text || '{}');
        const det = data.details || {};
        this.expandedRunDetail = {
          run_id: run.run_id,
          test_id: data.test_id || run.test_id,
          status: data.status || run.status,
          subphase: data.current_subphase,
          subphase_age_ms: data.subphase_age_ms,
          idle_secs: data.idle_secs,
          replay_mode: data.replay_mode,
          iterations: det.iterations ?? run.iterations,
          duration_ms: det.duration_ms ?? run.duration_ms,
          analysis: det.analysis ?? run.analysis,
          error: det.error,
          steps: data.recent_steps || [],
        };
        // Terminal status → stop polling.
        if (!['running', 'queued'].includes(this.expandedRunDetail.status)) {
          this._stopRunDetailPoll();
        }
      } catch (_) {
        // Silent — transient network error; next tick retries.
      }
    },

    _stopRunDetailPoll() {
      if (this._run_detail_poll) {
        clearInterval(this._run_detail_poll);
        this._run_detail_poll = null;
      }
    },

    // Toggle sidebar collapsed state and persist to localStorage so the
    // layout choice survives F5 / browser restart.
    toggleSidebar() {
      this.sidebarCollapsed = !this.sidebarCollapsed;
      try { localStorage.setItem('sirin_sidebar_collapsed', this.sidebarCollapsed ? '1' : '0'); }
      catch (_) { /* localStorage may be blocked */ }
    },

    // #279 — return from the run detail full-page view back to the
    // testing tab's history list.  Clears the run-id state so the panel
    // is ready for a fresh fetch on the next click.
    closeRunDetail() {
      this._stopRunDetailPoll();
      this.view = 'testing';
      this.testTab = 'runs';
      this.expandedRunId = null;
      this.expandedRunDetail = null;
    },

    // ── Kibana-style log helpers (run detail page) ────────────────────

    // Filter the current expandedRunDetail.steps by logSearch + logTypeFilter.
    filteredLogSteps() {
      const steps = this.expandedRunDetail?.steps || [];
      const q = this.logSearch.trim().toLowerCase();
      const f = this.logTypeFilter;
      return steps.filter(s => {
        // Type filter
        if (f !== 'all') {
          const action = (s.action?.action || '').toLowerCase();
          if (f === 'browser' && !['shadow_click','click_point','goto','wait','scroll','enable_a11y','flutter_type','flutter_scroll_until_visible','ax_click','ax_focus','ax_find','set_viewport','close','reset_session'].includes(action)) return false;
          if (f === 'llm-heavy' && (s.llm_latency_ms || 0) < 5000) return false;
          if (f === 'analyze' && action !== 'screenshot_analyze') return false;
          if (f === 'error' && !(s.observation || '').match(/error|fail|timeout|not.found|exception/i)) return false;
        }
        // Search
        if (q) {
          const blob = (s.thought + ' ' + JSON.stringify(s.action) + ' ' + s.observation).toLowerCase();
          if (!blob.includes(q)) return false;
        }
        return true;
      });
    },

    // Click a log row to toggle its full-document expanded view.
    toggleStepRow(iter) {
      const arr = this.expandedStepIters;
      const idx = arr.indexOf(iter);
      if (idx >= 0) arr.splice(idx, 1);
      else arr.push(iter);
    },

    isStepExpanded(iter) {
      return this.expandedStepIters.includes(iter);
    },

    // Color-class a row based on action type — Kibana-style level badge.
    actionRowClass(step) {
      const action = step.action?.action || '';
      const obs = step.observation || '';
      if (obs.match(/^ERROR|^FAIL|exception|panic/i)) return 'log-row-error';
      if ((step.llm_latency_ms || 0) > 15000) return 'log-row-warn';      // > 15s = slow
      if (action === 'screenshot_analyze') return 'log-row-analyze';
      if (['wait', 'enable_a11y'].includes(action)) return 'log-row-trivial';
      return 'log-row-info';
    },

    // (truncate(s, n) already defined earlier — reused here.)

    // Format a TestStep's `ts` (RFC-3339) → "HH:MM:SS" for display in
    // the run detail step cards.  Returns empty when ts missing.
    // Hover tooltip on the rendered span shows the full ISO timestamp.
    formatStepTime(ts) {
      if (!ts) return '';
      const d = new Date(ts);
      if (isNaN(d.getTime())) return '';
      const pad = (n) => String(n).padStart(2, '0');
      return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    },

    // Format a step's action for the expanded view — shows "shadow_click target=..."
    // or just the action name when no target.  Truncates long values.
    formatStepAction(step) {
      if (!step || !step.action) return '?';
      const a = step.action;
      if (typeof a === 'string') return a;
      const name = a.action || '?';
      const target = a.target || a.path || a.url || a.text;
      if (!target) return name;
      const t = String(target);
      return name + ' ' + (t.length > 60 ? t.slice(0, 60) + '…' : t);
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
