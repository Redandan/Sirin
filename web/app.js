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
  { id: 'kpi_replay_rate',  label: 'KPI: Zero-LLM pass rate', cols: 1 },
];

// ── Global "Enter submits" rule ──────────────────────────────────
//
// Instead of wiring @keydown.enter="fn()" on every text input, pressing
// Enter in any single-line text field clicks the nearest enabled
// btn-primary/btn-secondary that shares its IMMEDIATE parent element
// (siblings only — no walking further up the tree). That keeps it safe:
// a filter/search box whose parent has no action button (e.g. the
// run-history filter row, log search, MCP query box) just does nothing
// on Enter instead of accidentally clicking an unrelated button several
// sections up. Fields where the button lives in a different row (e.g.
// persona name → 儲存 in the row below) keep their own explicit
// @keydown.enter binding since they don't fit this pattern.
function findEnterSubmitButton(el) {
  const parent = el.parentElement;
  if (!parent) return null;
  return parent.querySelector(
    'button.btn-primary:not(:disabled), button.btn-secondary:not(:disabled)'
  );
}

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
    expandedRunDetail: null,
    expandedRunLoading: false,

    // Ops Scout — Sirin-local ops/test-health review inbox.
    scoutCandidates: [],
    scoutLoading: false,
    scoutError: null,
    scoutStatus: 'all',
    scoutSeverity: 'all',
    scoutEventType: 'all',
    scoutAllowedAction: 'all',
    scoutEvidenceLevel: 'all',
    scoutSelectedKey: null,
    scoutReviewStatus: 'CONVERT_TO_ISSUE',
    scoutReviewNote: '',
    scoutReviewBusy: false,
    scoutToast: null,
    handoffQueue: [],
    handoffLoading: false,
    handoffError: null,
    handoffStatus: 'READY',
    handoffBusy: false,
    opsSchedules: [],
    opsScheduleLoading: false,
    opsScheduleRunBusy: false,
    opsScheduleError: null,
    opsScheduleToast: null,
    opsScheduleStatus: 'all',
    newOpsTitle: 'Daily Agora test health scout',
    newOpsCadence: 'daily',
    newOpsPriority: 'P1',
    newOpsTaskType: 'SIRIN_AGORA_TEST_HEALTH_SCOUT',
    newOpsParams: '{ "suite": "active-smoke", "windowDays": 30, "maxCandidates": 12, "excludeTags": ["obsolete"] }',

    // iOS Location — owned/test iPhone developer-location workbench.
    iosStatus: null,
    iosStatusLoading: false,
    iosBusy: false,
    iosToast: null,
    iosError: null,
    iosResult: null,
    iosRouteResult: null,
    iosRouteRunResult: null,
    iosCurrentResult: null,
    iosLatestReportResult: null,
    iosShareInfo: null,
    iosLocationConfirmed: false,
    iosLocationSource: 'manual',
    iosSelectedPresetId: 'chiang_mai_airport',
    iosOriginPresets: [
      { id: 'chiang_mai_airport', label: '清邁機場', city: 'chiang_mai', lat: 18.766847, lng: 98.962646, radiusMeters: 1200 },
      { id: 'chiang_mai_old_city', label: '清邁古城', city: 'chiang_mai', lat: 18.788343, lng: 98.985300, radiusMeters: 1200 },
      { id: 'maya_chiang_mai', label: 'MAYA 商場', city: 'chiang_mai', lat: 18.802315, lng: 98.967018, radiusMeters: 1200 },
    ],
    iosRouteAutoRefresh: true,
    iosRouteRunPollTimer: null,
    iosAutoResetAfterPlayback: false,
    iosAutoResetDone: false,
    iosLat: '18.788343',
    iosLng: '98.985300',
    iosTargetApp: 'AgoraMarket iOS',
    iosNote: 'operator-ui smoke',
    iosRouteSource: 'current_city_roam',
    iosCity: 'chiang_mai',
    iosRoamRadiusMeters: 1500,
    iosRoamWaypoints: 8,
    iosRoadProvider: 'osrm',
    iosAllowSyntheticFallback: false,
    iosMovementProfile: 'bicycle',
    iosSpeedKmh: 20,
    iosDurationSeconds: 600,
    iosTickSeconds: 1,
    iosIncludeSchedule: false,
    iosRouteRunBusy: false,
    iosLeafletMap: null,
    iosLeafletLayers: null,
    iosLeafletReady: false,
    iosLeafletError: null,
    iosLeafletLoadAttempts: 0,
    iosMapPickMode: 'route',
    iosRoutePoints: `[
  {"lat":18.788343,"lng":98.985300},
  {"lat":18.793000,"lng":98.990000},
  {"lat":18.798000,"lng":98.987000},
  {"lat":18.801000,"lng":98.995000}
]`,

    // AI Work Monitor — on-demand only. No interval exists while this view
    // is closed; the backend additionally caches collection for 15 seconds.
    aiMonitor: null,
    aiMonitorLoading: false,
    aiMonitorError: null,
    aiMonitorTimer: null,
    aiSpeedTest: null,
    aiSpeedTestBusy: false,
    aiSpeedTestError: null,
    coreTelemetryStarted: false,
    coreTelemetryPaused: false,

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
      queue: {
        queued_count: 0,
        estimated_drain_secs: 0,
        median_test_secs_last_50: 0,
      },
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

      // Tray shortcut opens /ui/?view=ai-monitor. Only accept known views so
      // arbitrary query text can never become an Alpine expression/state key.
      try {
        const requested = new URLSearchParams(location.search).get('view');
        if (['dashboard', 'testing', 'ops-scout', 'ios-location', 'ai-monitor'].includes(requested)) {
          this.view = requested;
          if (requested === 'ai-monitor') {
            // The generic standalone mock says Browser=running. That is not
            // evidence, so fail closed until the core snapshot is requested.
            this.state.browser_open = false;
            this.state.browser_url = 'AI monitor · on-demand only';
            this.state.rpc_running = true;
            this.state.last_verdict = null;
            this.state.agents = [];
            this.state.pending_counts = {};
          }
        }
      } catch (_) {}

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
        } else if (e.key === 'Enter' && !e.isComposing) {
          // Skip while an IME (CJK) candidate window is open — that Enter
          // confirms the candidate, it isn't a "submit" keystroke.
          const el = e.target;
          const textTypes = ['text', 'search', 'email', 'password', 'number', 'url', 'tel'];
          if (el instanceof HTMLInputElement && textTypes.includes(el.type) && !el.disabled) {
            const btn = findEnterSubmitButton(el);
            if (btn) {
              e.preventDefault();
              btn.click();
            }
          }
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
      // A direct tray launch into AI Monitor must not also start the global
      // 2-second snapshot/WS loop. Start core telemetry only when another
      // Sirin view is actually used.
      if (this.view !== 'ai-monitor') this.startCoreTelemetry();

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
        if (v === 'ai-monitor') {
          this.pauseCoreTelemetry();
          this.startAiMonitorRefresh();
          return;
        }
        this.stopAiMonitorRefresh();
        this.startCoreTelemetry();
        if (v === 'ops-scout') {
          this.loadScoutCandidates();
          this.loadOpsSchedules();
          this.loadHandoffQueue();
          return;
        }
        if (v === 'ios-location') {
          this.loadIosStatus();
          this.loadIosShareInfo();
          return;
        }
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

      if (this.view === 'ai-monitor') this.startAiMonitorRefresh();
    },

    // ── AI Work Monitor (on-demand, read-only) ─────────────────────
    startCoreTelemetry() {
      if (this.coreTelemetryStarted) return;
      this.coreTelemetryPaused = false;
      this.coreTelemetryStarted = true;
      this.fetchSnapshot();
      this.connectWs();
    },

    pauseCoreTelemetry() {
      this.coreTelemetryPaused = true;
      this.coreTelemetryStarted = false;
      if (this._poll_timer) {
        window.clearInterval(this._poll_timer);
        this._poll_timer = null;
      }
      if (this._ws) {
        const ws = this._ws;
        this._ws = null;
        ws.close();
      }
      this._ws_connected = false;
    },

    startAiMonitorRefresh() {
      this.stopAiMonitorRefresh();
      this.loadAiMonitor();
      this.aiMonitorTimer = window.setInterval(() => {
        if (this.view === 'ai-monitor' && document.visibilityState === 'visible') {
          this.loadAiMonitor();
        }
      }, 15000);
    },

    stopAiMonitorRefresh() {
      if (this.aiMonitorTimer) {
        window.clearInterval(this.aiMonitorTimer);
        this.aiMonitorTimer = null;
      }
    },

    async loadAiMonitor() {
      if (this.aiMonitorLoading) return;
      this.aiMonitorLoading = true;
      this.aiMonitorError = null;
      try {
        const r = await fetch('/api/ai-monitor', { cache: 'no-store' });
        const data = await r.json();
        if (!r.ok || data.error) throw new Error(data.error || `HTTP ${r.status}`);
        this.aiMonitor = data;
        if (data.version) this.state.version = data.version;
      } catch (e) {
        this.aiMonitorError = e.message || String(e);
      } finally {
        this.aiMonitorLoading = false;
      }
    },

    async runAiSpeedTest() {
      if (this.aiSpeedTestBusy) return;
      this.aiSpeedTestBusy = true;
      this.aiSpeedTestError = null;
      try {
        const r = await fetch('/mcp', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            jsonrpc: '2.0',
            id: `ai-speed-${Date.now()}`,
            method: 'tools/call',
            params: { name: 'ai_monitor_speed_test', arguments: {} },
          }),
        });
        const data = await r.json();
        if (data.error) throw new Error(data.error.message || '測速失敗');
        const text = data.result?.content?.[0]?.text;
        this.aiSpeedTest = typeof text === 'string' ? JSON.parse(text) : data.result;
      } catch (e) {
        this.aiSpeedTestError = e.message || String(e);
      } finally {
        this.aiSpeedTestBusy = false;
      }
    },

    aiSelectedRoute(family) {
      return (this.aiMonitor?.network?.default_routes || [])
        .find(route => route.family === family && route.selected) || null;
    },

    aiEvidenceLabel(level) {
      return {
        MEASURED: '實測',
        INFERRED: '推估',
        MISSING_PROOF: '缺證據',
      }[level] || '缺證據';
    },

    aiEvidenceClass(level) {
      return `evidence-${String(level || 'MISSING_PROOF').toLowerCase().replace('_', '-')}`;
    },

    codexSupervisorStatusClass(status) {
      if (['HEALTHY', 'HEALTHY_IDLE', 'HEALTHY_RUNNING', 'COMPLETED'].includes(status)) return 'status-passed';
      if (['RECOVERABLE', 'RECOVERABLE_INTERRUPTION'].includes(status)) return 'status-running';
      if (['MISSING_PROOF', 'SCAN_FAILED', 'HIGH_RISK_AUTH_UNCLEAR', 'CONTROL_STATE_MISMATCH', 'UNREADABLE_OR_TIMEOUT'].includes(status)) return 'status-failed';
      if (['WAITING_FOR_HEARTBEAT', 'SCAN_PARTIAL', 'WAITING_CORRECT_TIME', 'SUSPECTED_STALL', 'UNKNOWN'].includes(status)) return 'status-idle';
      return 'status-timeout';
    },

    codexSupervisorActionLabel(action) {
      return {
        NONE: '不動作',
        OBSERVE_AGAIN: '下輪再觀察',
        CONTINUE_ONCE: '可續推一次',
        NOTIFY_USER: '需要你',
        FAIL_CLOSED: '停止並回報',
      }[action] || '不動作';
    },

    codexCoverageLabel(surface) {
      return {
        CODE: '程式碼',
        UNIT_TEST: '單元測試',
        INTEGRATION: '整合',
        RUNTIME: '運行',
        WEB_UI: 'Web UI',
        MOBILE_UI: '手機 UI',
        SIMULATOR: '模擬器',
        PHYSICAL_DEVICE: '真機',
        DEPLOYMENT: '部署',
      }[surface] || surface;
    },

    codexCoverageClass(status) {
      if (status === 'PASS') return 'coverage-pass';
      if (status === 'FAIL') return 'coverage-fail';
      if (status === 'NOT_APPLICABLE') return 'coverage-na';
      return 'coverage-gap';
    },

    formatCompactNumber(value) {
      const n = Number(value || 0);
      return new Intl.NumberFormat('zh-TW', { notation: 'compact', maximumFractionDigits: 1 }).format(n);
    },

    formatMonitorTime(value) {
      if (!value) return '—';
      const d = new Date(value);
      return Number.isNaN(d.getTime()) ? '—' : d.toLocaleTimeString('zh-TW', { hour12: false });
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

    /// #266: Deterministic pass rate over the last 7d (saved replay plus
    /// fixture-only assertions; both avoid LLM cost). 0–1 ratio.
    get kpi_replay_rate() {
      return this.state.replay_health?.success_rate_deterministic || 0;
    },

    get kpi_replay_has_data() {
      return this.state.replay_health?.has_recent_data === true;
    },

    /// #266: Drift in percentage points (last 7d minus prior 7d).  Returns
    /// null when there isn't enough history for the comparison window —
    /// callers render "—" or hide the sub-line in that case.
    get kpi_replay_drift_pp() {
      const d = this.state.replay_health?.deterministic_drift;
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

    async callMcpJson(name, args = {}) {
      const r = await fetch('/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          id: Date.now(),
          method: 'tools/call',
          params: { name, arguments: args },
        }),
      });
      if (!r.ok) throw new Error('HTTP ' + r.status);
      const env = await r.json();
      if (env.error) throw new Error(env.error.message);
      const text = env.result?.content?.[0]?.text;
      if (typeof text === 'string') {
        try { return JSON.parse(text); }
        catch (_) { return { text }; }
      }
      return env.result;
    },

    async loadIosStatus() {
      this.iosStatusLoading = true;
      this.iosError = null;
      try {
        this.iosStatus = await this.callMcpJson('ios_dev_location_status', {});
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosStatusLoading = false;
      }
    },

    async loadIosShareInfo() {
      try {
        const res = await fetch('/api/ios-location-share');
        this.iosShareInfo = await res.json();
      } catch (_) {
        this.iosShareInfo = {
          status: 'LAN_URL_UNAVAILABLE',
          loopbackUrl: `${window.location.origin}/ios-location-report`,
          lanUrl: null,
        };
      }
    },

    iosPointArgs() {
      const lat = Number(this.iosLat);
      const lng = Number(this.iosLng);
      if (!Number.isFinite(lat) || lat < -90 || lat > 90) throw new Error('緯度必須介於 -90 到 90');
      if (!Number.isFinite(lng) || lng < -180 || lng > 180) throw new Error('經度必須介於 -180 到 180');
      return {
        backend: 'auto',
        lat,
        lng,
        targetApp: this.iosTargetApp,
        note: this.iosNote,
      };
    },

    async iosReadCurrentLocation() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosCurrentResult = await this.callMcpJson('ios_dev_location_current', {
          note: this.iosNote || 'operator-ui current location probe',
        });
        const point = this.iosCurrentResult.point;
        if (point && Number.isFinite(Number(point.lat)) && Number.isFinite(Number(point.lng))) {
          this.iosLat = String(point.lat);
          this.iosLng = String(point.lng);
          this.iosConfirmCurrentLocation('device_read');
          this.iosToast = '已讀取手機位置並更新漫遊中心';
        } else {
          this.iosLocationConfirmed = false;
          this.iosToast = this.iosCurrentResult.status || '手機位置無法直接讀取';
        }
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    iosConfirmCurrentLocation(source = 'manual') {
      this.iosCurrentCenter();
      this.iosLocationConfirmed = true;
      this.iosLocationSource = source;
      this.iosMapPickMode = 'route';
      this.iosRouteResult = null;
      this.iosRouteRunResult = null;
      this.iosToast = source === 'device_read'
        ? '已用手機讀取位置作為漫遊起點'
        : source === 'phone_report'
          ? '已用手機回報位置作為漫遊起點'
        : source === 'city_center'
          ? '已用城市中心作為漫遊起點'
        : source === 'preset'
          ? '已用常用起點作為漫遊起點'
        : '已使用此座標作為漫遊起點';
      window.setTimeout(() => this.iosUpdateLeafletMap({ fit: true }), 0);
    },

    iosInvalidateLocation() {
      this.iosLocationConfirmed = false;
      this.iosLocationSource = 'manual';
      this.iosRouteResult = null;
      this.iosRouteRunResult = null;
    },

    async iosLoadReportedLocation() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosLatestReportResult = await this.callMcpJson('ios_location_report_latest', {});
        const point = this.iosLatestReportResult.point || this.iosLatestReportResult.report;
        const lat = Number(point?.lat);
        const lng = Number(point?.lng);
        if (!Number.isFinite(lat) || !Number.isFinite(lng)) {
          throw new Error(this.iosLatestReportResult.nextAction || '尚無可用手機回報位置');
        }
        this.iosLat = String(lat);
        this.iosLng = String(lng);
        this.iosConfirmCurrentLocation('phone_report');
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    iosUseCityCenter() {
      this.iosError = null;
      const center = this.iosCityCenterPoint(this.iosCity);
      if (!center) {
        this.iosError = '目前城市不在內建中心清單，請改用手動 LAT/LNG 作為起點';
        return;
      }
      this.iosLat = String(center.lat);
      this.iosLng = String(center.lng);
      this.iosConfirmCurrentLocation('city_center');
    },

    iosSelectedPreset() {
      return this.iosOriginPresets.find((preset) => preset.id === this.iosSelectedPresetId)
        || this.iosOriginPresets[0];
    },

    iosUsePresetOrigin(preset = this.iosSelectedPreset()) {
      if (!preset) return;
      this.iosError = null;
      this.iosCity = preset.city;
      this.iosLat = String(preset.lat);
      this.iosLng = String(preset.lng);
      this.iosRoamRadiusMeters = preset.radiusMeters;
      this.iosConfirmCurrentLocation('preset');
    },

    iosSetDuration(seconds) {
      this.iosDurationSeconds = seconds;
      this.iosRouteResult = null;
      this.iosRouteRunResult = null;
      this.iosUpdateLeafletMap();
    },

    iosSetMovementProfile(profile) {
      this.iosMovementProfile = profile;
      this.iosRouteResult = null;
      this.iosRouteRunResult = null;
      this.iosUpdateLeafletMap();
    },

    iosEnableOriginMapPick() {
      this.iosMapPickMode = 'origin';
      this.iosToast = '請在地圖上點擊新的漫遊起點';
    },

    async iosUsePresetAndPlan() {
      this.iosUsePresetOrigin();
      await this.iosPlanRoute();
    },

    async iosSetLocation() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosResult = await this.callMcpJson('ios_dev_location_set', this.iosPointArgs());
        this.iosToast = this.iosResult.status || '已送出定位設定';
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    async iosResetLocation() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosResult = await this.callMcpJson('ios_dev_location_reset', {
          backend: 'auto',
          note: this.iosNote || 'operator-ui reset',
        });
        this.iosToast = this.iosResult.status || '已送出 reset';
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    async iosPlanRoute() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        if (!this.iosLocationConfirmed) throw new Error('請先讀取或確認目前位置，再分析街道');
        const center = this.iosCurrentCenter();
        this.iosRouteResult = await this.callMcpJson('ios_dev_location_roam_plan', {
          routeSource: this.iosRouteSource,
          center,
          city: this.iosCity,
          radiusMeters: Number(this.iosRoamRadiusMeters),
          waypoints: Number(this.iosRoamWaypoints),
          roadProvider: this.iosRoadProvider,
          allowSyntheticFallback: Boolean(this.iosAllowSyntheticFallback),
          movementProfile: this.iosMovementProfile,
          speedKmh: Number(this.iosSpeedKmh),
          durationSeconds: Number(this.iosDurationSeconds),
          tickSeconds: Number(this.iosTickSeconds),
          includeSchedule: Boolean(this.iosIncludeSchedule),
          note: this.iosNote,
        });
        this.iosRouteRunResult = null;
        if (Array.isArray(this.iosRouteResult.roadRoutePoints) && this.iosRouteResult.roadRoutePoints.length > 1) {
          this.iosSetRoutePoints(this.iosRouteResult.roadRoutePoints);
        } else {
          this.iosUpdateLeafletMap({ fit: true });
        }
        this.iosToast = `${this.iosRouteResult.status || 'READY'} · 已分析街道並產生播放座標`;
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    async iosPlanManualRoute() {
      this.iosBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        const points = this.iosRoutePointArray();
        if (!Array.isArray(points) || points.length < 2) throw new Error('道路 polyline 至少需要兩個點');
        this.iosRouteResult = await this.callMcpJson('ios_dev_location_route_plan', {
          routeSource: 'manual_road_polyline',
          points,
          movementProfile: this.iosMovementProfile,
          speedKmh: Number(this.iosSpeedKmh),
          durationSeconds: Number(this.iosDurationSeconds),
          tickSeconds: Number(this.iosTickSeconds),
          includeSchedule: Boolean(this.iosIncludeSchedule),
          note: this.iosNote,
        });
        this.iosRouteRunResult = null;
        this.iosUpdateLeafletMap({ fit: true });
        this.iosToast = this.iosRouteResult.status || '已建立手動路線計畫';
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosBusy = false;
      }
    },

    async iosStartRoutePlayback() {
      const routeId = this.iosRouteResult?.routeId;
      if (!routeId) {
        this.iosError = '請先產生城市漫遊或手動路線，取得 routeId 後再開始播放';
        return;
      }
      this.iosRouteRunBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosRouteRunResult = await this.callMcpJson('ios_dev_location_route_start', {
          routeId,
          backend: 'pymobiledevice3',
          userspace: true,
        });
        this.iosAutoResetDone = false;
        if (this.iosRouteAutoRefresh && this.iosRouteRunResult?.routeRunId) {
          this.iosStartRouteRunPolling();
        }
        this.iosUpdateLeafletMap();
        this.iosToast = this.iosRouteRunResult.status || '已開始播放 iOS 路線';
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosRouteRunBusy = false;
      }
    },

    async iosRefreshRouteRunStatus() {
      const routeRunId = this.iosRouteRunResult?.routeRunId;
      this.iosRouteRunBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        const args = routeRunId ? { routeRunId } : {};
        this.iosRouteRunResult = await this.callMcpJson('ios_dev_location_route_run_status', args);
        this.iosUpdateLeafletMap();
        this.iosToast = `${this.iosRouteRunResult.status || 'ROUTE_RUN_STATUS'} · ${this.iosRouteRunResult.setLocationCount || 0} points`;
        await this.iosHandleRouteRunCompletion();
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosRouteRunBusy = false;
      }
    },

    async iosStopRoutePlayback() {
      const routeRunId = this.iosRouteRunResult?.routeRunId;
      const pid = this.iosRouteRunResult?.pid;
      if (!routeRunId && !pid) {
        this.iosError = '沒有可停止的 routeRunId 或 pid';
        return;
      }
      this.iosRouteRunBusy = true;
      this.iosError = null;
      this.iosToast = null;
      try {
        this.iosStopRouteRunPolling();
        const args = routeRunId ? { routeRunId } : { pid };
        const stopped = await this.callMcpJson('ios_dev_location_route_stop', args);
        this.iosRouteRunResult = {
          ...this.iosRouteRunResult,
          ...stopped,
          stillRunning: stopped.stillRunning === true,
          completed: stopped.completed === true,
        };
        this.iosUpdateLeafletMap();
        this.iosToast = stopped.status || '已停止 iOS 路線播放';
      } catch (e) {
        this.iosError = e.message || String(e);
      } finally {
        this.iosRouteRunBusy = false;
      }
    },

    iosStartRouteRunPolling() {
      this.iosStopRouteRunPolling();
      this.iosRouteRunPollTimer = window.setInterval(async () => {
        if (!this.iosRouteRunResult?.routeRunId || this.iosRouteRunBusy) return;
        await this.iosRefreshRouteRunStatus();
      }, 5000);
    },

    iosStopRouteRunPolling() {
      if (this.iosRouteRunPollTimer) {
        window.clearInterval(this.iosRouteRunPollTimer);
        this.iosRouteRunPollTimer = null;
      }
    },

    async iosHandleRouteRunCompletion() {
      if (!this.iosRouteRunResult?.completed) return;
      this.iosStopRouteRunPolling();
      if (!this.iosAutoResetAfterPlayback || this.iosAutoResetDone) return;
      this.iosAutoResetDone = true;
      await this.iosResetLocation();
      this.iosToast = '播放完成，已自動 reset developer simulated location';
    },

    iosReportPageUrl() {
      return this.iosShareInfo?.lanUrl || `${window.location.origin}/ios-location-report`;
    },

    async iosCopyReportUrl() {
      const url = this.iosReportPageUrl();
      try {
        await navigator.clipboard.writeText(url);
        this.iosToast = '已複製手機回報 URL';
      } catch (_) {
        this.iosToast = url;
      }
    },

    iosCurrentCenter() {
      const lat = Number(this.iosLat);
      const lng = Number(this.iosLng);
      if (!Number.isFinite(lat) || lat < -90 || lat > 90) throw new Error('目前位置緯度必須介於 -90 到 90');
      if (!Number.isFinite(lng) || lng < -180 || lng > 180) throw new Error('目前位置經度必須介於 -180 到 180');
      return { lat, lng };
    },

    iosCityCenterPoint(city) {
      const key = String(city || '').trim().toLowerCase();
      const centers = {
        taipei: { lat: 25.033964, lng: 121.564468 },
        '台北': { lat: 25.033964, lng: 121.564468 },
        '臺北': { lat: 25.033964, lng: 121.564468 },
        new_taipei: { lat: 25.011002, lng: 121.465661 },
        'new taipei': { lat: 25.011002, lng: 121.465661 },
        '新北': { lat: 25.011002, lng: 121.465661 },
        taichung: { lat: 24.147736, lng: 120.673648 },
        '台中': { lat: 24.147736, lng: 120.673648 },
        '臺中': { lat: 24.147736, lng: 120.673648 },
        kaohsiung: { lat: 22.627278, lng: 120.301435 },
        '高雄': { lat: 22.627278, lng: 120.301435 },
        chiang_mai: { lat: 18.788343, lng: 98.985300 },
        'chiang mai': { lat: 18.788343, lng: 98.985300 },
        '清邁': { lat: 18.788343, lng: 98.985300 },
        '清迈': { lat: 18.788343, lng: 98.985300 },
      };
      return centers[key] || null;
    },

    iosRoutePointArray() {
      const raw = JSON.parse(this.iosRoutePoints || '[]');
      if (!Array.isArray(raw)) return [];
      return raw
        .map((p) => ({ lat: Number(p.lat), lng: Number(p.lng) }))
        .filter((p) =>
          Number.isFinite(p.lat) && Number.isFinite(p.lng)
          && p.lat >= -90 && p.lat <= 90
          && p.lng >= -180 && p.lng <= 180
        );
    },

    iosSetRoutePoints(points) {
      const clean = points.map((p) => ({
        lat: Number(p.lat.toFixed(6)),
        lng: Number(p.lng.toFixed(6)),
      }));
      this.iosRoutePoints = JSON.stringify(clean, null, 2);
      window.setTimeout(() => this.iosUpdateLeafletMap({ fit: clean.length > 1 }), 0);
    },

    iosLoadDemoRoute() {
      this.iosSetRoutePoints([
        { lat: 25.033964, lng: 121.564468 },
        { lat: 25.037820, lng: 121.567120 },
        { lat: 25.041260, lng: 121.572640 },
        { lat: 25.046740, lng: 121.577680 },
        { lat: 25.052300, lng: 121.583520 },
        { lat: 25.056210, lng: 121.581182 },
        { lat: 25.064000, lng: 121.604468 },
      ]);
      this.iosRouteSource = 'manual_road_polyline';
      this.iosToast = '已載入示範道路 polyline';
    },

    iosClearRoute() {
      this.iosSetRoutePoints([]);
      this.iosRouteResult = null;
      this.iosToast = '已清空路線';
    },

    iosUndoRoutePoint() {
      const points = this.iosRoutePointArray();
      points.pop();
      this.iosSetRoutePoints(points);
    },

    iosRouteDistanceMeters() {
      const points = this.iosRoutePointArray();
      let total = 0;
      for (let i = 1; i < points.length; i++) total += this.iosDistanceMeters(points[i - 1], points[i]);
      return total;
    },

    iosRequestedDistanceMeters() {
      const speed = Number(this.iosSpeedKmh) || 0;
      const duration = Number(this.iosDurationSeconds) || 0;
      return speed * 1000 / 3600 * duration;
    },

    iosDistanceMeters(a, b) {
      const rad = Math.PI / 180;
      const lat1 = a.lat * rad;
      const lat2 = b.lat * rad;
      const dlat = (b.lat - a.lat) * rad;
      const dlng = (b.lng - a.lng) * rad;
      const h = Math.sin(dlat / 2) ** 2
        + Math.cos(lat1) * Math.cos(lat2) * Math.sin(dlng / 2) ** 2;
      return 2 * 6371000 * Math.asin(Math.sqrt(h));
    },

    iosRouteOriginalStartPoint() {
      return this.iosRouteResult?.center || this.iosCurrentCenterSafe();
    },

    iosRouteSnappedStartPoint() {
      const road = this.iosRouteResult?.roadRoutePoints;
      if (Array.isArray(road) && road.length > 0) return road[0];
      const previewFirst = this.iosRouteResult?.preview?.first;
      if (Array.isArray(previewFirst) && previewFirst.length > 0) return previewFirst[0];
      return null;
    },

    iosRouteSnapOffsetText() {
      const original = this.iosRouteOriginalStartPoint();
      const snapped = this.iosRouteSnappedStartPoint();
      if (!original || !snapped) return '-';
      return `${this.iosDistanceMeters(original, snapped).toFixed(1)} m`;
    },

    iosMapBounds() {
      const points = this.iosRoutePointArray();
      const center = {
        lat: Number(this.iosLat) || 18.788343,
        lng: Number(this.iosLng) || 98.985300,
      };
      const seed = points.length ? points : [center];
      let minLat = Math.min(...seed.map((p) => p.lat));
      let maxLat = Math.max(...seed.map((p) => p.lat));
      let minLng = Math.min(...seed.map((p) => p.lng));
      let maxLng = Math.max(...seed.map((p) => p.lng));
      const latPad = Math.max((maxLat - minLat) * 0.18, 0.004);
      const lngPad = Math.max((maxLng - minLng) * 0.18, 0.004);
      return {
        minLat: minLat - latPad,
        maxLat: maxLat + latPad,
        minLng: minLng - lngPad,
        maxLng: maxLng + lngPad,
      };
    },

    iosMapProject(point) {
      const b = this.iosMapBounds();
      const width = Math.max(b.maxLng - b.minLng, 0.000001);
      const height = Math.max(b.maxLat - b.minLat, 0.000001);
      return {
        x: 40 + ((point.lng - b.minLng) / width) * 920,
        y: 480 - ((point.lat - b.minLat) / height) * 440,
      };
    },

    iosMapUnproject(x, y) {
      const b = this.iosMapBounds();
      return {
        lat: b.minLat + ((480 - y) / 440) * (b.maxLat - b.minLat),
        lng: b.minLng + ((x - 40) / 920) * (b.maxLng - b.minLng),
      };
    },

    iosMapPolyline() {
      return this.iosRoutePointArray()
        .map((p) => this.iosMapProject(p))
        .map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`)
        .join(' ');
    },

    iosRouteProgressRatio() {
      const run = this.iosRouteRunResult || {};
      const duration = Number(run.durationSeconds);
      const elapsed = Number(run.elapsedSeconds);
      if (Number.isFinite(duration) && duration > 0 && Number.isFinite(elapsed)) {
        return Math.max(0, Math.min(1, elapsed / duration));
      }
      const total = Number(run.scheduledPointCount);
      const count = Number(run.setLocationCount);
      if (Number.isFinite(total) && total > 0 && Number.isFinite(count)) {
        return Math.max(0, Math.min(1, count / total));
      }
      return 0;
    },

    iosMapPointAtProgress() {
      const points = this.iosRoutePointArray();
      if (points.length === 0) return null;
      if (this.iosRouteRunResult?.lastPoint) return this.iosRouteRunResult.lastPoint;
      const idx = Math.min(points.length - 1, Math.round(this.iosRouteProgressRatio() * (points.length - 1)));
      return points[idx];
    },

    iosMapSegmentPolyline(kind) {
      const points = this.iosRoutePointArray();
      if (points.length < 2) return '';
      const ratio = this.iosRouteProgressRatio();
      if (!this.iosRouteRunResult?.routeRunId && kind === 'played') return '';
      if (!this.iosRouteRunResult?.routeRunId && kind === 'remaining') return this.iosMapPolyline();
      const split = Math.max(0, Math.min(points.length - 1, Math.round(ratio * (points.length - 1))));
      const selected = kind === 'played'
        ? points.slice(0, split + 1)
        : points.slice(split);
      return selected
        .map((p) => this.iosMapProject(p))
        .map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`)
        .join(' ');
    },

    iosMapPoints() {
      return this.iosRoutePointArray().map((point, idx) => ({
        ...point,
        idx,
        ...this.iosMapProject(point),
      }));
    },

    iosMapMarkersSvg() {
      const points = this.iosMapPoints();
      const step = Math.max(1, Math.ceil(points.length / 32));
      const markerSvg = points.filter((p) =>
        p.idx === 0 || p.idx === points.length - 1 || p.idx % step === 0
      ).map((p) => {
        const role = p.idx === 0 ? 'start' : (p.idx === points.length - 1 ? 'end' : '');
        const label = p.idx === 0 ? 'START' : (p.idx === points.length - 1 ? 'END' : '');
        return [
          `<circle class="ios-map-point ${role}" cx="${p.x.toFixed(1)}" cy="${p.y.toFixed(1)}" r="8"></circle>`,
          label ? `<text class="ios-map-point-label" x="${(p.x + 12).toFixed(1)}" y="${(p.y - 10).toFixed(1)}">${label}</text>` : '',
        ].join('');
      }).join('');
      const current = this.iosMapPointAtProgress();
      if (!current || !this.iosRouteRunResult?.routeRunId) return markerSvg;
      const projected = this.iosMapProject(current);
      return [
        markerSvg,
        `<circle class="ios-map-current-pulse" cx="${projected.x.toFixed(1)}" cy="${projected.y.toFixed(1)}" r="15"></circle>`,
        `<circle class="ios-map-point current" cx="${projected.x.toFixed(1)}" cy="${projected.y.toFixed(1)}" r="7"></circle>`,
        `<text class="ios-map-point-label" x="${(projected.x + 12).toFixed(1)}" y="${(projected.y + 22).toFixed(1)}">NOW</text>`,
      ].join('');
    },

    iosMapClick(event) {
      const rect = event.currentTarget.getBoundingClientRect();
      const x = ((event.clientX - rect.left) / rect.width) * 1000;
      const y = ((event.clientY - rect.top) / rect.height) * 520;
      if (x < 40 || x > 960 || y < 40 || y > 480) return;
      const points = this.iosRoutePointArray();
      points.push(this.iosMapUnproject(x, y));
      this.iosSetRoutePoints(points);
      this.iosToast = `已新增道路點 #${points.length}`;
    },

    iosInitLeafletMap() {
      if (this.iosLeafletMap) {
        window.setTimeout(() => this.iosLeafletMap.invalidateSize(), 0);
        this.iosUpdateLeafletMap();
        return;
      }
      const element = document.getElementById('ios-leaflet-map');
      if (!element) return;
      if (!window.L) {
        this.iosLeafletLoadAttempts += 1;
        if (this.iosLeafletLoadAttempts <= 20) {
          this.iosLeafletError = null;
          window.setTimeout(() => this.iosInitLeafletMap(), 250);
          return;
        }
        this.iosLeafletError = 'Leaflet 未載入';
        return;
      }
      try {
        const center = this.iosCurrentCenterSafe();
        const map = window.L.map(element, {
          zoomControl: true,
          attributionControl: false,
          preferCanvas: true,
        }).setView([center.lat, center.lng], 14);
        window.L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
          maxZoom: 19,
          crossOrigin: true,
          attribution: 'OpenStreetMap',
        }).addTo(map);
        window.L.control.attribution({ prefix: false }).addAttribution('OpenStreetMap').addTo(map);
        this.iosLeafletMap = map;
        this.iosLeafletLayers = {
          shadow: window.L.polyline([], {
            color: 'rgba(0,255,163,0.18)',
            weight: 14,
            opacity: 1,
            lineCap: 'round',
            lineJoin: 'round',
            interactive: false,
          }).addTo(map),
          remaining: window.L.polyline([], {
            color: 'rgba(180,190,190,0.72)',
            weight: 5,
            opacity: 1,
            lineCap: 'round',
            lineJoin: 'round',
          }).addTo(map),
          played: window.L.polyline([], {
            color: '#00FFA3',
            weight: 6,
            opacity: 1,
            lineCap: 'round',
            lineJoin: 'round',
          }).addTo(map),
          markers: [],
        };
        map.on('click', (event) => this.iosLeafletAddPoint(event.latlng));
        this.iosLeafletReady = true;
        this.iosLeafletError = null;
        window.setTimeout(() => {
          map.invalidateSize();
          this.iosUpdateLeafletMap({ fit: true });
        }, 120);
      } catch (e) {
        this.iosLeafletError = e.message || String(e);
      }
    },

    iosLeafletLatLngs(points = this.iosRoutePointArray()) {
      return points.map((p) => [p.lat, p.lng]);
    },

    iosLeafletProgressSegments() {
      const points = this.iosRoutePointArray();
      if (points.length < 2) return { played: [], remaining: points };
      if (!this.iosRouteRunResult?.routeRunId) return { played: [], remaining: points };
      const split = Math.max(0, Math.min(points.length - 1, Math.round(this.iosRouteProgressRatio() * (points.length - 1))));
      return {
        played: points.slice(0, split + 1),
        remaining: points.slice(split),
      };
    },

    iosLeafletDivIcon(className, label) {
      return window.L.divIcon({
        className: `ios-leaflet-marker ${className}`,
        html: `<span>${label}</span>`,
        iconSize: [28, 28],
        iconAnchor: [14, 14],
      });
    },

    iosClearLeafletMarkers() {
      if (!this.iosLeafletMap || !this.iosLeafletLayers) return;
      for (const marker of this.iosLeafletLayers.markers) {
        marker.removeFrom(this.iosLeafletMap);
      }
      this.iosLeafletLayers.markers = [];
    },

    iosAddLeafletMarker(point, className, label, title) {
      if (!this.iosLeafletMap || !window.L || !point) return;
      const marker = window.L.marker([point.lat, point.lng], {
        icon: this.iosLeafletDivIcon(className, label),
        title,
        keyboard: false,
      }).addTo(this.iosLeafletMap);
      this.iosLeafletLayers.markers.push(marker);
    },

    iosUpdateLeafletMap(options = {}) {
      if (!this.iosLeafletMap || !this.iosLeafletLayers || !window.L) return;
      const points = this.iosRoutePointArray();
      const segments = this.iosLeafletProgressSegments();
      this.iosLeafletLayers.shadow.setLatLngs(this.iosLeafletLatLngs(points));
      this.iosLeafletLayers.remaining.setLatLngs(this.iosLeafletLatLngs(segments.remaining));
      this.iosLeafletLayers.played.setLatLngs(this.iosLeafletLatLngs(segments.played));
      this.iosClearLeafletMarkers();
      if (points.length > 0) {
        this.iosAddLeafletMarker(points[0], 'start', 'S', 'START');
        this.iosAddLeafletMarker(points[points.length - 1], 'end', 'E', 'END');
      } else {
        this.iosAddLeafletMarker(this.iosCurrentCenterSafe(), 'start', 'S', 'START');
      }
      const current = this.iosMapPointAtProgress();
      if (current && this.iosRouteRunResult?.routeRunId) {
        this.iosAddLeafletMarker(current, 'current', '●', 'NOW');
      }
      if (options.fit && points.length > 1) {
        const bounds = window.L.latLngBounds(this.iosLeafletLatLngs(points));
        this.iosLeafletMap.fitBounds(bounds.pad(0.18), { animate: false });
      } else if (options.fit) {
        const center = this.iosCurrentCenterSafe();
        this.iosLeafletMap.setView([center.lat, center.lng], 14, { animate: false });
      }
      window.setTimeout(() => this.iosLeafletMap?.invalidateSize(), 0);
    },

    iosLeafletAddPoint(latlng) {
      if (this.iosMapPickMode === 'origin') {
        this.iosLat = String(Number(latlng.lat.toFixed(6)));
        this.iosLng = String(Number(latlng.lng.toFixed(6)));
        this.iosMapPickMode = 'route';
        this.iosConfirmCurrentLocation('map');
        return;
      }
      const points = this.iosRoutePointArray();
      points.push({ lat: latlng.lat, lng: latlng.lng });
      this.iosSetRoutePoints(points);
      this.iosRouteSource = 'manual_road_polyline';
      this.iosToast = `已在真實地圖新增道路點 #${points.length}`;
    },

    iosRouteDistanceText() {
      return (this.iosRouteDistanceMeters() / 1000).toFixed(2) + ' km';
    },

    iosRequestedDistanceText() {
      return (this.iosRequestedDistanceMeters() / 1000).toFixed(2) + ' km';
    },

    iosStatusClass(status) {
      const s = String(status || '').toUpperCase();
      if (s.includes('AVAILABLE') || s === 'RUNNING' || s === 'RESTORED') return '';
      if (s.includes('VERIFY') || s.includes('AMBIGUOUS') || s.includes('REQUIRES') || s.includes('STALLED')) return 'badge-soft-warn';
      if (s.includes('FAILED') || s.includes('MISSING') || s.includes('ERROR')) return 'badge-soft-danger';
      return 'badge-soft-info';
    },

    iosRoutePlayableLabel() {
      if (!this.iosRouteResult?.routeId) return '未產生';
      if (this.iosRouteResult.routePlayable === true || this.iosRouteQualityAcceptable()) return '可播放';
      return '需重新生成';
    },

    iosRoutePlayableClass() {
      if (!this.iosRouteResult?.routeId) return 'badge-soft-info';
      return (this.iosRouteResult.routePlayable === true || this.iosRouteQualityAcceptable())
        ? ''
        : 'badge-soft-warn';
    },

    iosPlaybackQualityClass(value) {
      const q = String(value || this.iosRouteRunResult?.playbackAnalysis?.quality || '').toUpperCase();
      if (q.includes('GOOD')) return '';
      if (q.includes('RUNNING')) return 'badge-soft-info';
      if (q.includes('STALLED') || q.includes('REPEATS') || q.includes('BACKTRACKS')) return 'badge-soft-danger';
      if (q.includes('OVERSPEED') || q.includes('SHORT')) return 'badge-soft-warn';
      return 'badge-soft-info';
    },

    iosOperationSummaryTitle() {
      const run = this.iosRouteRunResult || {};
      if (run.stalled) return '播放卡住';
      if (run.stillRunning === true) return '播放中';
      if (run.completed === true) return '播放完成';
      if (this.iosRouteResult?.routeId && this.iosRouteQualityAcceptable()) return '路線可播放';
      if (this.iosRouteResult?.routeId) return '路線需重新生成';
      if (this.iosLocationConfirmed) return '位置已確認';
      return '等待位置';
    },

    iosOperationSummaryClass() {
      const title = this.iosOperationSummaryTitle();
      if (title.includes('完成') || title.includes('可播放')) return '';
      if (title.includes('播放中') || title.includes('位置')) return 'badge-soft-info';
      if (title.includes('卡住')) return 'badge-soft-danger';
      return 'badge-soft-warn';
    },

    iosPrimaryPlaybackText() {
      if (this.iosRouteRunBusy) return '啟動中…';
      if (this.iosRouteRunResult?.stillRunning === true) return '播放中';
      if (!this.iosRouteResult?.routeId) return '先生成路線';
      if (!this.iosRouteQualityAcceptable()) return '重新生成後播放';
      if (!this.iosDeviceConnected()) return '等待 iPhone';
      return '開始播放';
    },

    iosBackendLabel() {
      return this.iosStatus?.preferredBackend || 'unknown';
    },

    iosDeviceSummary() {
      const src = this.iosStatus?.connectedDevices?.sources?.[0];
      if (!src) return '尚未取得裝置摘要';
      if (src.deviceCount === 0) return `${src.source || 'device probe'}: 0 devices`;
      return src.stdoutExcerpt || src.stderrExcerpt || '已偵測來源，但沒有輸出';
    },

    iosResultStatusClass(result) {
      return this.iosStatusClass(result?.status || result?.commandStatus);
    },

    iosRouteReadyForPlayback() {
      const status = String(this.iosRouteResult?.status || '').toUpperCase();
      return Boolean(this.iosRouteResult?.routeId)
        && this.iosLocationConfirmed
        && this.iosDeviceConnected()
        && this.iosPlaybackCoordinateCount() > 1
        && this.iosRouteQualityAcceptable()
        && !status.includes('INSUFFICIENT')
        && !status.includes('FAILED')
        && !status.includes('ERROR');
    },

    iosPrerequisites() {
      const routeStatus = String(this.iosRouteResult?.status || '').toUpperCase();
      return [
        {
          label: 'USB iPhone',
          ok: this.iosDeviceConnected(),
          value: this.iosDeviceConnected() ? '已偵測' : '未偵測',
        },
        {
          label: '目前位置',
          ok: this.iosLocationConfirmed,
          value: this.iosLocationConfirmed ? this.iosPointText(this.iosCurrentCenterSafe()) : '未確認',
        },
        {
          label: '道路路線',
          ok: Boolean(this.iosRouteResult?.routeId) && this.iosRouteQualityAcceptable() && !routeStatus.includes('FAILED') && !routeStatus.includes('ERROR'),
          value: this.iosRouteResult?.routeId || '尚未產生',
        },
        {
          label: '播放座標',
          ok: this.iosPlaybackCoordinateCount() > 1,
          value: `${this.iosPlaybackCoordinateCount()} points`,
        },
        {
          label: 'Backend',
          ok: this.iosBackendLabel() !== 'unknown',
          value: this.iosBackendLabel(),
        },
      ];
    },

    iosDeviceConnected() {
      return this.iosStatus?.connectedDevices?.available === true;
    },

    iosRoutePlaybackDisabledReason() {
      if (this.iosRouteRunBusy) return '路線播放正在啟動';
      if (!this.iosLocationConfirmed) return '請先讀取或確認目前位置';
      if (!this.iosRouteResult?.routeId) return '請先產生城市漫遊或手動路線';
      if (!this.iosDeviceConnected()) return '尚未偵測到 USB iPhone';
      if (this.iosPlaybackCoordinateCount() <= 1) return '尚未產生可播放座標';
      if (!this.iosRouteQualityAcceptable()) return '路線品質不合格，請縮小半徑、換起點或重新產生';
      const status = String(this.iosRouteResult?.status || '').toUpperCase();
      if (status.includes('INSUFFICIENT')) return '路線長度不足，請增加半徑或 duration';
      if (status.includes('FAILED') || status.includes('ERROR')) return '路線狀態不可播放';
      return '開始在 iPhone 播放 GPX 路線';
    },

    iosRouteQualityAcceptable() {
      if (!this.iosRouteResult?.routeId) return false;
      if (this.iosRouteResult.routePlayable === true) return true;
      if (this.iosRouteResult.routePlayable === false) return false;
      const status = String(this.iosRouteResult?.status || '').toUpperCase();
      const quality = String(this.iosRouteResult?.routeQuality || '').toUpperCase();
      const detour = Number(this.iosRouteResult?.detourRatio);
      if (status.includes('FAILED') || status.includes('ERROR') || status.includes('INSUFFICIENT') || status.includes('REJECTED')) return false;
      if (quality.includes('FAILED') || quality.includes('HIGH') || quality.includes('FAR') || quality.includes('REPEATS') || quality.includes('BACKTRACKS')) return false;
      if (Number.isFinite(detour) && detour > 3) return false;
      return true;
    },

    iosRouteLoopMetricsText() {
      const metrics = this.iosRouteResult?.routeLoopMetrics || {};
      const backtracks = Number(metrics.backtrackCount || 0);
      const repeats = Number(metrics.repeatedSegmentCount || 0);
      const overlap = Number(metrics.overlapRatio || 0);
      return `${backtracks} backtrack · ${repeats} repeat · ${(overlap * 100).toFixed(1)}% overlap`;
    },

    iosPlaybackCoordinateCount() {
      return Number(this.iosRouteResult?.scheduledPointCount || 0);
    },

    iosWorkflowState() {
      if (this.iosRouteRunResult?.status) return this.iosRouteRunResult.status;
      if (this.iosRouteReadyForPlayback()) return 'READY_TO_PLAY';
      if (this.iosRouteResult?.routeId && this.iosRouteResult?.routePlayable === false) return 'ROUTE_REGENERATE_REQUIRED';
      if (this.iosRouteResult?.routeId) return 'COORDINATES_GENERATED';
      if (this.iosLocationConfirmed) return 'LOCATION_CONFIRMED';
      return 'LOCATION_REQUIRED';
    },

    iosPlaybackMonitorMode() {
      const run = this.iosRouteRunResult || {};
      return Boolean(run.routeRunId || run.status || run.stillRunning === true || run.completed === true || run.stalled === true);
    },

    iosPlaybackMonitorTitle() {
      const run = this.iosRouteRunResult || {};
      if (run.stalled) return '播放卡住';
      if (run.stillRunning === true) return '正在播放';
      if (run.completed === true) return '播放完成';
      if (run.status) return run.status;
      return '等待播放狀態';
    },

    iosMapModeLabel() {
      if (this.iosMapPickMode === 'origin') return 'PICK ORIGIN';
      const run = this.iosRouteRunResult || {};
      if (run.stalled) return 'STALLED';
      if (run.stillRunning === true) return 'PLAYING';
      if (run.completed === true) return 'COMPLETED';
      if (this.iosRouteReadyForPlayback()) return 'READY';
      if (this.iosRouteResult?.routeId) return 'ROUTE';
      return 'PLAN';
    },

    iosMapModeClass() {
      const run = this.iosRouteRunResult || {};
      if (this.iosMapPickMode === 'origin') return 'ios-map-mode-pick';
      if (run.stalled) return 'ios-map-mode-warn';
      if (run.stillRunning === true) return 'ios-map-mode-live';
      if (run.completed === true) return 'ios-map-mode-done';
      if (this.iosRouteReadyForPlayback()) return 'ios-map-mode-ready';
      return 'ios-map-mode-plan';
    },

    iosRouteGuidanceClass() {
      if (this.iosRouteReadyForPlayback()) return 'ios-guidance-ok';
      if (this.iosRouteResult?.routeId && !this.iosRouteQualityAcceptable()) return 'ios-guidance-warn';
      if (this.iosRouteRunResult?.stalled) return 'ios-guidance-warn';
      return 'ios-guidance-neutral';
    },

    iosRouteGuidanceTitle() {
      if (!this.iosLocationConfirmed) return '先確認起點';
      if (!this.iosRouteResult?.routeId) return '生成道路路線';
      if (!this.iosRouteQualityAcceptable()) return '需要重生路線';
      if (this.iosPlaybackCoordinateCount() <= 1) return '缺少播放座標';
      if (!this.iosDeviceConnected()) return '等待 iPhone 連線';
      if (this.iosRouteRunResult?.stalled) return '播放可能卡住';
      if (this.iosRouteRunResult?.stillRunning === true) return '正在漫遊';
      if (this.iosRouteRunResult?.completed === true) return '播放完成';
      return '可以開始播放';
    },

    iosRouteGuidanceText() {
      const status = String(this.iosRouteResult?.status || '').toUpperCase();
      const quality = String(this.iosRouteResult?.routeQuality || '').toUpperCase();
      if (!this.iosLocationConfirmed) return '讀取手機位置或直接在地圖上選起點。';
      if (!this.iosRouteResult?.routeId) return '使用目前起點與道路資料產生可播放座標。';
      if (status.includes('INSUFFICIENT')) return '路線太短，增加半徑或時長後重新生成。';
      if (quality.includes('REPEATS') || quality.includes('BACKTRACKS')) return '路線有重複或折返，請重新生成。';
      if (!this.iosRouteQualityAcceptable()) return '目前路線品質不適合播放，換起點或縮小半徑。';
      if (this.iosPlaybackCoordinateCount() <= 1) return '路線已存在，但尚未產生足夠播放點。';
      if (!this.iosDeviceConnected()) return '接上測試 iPhone 後即可播放。';
      if (this.iosRouteRunResult?.stalled) return '刷新進度；若仍無更新，停止後重播或 Reset。';
      if (this.iosRouteRunResult?.stillRunning === true) return '保持連線，觀察速度、距離與剩餘時間。';
      if (this.iosRouteRunResult?.completed === true) return '確認 App 行為後 Reset 回真實定位。';
      return '路線品質通過，可以開始漫遊。';
    },

    iosFlowSteps() {
      const run = this.iosRouteRunResult || {};
      const hasReport = Boolean(this.iosLatestReportResult?.reportAvailable || this.iosLatestReportResult?.report);
      const routeReady = Boolean(this.iosRouteResult?.routeId);
      const playing = run.stillRunning === true;
      const completed = run.completed === true;
      const stalled = run.stalled === true;
      const resetDone = this.iosAutoResetDone || String(this.iosResult?.status || '').toUpperCase().includes('RESET');
      return [
        {
          n: 1,
          title: '取得起點',
          desc: this.iosLocationConfirmed
            ? `${this.iosLocationSourceLabel()} · ${this.iosPointText(this.iosCurrentCenterSafe())}`
            : hasReport
              ? '已收到手機回報，待套用'
              : '等待手機回報或手動確認',
          state: this.iosLocationConfirmed ? 'done' : (hasReport ? 'current' : 'wait'),
        },
        {
          n: 2,
          title: '分析街道',
          desc: routeReady ? `${this.iosRouteResult?.routeQuality || this.iosRouteResult?.roadProvider || 'road'} · ${this.iosRouteResult?.routeDistanceM || 0}m` : 'OSRM 道路路由待執行',
          state: routeReady ? (this.iosRouteQualityAcceptable() ? 'done' : 'warn') : (this.iosLocationConfirmed ? 'current' : 'wait'),
        },
        {
          n: 3,
          title: '產生播放座標',
          desc: this.iosPlaybackCoordinateCount() > 1 ? `${this.iosPlaybackCoordinateCount()} ticks · ${this.iosSpeedKmh}km/h` : '等待 route plan',
          state: this.iosPlaybackCoordinateCount() > 1 ? 'done' : (routeReady ? 'current' : 'wait'),
        },
        {
          n: 4,
          title: '播放路線',
          desc: stalled ? `卡住 · log ${run.stderrStaleSeconds || '-'}s 未更新` : playing ? `${this.iosRouteProgressPercent()}% · 剩 ${this.iosRouteRunRemainingText()}` : completed ? '播放完成' : '等待開始播放',
          state: stalled ? 'warn' : playing ? 'current' : completed ? 'done' : (this.iosRouteReadyForPlayback() ? 'current' : 'wait'),
        },
        {
          n: 5,
          title: 'Reset 收尾',
          desc: resetDone ? '已清除模擬定位' : completed ? '建議 reset 回真實定位' : '播放後處理',
          state: resetDone ? 'done' : completed ? 'warn' : 'wait',
        },
      ];
    },

    iosFlowStepClass(step) {
      return `ios-flow-step ios-flow-${step.state || 'wait'}`;
    },

    iosChecklistClass(item) {
      return item.ok ? 'ios-check-row ios-check-ok' : 'ios-check-row ios-check-missing';
    },

    iosLocationSourceLabel() {
      if (this.iosLocationSource === 'phone_report') return '手機回報';
      if (this.iosLocationSource === 'device_read') return '裝置讀取';
      if (this.iosLocationSource === 'city_center') return '城市中心';
      if (this.iosLocationSource === 'preset') return '常用起點';
      if (this.iosLocationSource === 'map') return '地圖選點';
      return '手動確認';
    },

    iosCurrentCenterSafe() {
      const lat = Number(this.iosLat);
      const lng = Number(this.iosLng);
      if (!Number.isFinite(lat) || !Number.isFinite(lng)) return null;
      return { lat, lng };
    },

    iosRouteRunElapsedText() {
      const value = this.iosRouteRunResult?.elapsedSeconds;
      return Number.isFinite(Number(value)) ? this.iosFormatDuration(value) : '-';
    },

    iosRouteRunRemainingText() {
      const value = this.iosRouteRunResult?.remainingSeconds;
      return Number.isFinite(Number(value)) ? this.iosFormatDuration(value) : '-';
    },

    iosFormatDuration(value) {
      const seconds = Math.max(0, Math.round(Number(value) || 0));
      const mm = String(Math.floor(seconds / 60)).padStart(2, '0');
      const ss = String(seconds % 60).padStart(2, '0');
      return `${mm}:${ss}`;
    },

    iosRouteProgressPercent() {
      const run = this.iosRouteRunResult || {};
      const duration = Number(run.durationSeconds);
      const elapsed = Number(run.elapsedSeconds);
      if (Number.isFinite(duration) && duration > 0 && Number.isFinite(elapsed)) {
        return Math.max(0, Math.min(100, Math.round((elapsed / duration) * 100)));
      }
      const total = Number(run.scheduledPointCount);
      const count = Number(run.setLocationCount);
      if (Number.isFinite(total) && total > 0 && Number.isFinite(count)) {
        return Math.max(0, Math.min(100, Math.round((count / total) * 100)));
      }
      return 0;
    },

    iosRouteDistanceProgressText() {
      const moved = Number(this.iosRouteRunResult?.movedDistanceM);
      const expected = Number(this.iosRouteResult?.requestedDistanceM || this.iosRouteRunResult?.requestedDistanceM);
      const movedText = Number.isFinite(moved) ? `${(moved / 1000).toFixed(2)}km` : '-';
      const expectedText = Number.isFinite(expected) && expected > 0 ? `${(expected / 1000).toFixed(2)}km` : '-';
      return `${movedText} / ${expectedText}`;
    },

    iosPointText(point) {
      if (!point) return '-';
      const lat = Number(point.lat);
      const lng = Number(point.lng);
      if (!Number.isFinite(lat) || !Number.isFinite(lng)) return '-';
      return `${lat.toFixed(6)}, ${lng.toFixed(6)}`;
    },

    async loadScoutCandidates() {
      this.scoutLoading = true;
      this.scoutError = null;
      try {
        const args = { limit: 100 };
        if (this.scoutStatus !== 'all') args.status = this.scoutStatus;
        if (this.scoutSeverity !== 'all') args.severity = this.scoutSeverity;
        if (this.scoutEventType !== 'all') args.eventType = this.scoutEventType;
        if (this.scoutAllowedAction !== 'all') args.allowedAction = this.scoutAllowedAction;
        if (this.scoutEvidenceLevel !== 'all') args.evidenceLevel = this.scoutEvidenceLevel;
        const data = await this.callMcpJson('ops_event_inbox', args);
        this.scoutCandidates = data.events || data.candidates || [];
        if (!this.scoutSelectedKey && this.scoutCandidates.length > 0) {
          this.scoutSelectedKey = this.scoutCandidateKey(this.scoutCandidates[0]);
          this.scoutReviewStatus = this.scoutSuggestedReviewStatus(this.scoutCandidates[0]);
        }
      } catch (e) {
        this.scoutError = e.message || String(e);
      } finally {
        this.scoutLoading = false;
      }
    },

    async loadOpsSchedules() {
      this.opsScheduleLoading = true;
      this.opsScheduleError = null;
      try {
        const args = { limit: 100 };
        if (this.opsScheduleStatus !== 'all') args.status = this.opsScheduleStatus;
        const data = await this.callMcpJson('ops_schedule_list', args);
        this.opsSchedules = data.schedules || [];
      } catch (e) {
        this.opsScheduleError = e.message || String(e);
      } finally {
        this.opsScheduleLoading = false;
      }
    },

    async createOpsSchedule() {
      this.opsScheduleToast = null;
      this.opsScheduleError = null;
      try {
        let params = {};
        const raw = (this.newOpsParams || '').trim();
        if (raw) params = JSON.parse(raw);
        await this.callMcpJson('ops_schedule_create', {
          title: this.newOpsTitle,
          taskType: this.newOpsTaskType,
          cadence: this.newOpsCadence,
          priority: this.newOpsPriority,
          params,
        });
        this.opsScheduleToast = '已建立本地排期';
        await this.loadOpsSchedules();
      } catch (e) {
        this.opsScheduleError = e.message || String(e);
      }
    },

    async updateOpsSchedule(row, status) {
      if (!row?.id) return;
      this.opsScheduleToast = null;
      this.opsScheduleError = null;
      try {
        await this.callMcpJson('ops_schedule_update', {
          scheduleId: row.id,
          status,
          note: 'operator-ui',
        });
        this.opsScheduleToast = '已更新排期狀態';
        await this.loadOpsSchedules();
      } catch (e) {
        this.opsScheduleError = e.message || String(e);
      }
    },

    async runOpsSchedule(row = null) {
      this.opsScheduleRunBusy = true;
      this.opsScheduleToast = null;
      this.opsScheduleError = null;
      try {
        const args = { limit: 1 };
        if (row?.id) args.scheduleId = row.id;
        const data = await this.callMcpJson('ops_schedule_run_due', args);
        if (data.ran > 0) {
          const statuses = (data.results || [])
            .map(r => r.result_status || r.last_result_status)
            .filter(Boolean);
          this.opsScheduleToast = statuses.length
            ? `已執行本地排期 · ${statuses.join(', ')}`
            : '已執行本地排期';
        } else if (data.schedule?.status === 'DONE') {
          this.opsScheduleToast = '排期已完成';
        } else if (data.schedule?.status === 'RUNNING') {
          this.opsScheduleToast = '排期執行中';
        } else {
          this.opsScheduleToast = '沒有到期排期';
        }
        await this.loadOpsSchedules();
        await this.loadScoutCandidates();
      } catch (e) {
        this.opsScheduleError = e.message || String(e);
      } finally {
        this.opsScheduleRunBusy = false;
      }
    },

    opsScheduleStatusClass(status) {
      const s = String(status || '').toUpperCase();
      if (s === 'SCHEDULED') return 'badge-soft-info';
      if (s === 'RUNNING') return 'badge-soft-warn';
      if (s === 'DONE') return 'badge-soft';
      if (s === 'FAILED' || s === 'CANCELLED') return 'badge-soft-danger';
      return 'badge-soft-info';
    },

    opsScheduleResultStatus(row) {
      return row?.last_result_status
        || row?.last_result?.ops_report?.reportStatus
        || row?.last_result?.reportStatus
        || '';
    },

    opsScheduleResultStatusClass(status) {
      const s = String(status || '').toUpperCase();
      if (s === 'READY_FOR_REVIEW') return '';
      if (s === 'WAITING_OPERATOR_APPROVAL' || s === 'BLOCKED_INTEGRATION') return 'badge-soft-warn';
      return 'badge-soft-info';
    },

    scoutCandidateKey(row) {
      return row?.record_id || row?.dedupe_key || row?.candidate?.dedupeKey || '';
    },

    scoutCandidateStatus(row) {
      return row?.review_status || row?.candidate?.status || 'CANDIDATE';
    },

    scoutCandidateSeverity(row) {
      return row?.candidate?.severity || 'LOW';
    },

    scoutOpsEvent(row) {
      return row?.candidate?.opsEvent || {};
    },

    scoutEventTypeValue(row) {
      return this.scoutOpsEvent(row)?.eventType || row?.candidate?.candidateType || 'ops_signal';
    },

    scoutAllowedActionValue(row) {
      return row?.candidate?.allowedAction || this.scoutOpsEvent(row)?.allowedAction || 'HANDOFF_REVIEW_ONLY';
    },

    scoutEvidenceLevelValue(row) {
      return row?.candidate?.evidenceLevel || this.scoutOpsEvent(row)?.evidenceLevel || 'suspected';
    },

    scoutTargetProjectValue(row) {
      return this.scoutOpsEvent(row)?.targetProject || 'Sirin';
    },

    scoutCandidateTitle(row) {
      return row?.candidate?.title || row?.dedupe_key || '未命名事件';
    },

    scoutCandidateDedupe(row) {
      return row?.dedupe_key || row?.candidate?.dedupeKey || '';
    },

    scoutStatusClass(status) {
      const s = String(status || '').toUpperCase();
      if (s === 'CONVERT_TO_ISSUE' || s === 'ACCEPTED') return 'badge-soft';
      if (s === 'REJECTED') return 'badge-soft-danger';
      if (s.includes('MORE')) return 'badge-soft-warn';
      return 'badge-soft-info';
    },

    scoutSeverityClass(severity) {
      const s = String(severity || '').toUpperCase();
      if (s === 'HIGH') return 'badge-soft-danger';
      if (s === 'MEDIUM') return 'badge-soft-warn';
      return 'badge-soft-info';
    },

    scoutAllowedActionClass(action) {
      const s = String(action || '').toUpperCase();
      if (s === 'OPEN_PRODUCT_ISSUE_AFTER_REVIEW') return 'badge-soft-danger';
      if (s === 'OPERATOR_REVIEW') return 'badge-soft-warn';
      if (s === 'SIRIN_BACKLOG_ONLY') return 'badge-soft-info';
      return 'badge-soft';
    },

    scoutSuggestedReviewStatus(row) {
      const action = String(this.scoutAllowedActionValue(row) || '').toUpperCase();
      if (action === 'OPEN_PRODUCT_ISSUE_AFTER_REVIEW') return 'CONVERT_TO_ISSUE';
      if (action === 'OPERATOR_REVIEW' || action === 'SIRIN_BACKLOG_ONLY') return 'ACCEPTED';
      return 'NEEDS_MORE_EVIDENCE';
    },

    scoutReviewStatusAllowed(row, status) {
      const s = String(status || '').toUpperCase();
      const action = String(this.scoutAllowedActionValue(row) || '').toUpperCase();
      if (s !== 'CONVERT_TO_ISSUE') return true;
      return action === 'OPEN_PRODUCT_ISSUE_AFTER_REVIEW';
    },

    selectScoutCandidate(row) {
      this.scoutSelectedKey = this.scoutCandidateKey(row);
      this.scoutReviewStatus = this.scoutSuggestedReviewStatus(row);
    },

    get selectedScoutCandidate() {
      return this.scoutCandidates.find(c => this.scoutCandidateKey(c) === this.scoutSelectedKey)
        || this.scoutCandidates[0]
        || null;
    },

    scoutEvidence(row) {
      const evidence = row?.candidate?.evidence;
      return Array.isArray(evidence) ? evidence : [];
    },

    scoutHandoffText(row) {
      const handoff = row?.candidate?.handoff || {};
      return JSON.stringify(handoff, null, 2);
    },

    async reviewSelectedScoutCandidate() {
      const row = this.selectedScoutCandidate;
      if (!row || this.scoutReviewBusy) return;
      if (!this.scoutReviewStatusAllowed(row, this.scoutReviewStatus)) {
        this.scoutReviewStatus = this.scoutSuggestedReviewStatus(row);
        this.scoutToast = '此事件不允許直接轉產品 issue，已切換為建議狀態';
        return;
      }
      this.scoutReviewBusy = true;
      this.scoutToast = null;
      try {
        const args = {
          dedupeKey: this.scoutCandidateDedupe(row),
          reviewStatus: this.scoutReviewStatus,
          reviewNote: this.scoutReviewNote,
          reviewedBy: 'operator-ui',
        };
        if (row.task_id) args.task_id = row.task_id;
        await this.callMcpJson('a2a_review_issue_candidate', args);
        this.scoutToast = '已更新候選狀態';
        await this.loadScoutCandidates();
        await this.loadHandoffQueue();
      } catch (e) {
        this.scoutToast = '更新失敗: ' + (e.message || e);
      } finally {
        this.scoutReviewBusy = false;
      }
    },

    async copyScoutHandoff() {
      const row = this.selectedScoutCandidate;
      if (!row) return;
      try {
        await navigator.clipboard.writeText(this.scoutHandoffText(row));
        this.scoutToast = 'handoff 已複製';
      } catch (_) {
        this.scoutToast = '無法複製 handoff';
      }
    },

    async loadHandoffQueue() {
      this.handoffLoading = true;
      this.handoffError = null;
      try {
        const args = { limit: 100 };
        args.status = this.handoffStatus === 'all' ? 'ALL' : this.handoffStatus;
        const data = await this.callMcpJson('a2a_handoff_queue', args);
        this.handoffQueue = data.handoffs || [];
      } catch (e) {
        this.handoffError = e.message || String(e);
      } finally {
        this.handoffLoading = false;
      }
    },

    async updateHandoff(row, status) {
      if (!row?.handoff_id || this.handoffBusy) return;
      this.handoffBusy = true;
      this.handoffError = null;
      try {
        await this.callMcpJson('a2a_update_handoff', {
          handoffId: row.handoff_id,
          status,
          note: 'operator-ui',
        });
        this.scoutToast = '已更新 handoff 狀態';
        await this.loadHandoffQueue();
      } catch (e) {
        this.handoffError = e.message || String(e);
      } finally {
        this.handoffBusy = false;
      }
    },

    handoffText(row) {
      return JSON.stringify(row?.handoff || {}, null, 2);
    },

    async copyQueuedHandoff(row) {
      try {
        await navigator.clipboard.writeText(this.handoffText(row));
        this.scoutToast = 'queued handoff 已複製';
      } catch (_) {
        this.scoutToast = '無法複製 queued handoff';
      }
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
      if (this.coreTelemetryPaused) return;
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
          if (this.coreTelemetryPaused) return;
          // Fallback: poll while disconnected, retry WS in 5 s.
          if (!this._poll_timer) {
            this._poll_timer = setInterval(() => this.fetchSnapshot(), 5000);
          }
          setTimeout(() => this.connectWs(), 5000);
        });
        ws.addEventListener('error', () => { /* close fires next */ });
      } catch (_) {
        if (this.coreTelemetryPaused) return;
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
      { group: 'SYSTEM',    label: 'AI Work Monitor',   action: 'go-ai-monitor' },
      { group: 'TESTING',   label: 'Coverage Map',     action: 'go-coverage' },
      { group: 'TESTING',   label: 'Browser Monitor',  action: 'go-browser' },
      { group: 'AUTOMATION',label: 'Dev Squad',        action: 'open-devsquad' },
      { group: 'AUTOMATION',label: 'MCP Playground',   action: 'open-mcp' },
      { group: 'OPS',       label: 'Ops Scout',        action: 'go-ops-scout' },
      { group: 'DEVICE',    label: 'iOS Location',     action: 'go-ios-location' },
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
        case 'go-ai-monitor': this.view = 'ai-monitor'; break;
        case 'go-ops-scout': this.view = 'ops-scout'; break;
        case 'go-ios-location': this.view = 'ios-location'; break;
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
