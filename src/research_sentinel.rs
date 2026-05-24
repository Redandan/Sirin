//! News/event research sentinel for Codex-defined review goals.
//!
//! This module is intentionally read-only.  It searches external public
//! sources, classifies news relevance for an AgoraMarketAPI automation goal,
//! and stores a Codex-readable inbox item.  It does not read or mutate
//! AgoraMarketAPI trading state.

use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::jsonl_log::JsonlLog;
use crate::skills::SearchResult;

const DEFAULT_TOPIC: &str = "AgoraMarketAPI automated trading crypto news";
const DEFAULT_FOCUS: &str = "events that may affect automated trading volatility, liquidity, exchange risk, stablecoin risk, regulation, or macro risk";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const MONITOR_CONFIG_FILE: &str = "research_sentinel_monitors.yaml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchGoal {
    pub id: String,
    pub created_at: String,
    pub status: String,
    pub topic: String,
    pub focus: String,
    pub keywords: Vec<String>,
    pub lookback_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchSource {
    pub name: String,
    pub url: String,
    #[serde(default = "default_source_kind")]
    pub kind: String,
    #[serde(default = "default_source_group")]
    pub group: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default = "default_source_trust")]
    pub trust: u8,
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchSentinelConfig {
    #[serde(default = "default_official_only")]
    pub official_only: bool,
    #[serde(default = "default_source_groups")]
    pub enabled_groups: Vec<String>,
    #[serde(default = "default_keywords")]
    pub default_keywords: Vec<String>,
    #[serde(default = "default_config_sources")]
    pub sources: Vec<ResearchSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchMonitorConfig {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    pub topic: String,
    pub focus: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_monitor_interval_minutes")]
    pub interval_minutes: u64,
    #[serde(default = "default_monitor_lookback_hours")]
    pub lookback_hours: u64,
    #[serde(default = "default_monitor_max_results")]
    pub max_results: usize,
    #[serde(default)]
    pub publish_to_kb: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchMonitorFile {
    #[serde(default)]
    pub monitors: Vec<ResearchMonitorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchMonitorStatus {
    pub id: String,
    pub enabled: bool,
    pub interval_minutes: u64,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub last_inbox_id: Option<String>,
    pub last_report_path: Option<String>,
    pub last_summary: Option<String>,
    pub last_error: Option<String>,
    pub event_count: usize,
    pub review_count: usize,
    pub new_event_count: usize,
    pub escalated_event_count: usize,
    pub kb_publish_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchItem {
    pub id: String,
    pub title: String,
    pub url: String,
    pub source: String,
    pub snippet: String,
    pub discovered_at: String,
    pub published_at: Option<String>,
    #[serde(default = "default_source_group")]
    pub source_group: String,
    #[serde(default)]
    pub source_trust: u8,
    #[serde(default)]
    pub source_categories: Vec<String>,
    #[serde(default)]
    pub official_source: bool,
    pub assets: Vec<String>,
    pub event_type: String,
    pub relevance: String,
    pub impact_window: String,
    pub confidence: String,
    pub needs_codex_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchEvent {
    pub id: String,
    pub event_key: String,
    pub event_type: String,
    pub title: String,
    pub summary: String,
    pub assets: Vec<String>,
    pub impact_window: String,
    pub confidence: String,
    pub needs_codex_review: bool,
    #[serde(default)]
    pub review_score: i32,
    #[serde(default)]
    pub codex_recommendation: String,
    pub relevance: String,
    #[serde(default)]
    pub source_count: usize,
    #[serde(default)]
    pub source_groups: Vec<String>,
    #[serde(default)]
    pub occurrence_status: String,
    #[serde(default)]
    pub kb_topic_key: Option<String>,
    #[serde(default)]
    pub kb_publish_status: String,
    pub sources: Vec<ResearchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchInboxEntry {
    pub id: String,
    pub run_id: String,
    pub goal_id: Option<String>,
    pub created_at: String,
    pub status: String,
    pub review_status: String,
    pub review_note: Option<String>,
    pub reviewed_at: Option<String>,
    pub topic: String,
    pub focus: String,
    pub summary: String,
    pub events: Vec<ResearchEvent>,
    pub items: Vec<ResearchItem>,
    pub source_errors: Vec<String>,
    pub guardrails: Vec<String>,
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default)]
    pub kb_publish_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResearchEventState {
    event_key: String,
    first_seen_at: String,
    last_seen_at: String,
    seen_count: u64,
    last_review_score: i32,
    last_recommendation: String,
    last_title: String,
    last_kb_topic_key: String,
}

#[derive(Debug, Clone)]
struct NewsCandidate {
    title: String,
    url: String,
    snippet: String,
    published_at: Option<String>,
    source_name: String,
    source_group: String,
    source_trust: u8,
    source_categories: Vec<String>,
    official_source: bool,
}

fn goal_log() -> &'static JsonlLog<ResearchGoal> {
    static LOG: OnceLock<JsonlLog<ResearchGoal>> = OnceLock::new();
    LOG.get_or_init(|| JsonlLog::new(tracking_path("research_sentinel_goals.jsonl")))
}

fn inbox_log() -> &'static JsonlLog<ResearchInboxEntry> {
    static LOG: OnceLock<JsonlLog<ResearchInboxEntry>> = OnceLock::new();
    LOG.get_or_init(|| JsonlLog::new(tracking_path("research_sentinel_inbox.jsonl")))
}

fn tracking_path(file: &str) -> PathBuf {
    crate::platform::app_data_dir().join("tracking").join(file)
}

fn state_log() -> &'static JsonlLog<ResearchEventState> {
    static LOG: OnceLock<JsonlLog<ResearchEventState>> = OnceLock::new();
    LOG.get_or_init(|| JsonlLog::new(tracking_path("research_sentinel_state.jsonl")))
}

fn monitor_statuses() -> &'static Mutex<Vec<ResearchMonitorStatus>> {
    static STATUSES: OnceLock<Mutex<Vec<ResearchMonitorStatus>>> = OnceLock::new();
    STATUSES.get_or_init(|| Mutex::new(Vec::new()))
}

fn report_dir() -> PathBuf {
    crate::platform::app_data_dir().join("tracking").join("reports").join("research_sentinel")
}

fn default_source_kind() -> String {
    "rss".into()
}

fn default_source_group() -> String {
    "official".into()
}

fn default_source_trust() -> u8 {
    100
}

fn default_official_only() -> bool {
    true
}

fn default_source_groups() -> Vec<String> {
    vec!["official".into(), "investment_data".into()]
}

fn default_monitor_interval_minutes() -> u64 {
    60
}

fn default_monitor_lookback_hours() -> u64 {
    72
}

fn default_monitor_max_results() -> usize {
    12
}

fn default_config_sources() -> Vec<ResearchSource> {
    vec![
        ResearchSource {
            name: "SEC Press Releases".into(),
            url: "https://www.sec.gov/news/pressreleases.rss".into(),
            kind: "rss".into(),
            group: "official".into(),
            categories: vec!["regulation".into(), "policy".into()],
            trust: 100,
            official: true,
            priority: 10,
            enabled: true,
        },
        ResearchSource {
            name: "Federal Reserve Press Releases".into(),
            url: "https://www.federalreserve.gov/feeds/press_all.xml".into(),
            kind: "rss".into(),
            group: "official".into(),
            categories: vec!["macro_calendar".into(), "macro_data".into(), "policy".into()],
            trust: 100,
            official: true,
            priority: 20,
            enabled: true,
        },
        ResearchSource {
            name: "Investing.com Cryptocurrency News".into(),
            url: "https://www.investing.com/rss/news_301.rss".into(),
            kind: "rss".into(),
            group: "investment_data".into(),
            categories: vec!["market_flow".into(), "volatility_regime".into(), "background".into()],
            trust: 70,
            official: false,
            priority: 100,
            enabled: true,
        },
        ResearchSource {
            name: "Investing.com Economic Indicators".into(),
            url: "https://www.investing.com/rss/news_95.rss".into(),
            kind: "rss".into(),
            group: "investment_data".into(),
            categories: vec!["macro_data".into(), "macro_calendar".into()],
            trust: 75,
            official: false,
            priority: 110,
            enabled: true,
        },
    ]
}

fn default_keywords() -> Vec<String> {
    [
        "BTC", "Bitcoin", "ETH", "USDT", "stablecoin", "Binance", "Coinbase",
        "SEC", "CFTC", "Federal Reserve", "CPI", "FOMC",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_rss_feeds() -> Vec<String> {
    default_config_sources().into_iter().map(|source| source.url).collect()
}

fn default_config() -> ResearchSentinelConfig {
    ResearchSentinelConfig {
        official_only: true,
        enabled_groups: default_source_groups(),
        default_keywords: default_keywords(),
        sources: default_config_sources(),
    }
}

fn default_monitor_file() -> ResearchMonitorFile {
    ResearchMonitorFile {
        monitors: vec![ResearchMonitorConfig {
            id: "agoramarketapi-official-investment".into(),
            enabled: false,
            topic: "AgoraMarketAPI auto trading official investment monitor".into(),
            focus: "official announcements and investment market data that may affect automated trading guardrails: volatility, liquidity, exchange operations, stablecoin risk, ETF flow, regulation, SEC CFTC Federal Reserve macro calendar".into(),
            keywords: default_keywords(),
            interval_minutes: default_monitor_interval_minutes(),
            lookback_hours: default_monitor_lookback_hours(),
            max_results: default_monitor_max_results(),
            publish_to_kb: false,
        }],
    }
}

fn config_candidates() -> Vec<PathBuf> {
    vec![
        crate::platform::config_path("research_sentinel.yaml"),
        PathBuf::from("config").join("research_sentinel.yaml"),
    ]
}

fn load_config() -> ResearchSentinelConfig {
    for path in config_candidates() {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_yaml::from_str::<ResearchSentinelConfig>(&text) {
                return cfg;
            }
        }
    }
    default_config()
}

fn monitor_config_candidates() -> Vec<PathBuf> {
    vec![
        crate::platform::config_path(MONITOR_CONFIG_FILE),
        PathBuf::from("config").join(MONITOR_CONFIG_FILE),
    ]
}

fn load_monitor_file() -> ResearchMonitorFile {
    for path in monitor_config_candidates() {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_yaml::from_str::<ResearchMonitorFile>(&text) {
                return cfg;
            }
        }
    }
    default_monitor_file()
}

fn hash_id(prefix: &str, value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{prefix}-{:016x}", hasher.finish())
}

fn str_arg(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

fn keywords_arg(args: &Value) -> Vec<String> {
    let from_args = args
        .get("keywords")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if from_args.is_empty() {
        let cfg = load_config();
        if cfg.default_keywords.is_empty() {
            default_keywords()
        } else {
            cfg.default_keywords
        }
    } else {
        from_args
    }
}

fn source_enabled(source: &ResearchSource, cfg: &ResearchSentinelConfig) -> bool {
    source.enabled
        && source.kind == "rss"
        && cfg.enabled_groups.iter().any(|group| group == &source.group)
        && (!cfg.official_only || source.group == "official" || source.group == "investment_data")
}

fn sources_arg(args: &Value) -> Vec<ResearchSource> {
    let feeds = args
        .get("rss_feeds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !feeds.is_empty() {
        return feeds
            .into_iter()
            .enumerate()
            .map(|(idx, url)| ResearchSource {
                name: source_domain(&url),
                url,
                kind: "rss".into(),
                group: "investment_data".into(),
                categories: vec!["background".into()],
                trust: 60,
                official: false,
                priority: idx as u8,
                enabled: true,
            })
            .collect();
    }

    let cfg = load_config();
    let mut sources = cfg.sources.clone();
    sources.sort_by_key(|source| source.priority);
    let configured = sources
        .into_iter()
        .filter(|source| source_enabled(source, &cfg))
        .collect::<Vec<_>>();
    if configured.is_empty() {
        let mut sources = default_config_sources();
        sources.sort_by_key(|source| source.priority);
        sources
    } else {
        configured
    }
}

fn build_goal_from_args(args: &Value) -> ResearchGoal {
    ResearchGoal {
        id: format!("rsg-{}", Utc::now().timestamp_millis()),
        created_at: Utc::now().to_rfc3339(),
        status: "active".into(),
        topic: str_arg(args, "topic", DEFAULT_TOPIC),
        focus: str_arg(args, "focus", DEFAULT_FOCUS),
        keywords: keywords_arg(args),
        lookback_hours: args.get("lookback_hours").and_then(Value::as_u64).unwrap_or(24).clamp(1, 168),
    }
}

fn find_goal(id: &str) -> Result<Option<ResearchGoal>, String> {
    Ok(goal_log()
        .read_all()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|goal| goal.id == id))
}

fn search_queries(goal: &ResearchGoal, max_queries: usize) -> Vec<String> {
    let mut queries = Vec::new();
    for keyword in goal.keywords.iter().take(max_queries.max(1)) {
        queries.push(format!(
            "{} {} {} latest news past {} hours",
            goal.topic, goal.focus, keyword, goal.lookback_hours
        ));
    }
    if queries.is_empty() {
        queries.push(format!(
            "{} {} latest news past {} hours",
            goal.topic, goal.focus, goal.lookback_hours
        ));
    }
    queries
}

fn source_domain(url: &str) -> String {
    let without_scheme = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    without_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_string()
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn contains_token(text: &str, needle: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token.eq_ignore_ascii_case(needle))
}

fn contains_any_token(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| contains_token(text, needle))
}

fn has_strategy_relevance(text: &str) -> bool {
    let lower = text.to_lowercase();
    contains_any_token(&lower, &["btc", "eth", "usdt", "usdc", "fomc", "cpi", "pce", "ppi"])
        || contains_any(
            &lower,
            &[
                "bitcoin",
                "ether",
                "ethereum",
                "crypto",
                "digital asset",
                "digital assets",
                "stablecoin",
                "tether",
                "circle",
                "binance",
                "coinbase",
                "crypto exchange",
                "digital asset exchange",
                "spot etf",
                "bitcoin etf",
                "ether etf",
                "etf flow",
                "etf flows",
                "liquidity",
                "order book",
                "market depth",
                "market maker",
                "volatility",
                "funding rate",
                "open interest",
                "liquidation",
                "liquidations",
                "federal funds",
                "interest rate",
                "rate cut",
                "rate hike",
                "inflation",
                "payroll",
                "treasury yield",
                "dollar liquidity",
            ],
        )
}

fn classify_event(text: &str) -> String {
    let lower = text.to_lowercase();
    if contains_any(&lower, &["wallet maintenance", "maintenance", "suspend deposits", "suspend withdrawals", "network upgrade"]) {
        "exchange_ops".into()
    } else if contains_any(&lower, &["listing", "delisting", "will list", "adds trading", "removes trading"]) {
        "listing_delisting".into()
    } else if contains_any(&lower, &["hack", "exploit", "breach", "stolen", "security incident", "phishing"]) {
        "security_incident".into()
    } else if contains_any(&lower, &["stablecoin", "usdt", "usdc", "depeg", "stablecoin reserve", "usdt reserve", "usdc reserve", "redemption", "tether", "circle"]) {
        "stablecoin_risk".into()
    } else if contains_any(&lower, &["etf flow", "etf flows", "outflow", "outflows", "inflow", "inflows", "fund flow", "fund flows"]) {
        "etf_flow".into()
    } else if contains_any(&lower, &["liquidity", "order book", "depth", "market maker", "redemption"]) {
        "liquidity".into()
    } else if contains_any(&lower, &["volatility", "implied volatility", "realized volatility", "risk appetite"]) {
        "volatility_regime".into()
    } else if contains_any(&lower, &["funding", "open interest", "liquidation", "liquidations", "basis"]) {
        "market_structure".into()
    } else if contains_any(&lower, &["federal reserve", "fomc", "cpi", "inflation", "rate cut", "interest rate", "nfp", "payroll", "economic calendar"]) {
        "macro_calendar".into()
    } else if contains_any(&lower, &["jobs report", "gdp", "pce", "ppi", "retail sales", "selected interest rates", "treasury yield"]) {
        "macro_data".into()
    } else if contains_any(&lower, &["sec", "cftc", "lawsuit", "regulation", "regulatory", "treasury", "enforcement", "commissioner"])
        && has_strategy_relevance(&lower)
    {
        "regulation".into()
    } else if contains_any(&lower, &["policy", "rule", "proposal", "guidance"]) && has_strategy_relevance(&lower) {
        "policy".into()
    } else if contains_any(&lower, &["price", "rally", "tanks", "stocks", "bonds", "wall street"]) && has_strategy_relevance(&lower) {
        "market_flow".into()
    } else {
        "background".into()
    }
}

fn classify_event_with_source(text: &str, candidate: &NewsCandidate) -> String {
    let text_type = classify_event(text);
    if text_type != "background" {
        return text_type;
    }
    if !has_strategy_relevance(text) {
        return text_type;
    }
    for category in &candidate.source_categories {
        if category != "background" {
            return category.clone();
        }
    }
    text_type
}

fn detect_assets(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut assets = Vec::new();
    for (asset, token_needles, phrase_needles) in [
        ("BTC", vec!["btc"], vec!["bitcoin"]),
        ("ETH", vec!["eth"], vec!["ethereum", "ether"]),
        ("USDT", vec!["usdt"], vec!["tether"]),
        ("USDC", vec!["usdc"], vec![]),
        ("BNB", vec!["bnb"], vec!["binance"]),
        ("SOL", vec!["sol"], vec!["solana"]),
    ] {
        if token_needles.iter().any(|needle| contains_token(&lower, needle))
            || phrase_needles.iter().any(|needle| lower.contains(needle))
        {
            assets.push(asset.to_string());
        }
    }
    assets
}

fn relevance_for(event_type: &str, assets: &[String]) -> String {
    let asset_text = if assets.is_empty() {
        "crypto market".to_string()
    } else {
        assets.join("/")
    };
    match event_type {
        "exchange_ops" => format!("{asset_text}: may affect venue availability, deposits/withdrawals, liquidity routing, or exchange-status guardrails."),
        "listing_delisting" => format!("{asset_text}: may affect symbol availability, route eligibility, liquidity, or instrument risk controls."),
        "security_incident" => format!("{asset_text}: may trigger exchange risk, liquidity withdrawal, or emergency execution guardrails."),
        "stablecoin_risk" => format!("{asset_text}: may affect settlement asset assumptions, stablecoin liquidity, or risk-off controls."),
        "regulation" => format!("{asset_text}: may change compliance, venue, or volatility assumptions for automated trading risk filters."),
        "policy" => format!("{asset_text}: may affect medium-term compliance or market-structure assumptions."),
        "macro_calendar" => format!("{asset_text}: marks scheduled macro event risk that Codex may map to pre/post-event volatility guardrails."),
        "macro_data" => format!("{asset_text}: may affect dollar/rate/liquidity backdrop and volatility regime."),
        "market_flow" => format!("{asset_text}: may affect flow backdrop, positioning, and short-horizon risk review."),
        "etf_flow" => format!("{asset_text}: may affect spot demand/supply pressure and liquidity/risk-limit review."),
        "liquidity" => format!("{asset_text}: may affect stablecoin or order-book liquidity assumptions."),
        "volatility_regime" => format!("{asset_text}: may affect volatility-aware sizing or strategy pause/review thresholds."),
        "market_structure" => format!("{asset_text}: may affect liquidation/funding/open-interest context used by strategy review."),
        _ => "Background item. Codex should only promote it if the source or follow-up evidence shows direct strategy impact.".into(),
    }
}

fn confidence_for(source: &str, event_type: &str, text: &str, candidate: &NewsCandidate) -> String {
    let lower = text.to_lowercase();
    if candidate.official_source || candidate.source_trust >= 90 || contains_any(source, &["sec.gov", "cftc.gov", "federalreserve.gov", "binance.com", "coinbase.com"]) {
        "high".into()
    } else if event_type != "other" && contains_any(&lower, &["announces", "files", "approves", "halts", "delists", "exploit"]) {
        "medium".into()
    } else if event_type != "background" {
        "medium".into()
    } else {
        "low".into()
    }
}

fn impact_window_for(event_type: &str) -> String {
    match event_type {
        "exchange_ops" | "security_incident" => "scheduled_or_intraday".into(),
        "macro_calendar" => "scheduled".into(),
        "macro_data" | "market_flow" | "etf_flow" | "liquidity" | "volatility_regime" | "market_structure" => "intraday".into(),
        "regulation" | "policy" | "listing_delisting" | "stablecoin_risk" => "multi-day".into(),
        _ => "unknown".into(),
    }
}

fn normalize_event_word(word: &str) -> String {
    word.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn event_key_for(item: &ResearchItem) -> String {
    let mut parts = item.assets.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
    parts.push(item.event_type.clone());
    let mut title_words = item
        .title
        .split_whitespace()
        .map(normalize_event_word)
        .filter(|word| word.len() >= 4)
        .filter(|word| {
            !matches!(
                word.as_str(),
                "with" | "from" | "that" | "this" | "after" | "before" | "into" | "over" | "under"
            )
        })
        .take(6)
        .collect::<Vec<_>>();
    parts.append(&mut title_words);
    if parts.len() <= 1 {
        parts.push(hash_id("h", &item.url));
    }
    parts.join(":")
}

fn merge_assets(items: &[ResearchItem]) -> Vec<String> {
    let mut assets = Vec::new();
    for item in items {
        for asset in &item.assets {
            if !assets.contains(asset) {
                assets.push(asset.clone());
            }
        }
    }
    assets
}

fn best_confidence(items: &[ResearchItem]) -> String {
    if items.iter().any(|item| item.confidence == "high") {
        "high".into()
    } else if items.iter().any(|item| item.confidence == "medium") {
        "medium".into()
    } else {
        "low".into()
    }
}

fn source_groups(items: &[ResearchItem]) -> Vec<String> {
    let mut groups = Vec::new();
    for item in items {
        if !groups.contains(&item.source_group) {
            groups.push(item.source_group.clone());
        }
    }
    groups
}

fn kb_topic_key_for(event: &ResearchEvent) -> String {
    let mut slug = event
        .event_key
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '-' })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug = slug.trim_matches('-').to_string();
    if slug.len() > 72 {
        slug.truncate(72);
        slug = slug.trim_matches('-').to_string();
    }
    format!("research-sentinel-event-{slug}")
}

fn recommendation_rank(recommendation: &str) -> i32 {
    match recommendation {
        "convert_to_issue" => 4,
        "needs_more_sources" => 3,
        "watch" => 2,
        "ignore" => 1,
        _ => 0,
    }
}

fn classify_occurrence(event: &ResearchEvent, previous: Option<&ResearchEventState>) -> String {
    match previous {
        None => "new".into(),
        Some(prev)
            if recommendation_rank(&event.codex_recommendation) > recommendation_rank(&prev.last_recommendation)
                || event.review_score >= prev.last_review_score + 20 =>
        {
            "escalated".into()
        }
        Some(prev) if event.review_score != prev.last_review_score || event.codex_recommendation != prev.last_recommendation => {
            "updated".into()
        }
        Some(_) => "seen".into(),
    }
}

fn event_should_publish_to_kb(event: &ResearchEvent, review_status: &str) -> bool {
    matches!(review_status, "accepted" | "convert_to_issue")
        || event.occurrence_status == "escalated"
        || (event.occurrence_status == "new" && event.codex_recommendation == "convert_to_issue")
}

fn event_is_report_worthy(event: &ResearchEvent) -> bool {
    event.occurrence_status == "escalated"
        || (event.occurrence_status == "new" && event.codex_recommendation == "convert_to_issue")
}

fn entry_is_report_worthy(entry: &ResearchInboxEntry) -> bool {
    !entry.source_errors.is_empty() || entry.events.iter().any(event_is_report_worthy)
}

fn apply_event_state(events: &mut [ResearchEvent]) -> Result<(), String> {
    let now = Utc::now().to_rfc3339();
    let existing = state_log().read_all().map_err(|e| e.to_string())?;
    let mut updates = Vec::new();
    for event in events.iter_mut() {
        let previous = existing.iter().rev().find(|state| state.event_key == event.event_key);
        let topic_key = previous
            .map(|state| state.last_kb_topic_key.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| kb_topic_key_for(event));
        event.occurrence_status = classify_occurrence(event, previous);
        event.kb_topic_key = Some(topic_key.clone());
        event.kb_publish_status = if event_should_publish_to_kb(event, "pending") {
            "candidate".into()
        } else {
            "not_eligible".into()
        };
        updates.push(ResearchEventState {
            event_key: event.event_key.clone(),
            first_seen_at: previous.map(|state| state.first_seen_at.clone()).unwrap_or_else(|| now.clone()),
            last_seen_at: now.clone(),
            seen_count: previous.map(|state| state.seen_count + 1).unwrap_or(1),
            last_review_score: event.review_score,
            last_recommendation: event.codex_recommendation.clone(),
            last_title: event.title.clone(),
            last_kb_topic_key: topic_key,
        });
    }
    for update in updates {
        state_log()
            .upsert_by(update, |state| state.event_key.clone())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn category_weight(event_type: &str) -> i32 {
    match event_type {
        "exchange_ops" | "security_incident" | "stablecoin_risk" => 35,
        "listing_delisting" | "regulation" | "macro_calendar" | "macro_data" => 25,
        "etf_flow" | "liquidity" | "volatility_regime" | "market_structure" | "market_flow" => 18,
        "policy" => 12,
        _ => -25,
    }
}

fn source_group_weight(groups: &[String]) -> i32 {
    if groups.iter().any(|group| group == "official") {
        30
    } else if groups.iter().any(|group| group == "investment_data") {
        10
    } else {
        -20
    }
}

fn review_score_for(event_type: &str, assets: &[String], sources: &[ResearchItem], strategy_relevant: bool) -> i32 {
    let max_trust = sources.iter().map(|item| item.source_trust as i32).max().unwrap_or(0);
    let trust_score = max_trust / 2;
    let source_count_bonus = ((sources.len().saturating_sub(1)).min(3) as i32) * 10;
    let asset_bonus = if assets.iter().any(|asset| matches!(asset.as_str(), "BTC" | "ETH" | "USDT" | "USDC" | "BNB")) {
        15
    } else {
        0
    };
    let groups = source_groups(sources);
    let single_investment_penalty = if sources.len() == 1 && groups.iter().any(|group| group == "investment_data") {
        -15
    } else {
        0
    };
    let relevance_gate = if strategy_relevant { 0 } else { -80 };
    trust_score
        + source_group_weight(&groups)
        + source_count_bonus
        + asset_bonus
        + category_weight(event_type)
        + single_investment_penalty
        + relevance_gate
}

fn codex_recommendation_for(score: i32, event_type: &str, groups: &[String], source_count: usize, strategy_relevant: bool) -> String {
    if !strategy_relevant || event_type == "background" || score < 45 {
        "ignore".into()
    } else if groups.iter().any(|group| group == "official")
        && score >= 75
        && matches!(event_type, "exchange_ops" | "listing_delisting" | "security_incident" | "stablecoin_risk" | "regulation" | "macro_calendar")
    {
        "convert_to_issue".into()
    } else if groups.iter().any(|group| group == "investment_data") && source_count == 1 {
        "watch".into()
    } else if score >= 65 {
        "needs_more_sources".into()
    } else {
        "watch".into()
    }
}

fn aggregate_events(items: &[ResearchItem]) -> Vec<ResearchEvent> {
    let mut grouped: Vec<(String, Vec<ResearchItem>)> = Vec::new();
    for item in items {
        let key = event_key_for(item);
        if let Some((_, bucket)) = grouped.iter_mut().find(|(existing, _)| existing == &key) {
            bucket.push(item.clone());
        } else {
            grouped.push((key, vec![item.clone()]));
        }
    }

    let mut events = grouped
        .into_iter()
        .map(|(event_key, sources)| {
            let lead = sources
                .iter()
                .find(|item| item.needs_codex_review)
                .unwrap_or(&sources[0]);
            let assets = merge_assets(&sources);
            let confidence = best_confidence(&sources);
            let groups = source_groups(&sources);
            let strategy_relevant = sources
                .iter()
                .any(|item| has_strategy_relevance(&format!("{} {}", item.title, item.snippet)));
            let review_score = review_score_for(&lead.event_type, &assets, &sources, strategy_relevant);
            let codex_recommendation = codex_recommendation_for(
                review_score,
                &lead.event_type,
                &groups,
                sources.len(),
                strategy_relevant,
            );
            ResearchEvent {
                id: hash_id("rse", &event_key),
                event_key,
                event_type: lead.event_type.clone(),
                title: lead.title.clone(),
                summary: lead.snippet.clone(),
                assets: assets.clone(),
                impact_window: lead.impact_window.clone(),
                confidence,
                needs_codex_review: codex_recommendation != "ignore",
                review_score,
                codex_recommendation,
                relevance: relevance_for(&lead.event_type, &assets),
                source_count: sources.len(),
                source_groups: groups,
                occurrence_status: String::new(),
                kb_topic_key: None,
                kb_publish_status: "not_evaluated".into(),
                sources,
            }
        })
        .collect::<Vec<_>>();

    events.sort_by_key(|event| {
        let review_rank = if event.needs_codex_review { 0 } else { 1 };
        let confidence_rank = match event.confidence.as_str() {
            "high" => 0,
            "medium" => 1,
            _ => 2,
        };
        (review_rank, confidence_rank, event.event_type.clone())
    });
    events
}

fn item_from_search(result: SearchResult) -> ResearchItem {
    item_from_candidate(NewsCandidate {
        title: result.title,
        url: result.url,
        snippet: result.snippet,
        published_at: None,
        source_name: "Search Result".into(),
        source_group: "news".into(),
        source_trust: 40,
        source_categories: vec!["background".into()],
        official_source: false,
    })
}

fn item_from_candidate(candidate: NewsCandidate) -> ResearchItem {
    let text = format!("{} {}", candidate.title, candidate.snippet);
    let source = source_domain(&candidate.url);
    let event_type = classify_event_with_source(&text, &candidate);
    let assets = detect_assets(&text);
    let confidence = confidence_for(&source, &event_type, &text, &candidate);
    let source_group = candidate.source_group.clone();
    let source_trust = candidate.source_trust;
    let source_categories = candidate.source_categories.clone();
    let official_source = candidate.official_source;
    let strategy_relevant = has_strategy_relevance(&text);
    let needs_codex_review = event_type != "background"
        && strategy_relevant
        && (official_source || source_group == "investment_data" || confidence != "low" || !assets.is_empty());
    ResearchItem {
        id: hash_id("rsi", &candidate.url),
        title: candidate.title,
        url: candidate.url,
        source: if candidate.source_name.trim().is_empty() { source } else { candidate.source_name },
        snippet: candidate.snippet,
        discovered_at: Utc::now().to_rfc3339(),
        published_at: candidate.published_at,
        source_group,
        source_trust,
        source_categories,
        official_source,
        assets: assets.clone(),
        event_type: event_type.clone(),
        relevance: relevance_for(&event_type, &assets),
        impact_window: impact_window_for(&event_type),
        confidence,
        needs_codex_review,
    }
}

fn xml_text(block: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}");
    let start = block.find(&start_tag)?;
    let after_start = &block[start..];
    let content_start = after_start.find('>')? + start + 1;
    let end_tag = format!("</{tag}>");
    let content_end = block[content_start..].find(&end_tag)? + content_start;
    let raw = block[content_start..content_end].trim();
    let raw = raw
        .trim_start_matches("<![CDATA[")
        .trim_end_matches("]]>")
        .trim();
    Some(decode_xml(raw))
}

fn decode_xml(text: &str) -> String {
    let no_tags = strip_tags(text);
    no_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn parse_rss_items(xml: &str, source: &ResearchSource) -> Vec<NewsCandidate> {
    xml.split("<item")
        .skip(1)
        .filter_map(|chunk| {
            let block = format!("<item{chunk}");
            let title = xml_text(&block, "title")?;
            let url = xml_text(&block, "link")
                .or_else(|| xml_text(&block, "guid"))?;
            let snippet = xml_text(&block, "description")
                .or_else(|| xml_text(&block, "content:encoded"))
                .unwrap_or_default();
            if title.trim().is_empty() || url.trim().is_empty() {
                return None;
            }
            Some(NewsCandidate {
                title,
                url,
                snippet,
                published_at: xml_text(&block, "pubDate").or_else(|| xml_text(&block, "dc:date")),
                source_name: source.name.clone(),
                source_group: source.group.clone(),
                source_trust: source.trust,
                source_categories: source.categories.clone(),
                official_source: source.official,
            })
        })
        .collect()
}

fn candidate_matches_goal(candidate: &NewsCandidate, goal: &ResearchGoal) -> bool {
    let haystack = format!("{} {}", candidate.title, candidate.snippet).to_lowercase();
    let keyword_match = goal
        .keywords
        .iter()
        .any(|kw| haystack.contains(&kw.to_lowercase()))
        || haystack.contains("bitcoin")
        || haystack.contains("crypto")
        || haystack.contains("stablecoin");
    keyword_match && has_strategy_relevance(&haystack)
}

async fn fetch_rss_candidates(sources: &[ResearchSource], goal: &ResearchGoal) -> (Vec<NewsCandidate>, Vec<String>) {
    let client = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(client) => client,
        Err(e) => return (Vec::new(), vec![format!("RSS client build failed: {e}")]),
    };

    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    for source in sources {
        match client.get(&source.url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(ok) => match ok.text().await {
                    Ok(xml) => {
                        candidates.extend(
                            parse_rss_items(&xml, source)
                                .into_iter()
                                .filter(|item| candidate_matches_goal(item, goal)),
                        );
                    }
                    Err(e) => errors.push(format!("{}: read RSS body failed: {e}", source.name)),
                },
                Err(e) => errors.push(format!("{}: RSS status error: {e}", source.name)),
            },
            Err(e) => errors.push(format!("{}: RSS request failed: {e}", source.name)),
        }
    }
    (candidates, errors)
}

fn summarize(topic: &str, events: &[ResearchEvent], errors: &[String]) -> String {
    let review_count = events.iter().filter(|event| event.needs_codex_review).count();
    let critical_types = events
        .iter()
        .filter(|event| event.needs_codex_review)
        .map(|event| event.event_type.as_str())
        .collect::<HashSet<_>>()
        .len();
    if events.is_empty() {
        format!("No usable research items found for `{topic}`. Source errors: {}.", errors.len())
    } else if review_count > 0 {
        format!("Found {review_count} Codex-review event(s) across {critical_types} event type(s) for `{topic}`.")
    } else {
        format!("Found {} background event(s) for `{topic}`, with no direct Codex-review flag.", events.len())
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn report_file_path(entry: &ResearchInboxEntry) -> PathBuf {
    report_dir().join(format!("research_sentinel_{}_{}.html", entry.run_id, entry.id))
}

fn render_research_report_html(entry: &ResearchInboxEntry) -> String {
    let mut html = String::new();
    let _ = write!(
        html,
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>\
<style>\
body{{font-family:Arial,sans-serif;background:#0b0f10;color:#d7e0dc;margin:0;padding:24px;line-height:1.45}}\
a{{color:#7dd3fc}}.meta,.guardrail,.error{{color:#94a3b8}}\
.event{{border:1px solid #263238;border-radius:8px;padding:16px;margin:16px 0;background:#111719}}\
.pill{{display:inline-block;border:1px solid #334155;border-radius:999px;padding:2px 8px;margin:0 6px 6px 0;color:#cbd5e1;font-size:12px}}\
.convert_to_issue{{border-color:#22c55e}}.watch{{border-color:#f59e0b}}.ignore{{opacity:.72}}\
pre{{white-space:pre-wrap;background:#050708;padding:12px;border-radius:6px;color:#cbd5e1}}\
</style></head><body>",
        html_escape(&entry.topic)
    );
    let _ = write!(
        html,
        "<h1>{}</h1><p class=\"meta\">Inbox: {} | Run: {} | Created: {} | Review: {}</p>\
<p>{}</p><p class=\"meta\">Focus: {}</p>\
<p class=\"meta\">Events: {} | Items: {} | Source errors: {}</p>",
        html_escape(&entry.topic),
        html_escape(&entry.id),
        html_escape(&entry.run_id),
        html_escape(&entry.created_at),
        html_escape(&entry.review_status),
        html_escape(&entry.summary),
        html_escape(&entry.focus),
        entry.events.len(),
        entry.items.len(),
        entry.source_errors.len()
    );
    if let Some(note) = &entry.review_note {
        let _ = write!(html, "<h2>Codex Review</h2><pre>{}</pre>", html_escape(note));
    }
    html.push_str("<h2>Events</h2>");
    for event in &entry.events {
        let _ = write!(
            html,
            "<section class=\"event {}\"><h3>{}</h3>\
<p><span class=\"pill\">{}</span><span class=\"pill\">score {}</span><span class=\"pill\">{}</span><span class=\"pill\">{}</span></p>\
<p>{}</p><p>{}</p>",
            html_escape(&event.codex_recommendation),
            html_escape(&event.title),
            html_escape(&event.event_type),
            event.review_score,
            html_escape(&event.confidence),
            html_escape(&event.codex_recommendation),
            html_escape(&event.relevance),
            html_escape(&event.summary)
        );
        html.push_str("<h4>Sources</h4><ul>");
        for source in &event.sources {
            let _ = write!(
                html,
                "<li><a href=\"{}\">{}</a> <span class=\"pill\">{}</span><span class=\"pill\">trust {}</span><br>{}</li>",
                html_escape(&source.url),
                html_escape(&source.title),
                html_escape(&source.source_group),
                source.source_trust,
                html_escape(&source.snippet)
            );
        }
        html.push_str("</ul></section>");
    }
    if !entry.source_errors.is_empty() {
        html.push_str("<h2>Source Errors</h2><ul>");
        for error in &entry.source_errors {
            let _ = write!(html, "<li class=\"error\">{}</li>", html_escape(error));
        }
        html.push_str("</ul>");
    }
    if !entry.guardrails.is_empty() {
        html.push_str("<h2>Guardrails</h2><ul>");
        for guardrail in &entry.guardrails {
            let _ = write!(html, "<li class=\"guardrail\">{}</li>", html_escape(guardrail));
        }
        html.push_str("</ul>");
    }
    html.push_str("</body></html>");
    html
}

fn write_research_report(entry: &ResearchInboxEntry) -> Result<PathBuf, String> {
    let path = report_file_path(entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create report dir failed: {e}"))?;
    }
    std::fs::write(&path, render_research_report_html(entry)).map_err(|e| format!("write report failed: {e}"))?;
    Ok(path)
}

fn render_kb_intelligence_note(entry: &ResearchInboxEntry, event: &ResearchEvent) -> String {
    let mut content = String::new();
    let _ = writeln!(content, "# {}", event.title);
    let _ = writeln!(content);
    let _ = writeln!(content, "- inbox_id: {}", entry.id);
    let _ = writeln!(content, "- run_id: {}", entry.run_id);
    let _ = writeln!(content, "- topic: {}", entry.topic);
    let _ = writeln!(content, "- event_type: {}", event.event_type);
    let _ = writeln!(content, "- occurrence_status: {}", event.occurrence_status);
    let _ = writeln!(content, "- recommendation: {}", event.codex_recommendation);
    let _ = writeln!(content, "- review_score: {}", event.review_score);
    let _ = writeln!(content, "- confidence: {}", event.confidence);
    let _ = writeln!(content, "- review_status: {}", entry.review_status);
    if let Some(path) = &entry.report_path {
        let _ = writeln!(content, "- html_report: {}", path);
    }
    if let Some(note) = &entry.review_note {
        let _ = writeln!(content, "- codex_review_note: {}", note);
    }
    let _ = writeln!(content);
    let _ = writeln!(content, "## Why It Matters");
    let _ = writeln!(content, "{}", event.relevance);
    let _ = writeln!(content);
    let _ = writeln!(content, "## Summary");
    let _ = writeln!(content, "{}", event.summary);
    let _ = writeln!(content);
    let _ = writeln!(content, "## Sources");
    for source in &event.sources {
        let _ = writeln!(
            content,
            "- [{}]({}) — group={}, trust={}, published={}",
            source.title,
            source.url,
            source.source_group,
            source.source_trust,
            source.published_at.clone().unwrap_or_else(|| "unknown".into())
        );
    }
    let _ = writeln!(content);
    let _ = writeln!(content, "## Guardrails");
    for guardrail in &entry.guardrails {
        let _ = writeln!(content, "- {}", guardrail);
    }
    content
}

async fn publish_entry_to_kb(entry: &mut ResearchInboxEntry) -> usize {
    let mut published = 0usize;
    let report_path = entry.report_path.clone().unwrap_or_default();
    let file_refs = if report_path.is_empty() { String::new() } else { report_path };
    let review_status = entry.review_status.clone();
    let snapshot = entry.clone();
    for event in entry.events.iter_mut() {
        if !event_should_publish_to_kb(event, &review_status) {
            if event.kb_publish_status.is_empty() || event.kb_publish_status == "candidate" {
                event.kb_publish_status = "not_eligible".into();
            }
            continue;
        }
        let topic_key = event.kb_topic_key.clone().unwrap_or_else(|| kb_topic_key_for(event));
        let title = format!("Research Sentinel: {}", event.title);
        let content = render_kb_intelligence_note(&snapshot, event);
        match crate::kb_client::write_raw_to_project(
            "sirin",
            &topic_key,
            &title,
            &content,
            "research-sentinel",
            "research_sentinel,official_investment,agoramarketapi_guardrails",
            &file_refs,
        )
        .await
        {
            Ok(()) => {
                event.kb_topic_key = Some(topic_key);
                event.kb_publish_status = "published_or_skipped_by_env".into();
                published += 1;
            }
            Err(e) => {
                event.kb_topic_key = Some(topic_key);
                event.kb_publish_status = format!("error: {e}");
            }
        }
    }
    entry.kb_publish_count = published;
    published
}

fn monitor_args(monitor: &ResearchMonitorConfig) -> Value {
    json!({
        "topic": monitor.topic.clone(),
        "focus": monitor.focus.clone(),
        "keywords": monitor.keywords.clone(),
        "lookback_hours": monitor.lookback_hours.clamp(1, 168),
        "max_results": monitor.max_results.clamp(1, 30),
        "create_inbox": true,
        "publish_to_kb": monitor.publish_to_kb,
        "create_report": "auto",
    })
}

fn upsert_monitor_status(status: ResearchMonitorStatus) {
    let mut statuses = monitor_statuses().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = statuses.iter_mut().find(|item| item.id == status.id) {
        *existing = status;
    } else {
        statuses.push(status);
    }
}

fn status_from_run(monitor: &ResearchMonitorConfig, next_run_at: String, run: &Value) -> ResearchMonitorStatus {
    ResearchMonitorStatus {
        id: monitor.id.clone(),
        enabled: monitor.enabled,
        interval_minutes: monitor.interval_minutes,
        last_run_at: Some(Utc::now().to_rfc3339()),
        next_run_at: Some(next_run_at),
        last_inbox_id: run.get("inbox_id").and_then(Value::as_str).map(str::to_string),
        last_report_path: run.get("report_path").and_then(Value::as_str).map(str::to_string),
        last_summary: run.get("summary").and_then(Value::as_str).map(str::to_string),
        last_error: None,
        event_count: run.get("event_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        review_count: run.get("review_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        new_event_count: run.get("new_event_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        escalated_event_count: run.get("escalated_event_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        kb_publish_count: run.get("kb_publish_count").and_then(Value::as_u64).unwrap_or(0) as usize,
    }
}

fn error_monitor_status(monitor: &ResearchMonitorConfig, next_run_at: String, error: String) -> ResearchMonitorStatus {
    ResearchMonitorStatus {
        id: monitor.id.clone(),
        enabled: monitor.enabled,
        interval_minutes: monitor.interval_minutes,
        last_run_at: Some(Utc::now().to_rfc3339()),
        next_run_at: Some(next_run_at),
        last_inbox_id: None,
        last_report_path: None,
        last_summary: None,
        last_error: Some(error),
        event_count: 0,
        review_count: 0,
        new_event_count: 0,
        escalated_event_count: 0,
        kb_publish_count: 0,
    }
}

fn should_create_report(args: &Value, entry: &ResearchInboxEntry) -> bool {
    match args.get("create_report") {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("always") => true,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("never") => false,
        Some(Value::String(value)) if value.eq_ignore_ascii_case("auto") => entry_is_report_worthy(entry),
        Some(_) => entry_is_report_worthy(entry),
        None => true,
    }
}

pub(crate) fn spawn_monitor_loop() {
    tokio::spawn(async {
        let mut next_due: HashMap<String, Instant> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            let cfg = load_monitor_file();
            for monitor in cfg.monitors.iter().filter(|monitor| monitor.enabled) {
                let interval_minutes = monitor.interval_minutes.max(1);
                let due = next_due.entry(monitor.id.clone()).or_insert_with(Instant::now);
                if Instant::now() < *due {
                    continue;
                }
                *due = Instant::now() + Duration::from_secs(interval_minutes * 60);
                let next_run_at = (Utc::now() + chrono::Duration::minutes(interval_minutes as i64)).to_rfc3339();
                tracing::info!(target: "sirin", "[research_sentinel] monitor {} running", monitor.id);
                match call_research_sentinel_run_once(monitor_args(monitor)).await {
                    Ok(run) => {
                        let status = status_from_run(monitor, next_run_at, &run);
                        tracing::info!(
                            target: "sirin",
                            "[research_sentinel] monitor {} complete: new={}, escalated={}, report={:?}",
                            status.id,
                            status.new_event_count,
                            status.escalated_event_count,
                            status.last_report_path
                        );
                        upsert_monitor_status(status);
                    }
                    Err(e) => {
                        tracing::warn!(target: "sirin", "[research_sentinel] monitor {} failed: {e}", monitor.id);
                        upsert_monitor_status(error_monitor_status(monitor, next_run_at, e));
                    }
                }
            }
        }
    });
}

pub(crate) fn call_research_sentinel_goal_create(args: Value) -> Result<Value, String> {
    let goal = build_goal_from_args(&args);
    goal_log()
        .upsert_by(goal.clone(), |entry| entry.id.clone())
        .map_err(|e| e.to_string())?;
    Ok(json!({ "goal": goal }))
}

pub(crate) async fn call_research_sentinel_run_once(args: Value) -> Result<Value, String> {
    let mut goal = if let Some(goal_id) = args.get("goal_id").and_then(Value::as_str) {
        find_goal(goal_id)?.ok_or_else(|| format!("Unknown research sentinel goal_id: {goal_id}"))?
    } else {
        build_goal_from_args(&args)
    };

    if let Some(lookback_hours) = args.get("lookback_hours").and_then(Value::as_u64) {
        goal.lookback_hours = lookback_hours.clamp(1, 168);
    }

    let max_results = args.get("max_results").and_then(Value::as_u64).unwrap_or(12).clamp(1, 30) as usize;
    let create_inbox = args.get("create_inbox").and_then(Value::as_bool).unwrap_or(true);
    let publish_to_kb = args.get("publish_to_kb").and_then(Value::as_bool).unwrap_or(false);
    let sources = sources_arg(&args);

    let mut source_errors = Vec::new();
    let mut seen_urls = HashSet::new();
    let mut items = Vec::new();

    let (rss_candidates, rss_errors) = fetch_rss_candidates(&sources, &goal).await;
    source_errors.extend(rss_errors);
    for candidate in rss_candidates {
        if seen_urls.insert(candidate.url.to_lowercase()) {
            items.push(item_from_candidate(candidate));
        }
        if items.len() >= max_results {
            break;
        }
    }

    items.sort_by_key(|item| {
        let review_rank = if item.needs_codex_review { 0 } else { 1 };
        let confidence_rank = match item.confidence.as_str() {
            "high" => 0,
            "medium" => 1,
            _ => 2,
        };
        (review_rank, confidence_rank, item.source.clone())
    });

    let run_id = format!("rsr-{}", Utc::now().timestamp_millis());
    let inbox_id = hash_id("rsn", &format!("{}:{run_id}", goal.id));
    let mut events = aggregate_events(&items);
    if create_inbox {
        apply_event_state(&mut events)?;
    }
    let mut entry = ResearchInboxEntry {
        id: inbox_id.clone(),
        run_id: run_id.clone(),
        goal_id: Some(goal.id.clone()),
        created_at: Utc::now().to_rfc3339(),
        status: "unread".into(),
        review_status: "pending".into(),
        review_note: None,
        reviewed_at: None,
        topic: goal.topic.clone(),
        focus: goal.focus.clone(),
        summary: summarize(&goal.topic, &events, &source_errors),
        events,
        items,
        source_errors,
        guardrails: vec![
            "Research only; no trading instruction was generated.".into(),
            "Sirin did not read or mutate AgoraMarketAPI trading state.".into(),
            "Codex must review sources before converting findings into engineering or risk-control work.".into(),
        ],
        report_path: None,
        kb_publish_count: 0,
    };

    if create_inbox {
        if should_create_report(&args, &entry) {
            let report_path = write_research_report(&entry)?;
            entry.report_path = Some(report_path.to_string_lossy().to_string());
        }
        if publish_to_kb {
            publish_entry_to_kb(&mut entry).await;
            if entry.report_path.is_some() {
                let _ = write_research_report(&entry);
            }
        }
        inbox_log()
            .upsert_by(entry.clone(), |item| item.id.clone())
            .map_err(|e| e.to_string())?;
    }

    Ok(json!({
        "run_id": run_id,
        "inbox_id": if create_inbox { Some(inbox_id) } else { None },
        "summary": entry.summary,
        "count": entry.items.len(),
        "event_count": entry.events.len(),
        "review_count": entry.events.iter().filter(|event| event.needs_codex_review).count(),
        "new_event_count": entry.events.iter().filter(|event| event.occurrence_status == "new").count(),
        "escalated_event_count": entry.events.iter().filter(|event| event.occurrence_status == "escalated").count(),
        "kb_publish_count": entry.kb_publish_count,
        "report_path": entry.report_path,
        "entry": entry,
    }))
}

pub(crate) fn call_research_sentinel_inbox(args: Value) -> Result<Value, String> {
    let unread_only = args.get("unread_only").and_then(Value::as_bool).unwrap_or(true);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100) as usize;
    let mut entries = inbox_log().read_all().map_err(|e| e.to_string())?;
    entries.reverse();
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !unread_only || entry.status == "unread")
        .take(limit)
        .collect();
    Ok(json!({
        "unread_only": unread_only,
        "count": entries.len(),
        "entries": entries,
    }))
}

pub(crate) fn call_research_sentinel_monitor_status(_args: Value) -> Result<Value, String> {
    let cfg = load_monitor_file();
    let statuses = monitor_statuses()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut out = Vec::new();
    for monitor in cfg.monitors {
        if let Some(status) = statuses.iter().find(|status| status.id == monitor.id) {
            out.push(status.clone());
        } else {
            out.push(ResearchMonitorStatus {
                id: monitor.id,
                enabled: monitor.enabled,
                interval_minutes: monitor.interval_minutes,
                last_run_at: None,
                next_run_at: None,
                last_inbox_id: None,
                last_report_path: None,
                last_summary: None,
                last_error: None,
                event_count: 0,
                review_count: 0,
                new_event_count: 0,
                escalated_event_count: 0,
                kb_publish_count: 0,
            });
        }
    }
    Ok(json!({
        "config_file": crate::platform::config_path(MONITOR_CONFIG_FILE),
        "count": out.len(),
        "monitors": out,
    }))
}

pub(crate) fn call_research_sentinel_ack(args: Value) -> Result<Value, String> {
    let id = args.get("id").and_then(Value::as_str).ok_or("Missing id")?.to_string();
    let mut changed = false;
    inbox_log()
        .rewrite_with(|mut entries| {
            for entry in entries.iter_mut() {
                if entry.id == id {
                    entry.status = "read".into();
                    changed = true;
                    break;
                }
            }
            entries
        })
        .map_err(|e| e.to_string())?;
    Ok(json!({ "id": id, "acknowledged": changed }))
}

pub(crate) async fn call_research_sentinel_review(args: Value) -> Result<Value, String> {
    let id = args.get("id").and_then(Value::as_str).ok_or("Missing id")?.to_string();
    let review_status = args
        .get("review_status")
        .and_then(Value::as_str)
        .ok_or("Missing review_status")?
        .trim()
        .to_string();
    let allowed = ["accepted", "ignored", "needs_more_sources", "convert_to_issue"];
    if !allowed.contains(&review_status.as_str()) {
        return Err(format!(
            "Invalid review_status `{review_status}`; expected one of {}",
            allowed.join(", ")
        ));
    }
    let review_note = args
        .get("review_note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut updated: Option<ResearchInboxEntry> = None;
    inbox_log()
        .rewrite_with(|mut entries| {
            for entry in entries.iter_mut() {
                if entry.id == id {
                    entry.review_status = review_status.clone();
                    entry.review_note = review_note.clone();
                    entry.reviewed_at = Some(Utc::now().to_rfc3339());
                    entry.status = "read".into();
                    updated = Some(entry.clone());
                    break;
                }
            }
            entries
        })
        .map_err(|e| e.to_string())?;

    match updated {
        Some(mut entry) => {
            if entry.report_path.is_none() {
                entry.report_path = Some(report_file_path(&entry).to_string_lossy().to_string());
            }
            if matches!(entry.review_status.as_str(), "accepted" | "convert_to_issue") {
                publish_entry_to_kb(&mut entry).await;
            }
            let _ = write_research_report(&entry);
            inbox_log()
                .upsert_by(entry.clone(), |item| item.id.clone())
                .map_err(|e| e.to_string())?;
            Ok(json!({ "updated": true, "entry": entry }))
        }
        None => Ok(json!({ "updated": false, "id": id })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_source(name: &str, group: &str, trust: u8, categories: Vec<&str>) -> ResearchSource {
        ResearchSource {
            name: name.into(),
            url: format!("https://example.com/{name}.rss"),
            kind: "rss".into(),
            group: group.into(),
            categories: categories.into_iter().map(str::to_string).collect(),
            trust,
            official: group == "official",
            priority: 1,
            enabled: true,
        }
    }

    #[test]
    fn classifies_crypto_trading_news_context() {
        let text = "SEC approves spot Bitcoin ETF while Binance faces liquidity risk";
        assert_eq!(classify_event(text), "liquidity");
        let assets = detect_assets(text);
        assert!(assets.contains(&"BTC".to_string()));
        assert!(assets.contains(&"BNB".to_string()));
    }

    #[test]
    fn search_queries_use_codex_defined_goal() {
        let goal = ResearchGoal {
            id: "g".into(),
            created_at: Utc::now().to_rfc3339(),
            status: "active".into(),
            topic: "BTCUSDT automated trading risk".into(),
            focus: "macro and exchange news".into(),
            keywords: vec!["Fed".into(), "Binance".into()],
            lookback_hours: 24,
        };
        let queries = search_queries(&goal, 2);
        assert_eq!(queries.len(), 2);
        assert!(queries[0].contains("BTCUSDT automated trading risk"));
        assert!(queries[0].contains("Fed"));
    }

    #[test]
    fn item_from_search_sets_review_fields_without_trading_advice() {
        let item = item_from_search(SearchResult {
            title: "Binance announces wallet maintenance for Bitcoin withdrawals".into(),
            url: "https://www.binance.com/en/support/announcement/example".into(),
            snippet: "BTC withdrawals paused during scheduled maintenance".into(),
        });
        assert_eq!(item.event_type, "exchange_ops");
        assert_eq!(item.confidence, "high");
        assert!(item.needs_codex_review);
        assert!(!item.relevance.to_lowercase().contains("buy"));
        assert!(!item.relevance.to_lowercase().contains("sell"));
    }

    #[test]
    fn parses_rss_items_and_keeps_source_metadata() {
        let xml = r#"
            <rss><channel>
              <item>
                <title><![CDATA[Bitcoin ETF flows affect market volatility]]></title>
                <link>https://www.coindesk.com/markets/example</link>
                <description><![CDATA[BTC liquidity and volatility changed after ETF flows.]]></description>
                <pubDate>Sun, 24 May 2026 00:00:00 +0000</pubDate>
              </item>
            </channel></rss>
        "#;
        let source = test_source("Federal Reserve", "official", 100, vec!["macro_data"]);
        let items = parse_rss_items(xml, &source);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Bitcoin ETF flows affect market volatility");
        assert_eq!(items[0].published_at.as_deref(), Some("Sun, 24 May 2026 00:00:00 +0000"));
        assert!(items[0].official_source);
        assert_eq!(items[0].source_trust, 100);
    }

    #[test]
    fn aggregates_items_into_reviewable_events() {
        let items = vec![
            item_from_search(SearchResult {
                title: "Binance announces Bitcoin wallet maintenance".into(),
                url: "https://www.binance.com/a".into(),
                snippet: "BTC withdrawals paused during scheduled maintenance".into(),
            }),
            item_from_search(SearchResult {
                title: "Binance announces Bitcoin wallet maintenance update".into(),
                url: "https://www.binance.com/b".into(),
                snippet: "BTC maintenance update from Binance".into(),
            }),
        ];
        let events = aggregate_events(&items);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.needs_codex_review));
        assert!(events.iter().any(|event| event.assets.contains(&"BTC".to_string())));
    }

    #[test]
    fn load_config_falls_back_to_defaults() {
        let cfg = default_config();
        assert!(!cfg.default_keywords.is_empty());
        assert!(cfg.official_only);
        assert!(cfg.sources.iter().any(|source| source.group == "official"));
        assert!(cfg.sources.iter().any(|source| source.group == "investment_data"));
    }

    #[test]
    fn official_events_get_convert_to_issue_recommendation() {
        let item = item_from_candidate(NewsCandidate {
            title: "SEC announces crypto market structure rule proposal".into(),
            url: "https://www.sec.gov/news/press-release/example".into(),
            snippet: "The SEC announces a regulatory proposal for crypto market structure.".into(),
            published_at: Some("Sun, 24 May 2026 00:00:00 +0000".into()),
            source_name: "SEC Press Releases".into(),
            source_group: "official".into(),
            source_trust: 100,
            source_categories: vec!["regulation".into()],
            official_source: true,
        });
        let events = aggregate_events(&[item]);
        assert_eq!(events[0].codex_recommendation, "convert_to_issue");
        assert!(events[0].review_score >= 75);
        assert!(events[0].source_groups.contains(&"official".to_string()));
    }

    #[test]
    fn generic_official_sec_item_is_ignored_without_strategy_relevance() {
        let candidate = NewsCandidate {
            title: "SEC proposes reforms to help public companies conduct registered offerings".into(),
            url: "https://www.sec.gov/news/press-release/generic".into(),
            snippet: "The SEC proposed amendments to simplify reporting requirements for public companies.".into(),
            published_at: Some("Sun, 24 May 2026 00:00:00 +0000".into()),
            source_name: "SEC Press Releases".into(),
            source_group: "official".into(),
            source_trust: 100,
            source_categories: vec!["regulation".into(), "policy".into()],
            official_source: true,
        };
        let goal = ResearchGoal {
            id: "g".into(),
            created_at: Utc::now().to_rfc3339(),
            status: "active".into(),
            topic: "AgoraMarketAPI auto trading official monitor".into(),
            focus: "official market and crypto risk".into(),
            keywords: vec!["SEC".into()],
            lookback_hours: 24,
        };
        assert!(!candidate_matches_goal(&candidate, &goal));
        let item = item_from_candidate(candidate);
        assert_eq!(item.event_type, "background");
        assert!(item.assets.is_empty());
        assert!(!item.needs_codex_review);
        let events = aggregate_events(&[item]);
        assert_eq!(events[0].codex_recommendation, "ignore");
        assert!(!events[0].needs_codex_review);
    }

    #[test]
    fn federal_reserve_macro_item_is_not_stablecoin_risk() {
        let item = item_from_candidate(NewsCandidate {
            title: "Federal Reserve issues FOMC statement".into(),
            url: "https://www.federalreserve.gov/newsevents/pressreleases/example.htm".into(),
            snippet: "The Federal Open Market Committee maintained the target range for the federal funds rate.".into(),
            published_at: Some("Sun, 24 May 2026 00:00:00 +0000".into()),
            source_name: "Federal Reserve Press Releases".into(),
            source_group: "official".into(),
            source_trust: 100,
            source_categories: vec!["macro_calendar".into(), "macro_data".into()],
            official_source: true,
        });
        assert_eq!(item.event_type, "macro_calendar");
        assert!(item.needs_codex_review);
        assert!(!item.assets.contains(&"USDT".to_string()));
    }

    #[test]
    fn renders_human_readable_html_report() {
        let item = item_from_candidate(NewsCandidate {
            title: "SEC announces crypto market structure rule proposal".into(),
            url: "https://www.sec.gov/news/press-release/example".into(),
            snippet: "The SEC announces a regulatory proposal for crypto market structure.".into(),
            published_at: Some("Sun, 24 May 2026 00:00:00 +0000".into()),
            source_name: "SEC Press Releases".into(),
            source_group: "official".into(),
            source_trust: 100,
            source_categories: vec!["regulation".into()],
            official_source: true,
        });
        let events = aggregate_events(&[item.clone()]);
        let entry = ResearchInboxEntry {
            id: "rsn-test".into(),
            run_id: "rsr-test".into(),
            goal_id: Some("goal-test".into()),
            created_at: "2026-05-24T00:00:00Z".into(),
            status: "unread".into(),
            review_status: "pending".into(),
            review_note: None,
            reviewed_at: None,
            topic: "HTML report smoke".into(),
            focus: "official crypto market structure".into(),
            summary: "Found one review event.".into(),
            events,
            items: vec![item],
            source_errors: Vec::new(),
            guardrails: vec!["Research only; no trading instruction was generated.".into()],
            report_path: None,
            kb_publish_count: 0,
        };
        let html = render_research_report_html(&entry);
        assert!(html.contains("SEC announces crypto market structure rule proposal"));
        assert!(html.contains("convert_to_issue"));
        assert!(html.contains("https://www.sec.gov/news/press-release/example"));
        assert!(html.contains("Research only; no trading instruction was generated."));
    }

    #[test]
    fn report_policy_respects_manual_and_auto_modes() {
        let item = item_from_candidate(NewsCandidate {
            title: "Bitcoin slips below $77,000 as BTC cools".into(),
            url: "https://www.investing.com/rss/example".into(),
            snippet: "Bitcoin price cools without a new official action.".into(),
            published_at: None,
            source_name: "Investing.com Cryptocurrency News".into(),
            source_group: "investment_data".into(),
            source_trust: 70,
            source_categories: vec!["market_flow".into()],
            official_source: false,
        });
        let mut events = aggregate_events(&[item.clone()]);
        events[0].occurrence_status = "seen".into();
        let entry = ResearchInboxEntry {
            id: "rsn-policy".into(),
            run_id: "rsr-policy".into(),
            goal_id: None,
            created_at: "2026-05-24T00:00:00Z".into(),
            status: "unread".into(),
            review_status: "pending".into(),
            review_note: None,
            reviewed_at: None,
            topic: "Report policy".into(),
            focus: "avoid HTML noise".into(),
            summary: "Seen watch item.".into(),
            events,
            items: vec![item],
            source_errors: Vec::new(),
            guardrails: Vec::new(),
            report_path: None,
            kb_publish_count: 0,
        };
        assert!(should_create_report(&json!({}), &entry));
        assert!(!should_create_report(&json!({ "create_report": "auto" }), &entry));
        assert!(!should_create_report(&json!({ "create_report": false }), &entry));
        assert!(should_create_report(&json!({ "create_report": true }), &entry));
    }

    #[test]
    fn classifies_event_occurrence_for_kb_pipeline() {
        let item = item_from_candidate(NewsCandidate {
            title: "SEC announces crypto market structure rule proposal".into(),
            url: "https://www.sec.gov/news/press-release/example".into(),
            snippet: "The SEC announces a regulatory proposal for crypto market structure.".into(),
            published_at: Some("Sun, 24 May 2026 00:00:00 +0000".into()),
            source_name: "SEC Press Releases".into(),
            source_group: "official".into(),
            source_trust: 100,
            source_categories: vec!["regulation".into()],
            official_source: true,
        });
        let event = aggregate_events(&[item])[0].clone();
        assert_eq!(classify_occurrence(&event, None), "new");
        let previous = ResearchEventState {
            event_key: event.event_key.clone(),
            first_seen_at: "2026-05-24T00:00:00Z".into(),
            last_seen_at: "2026-05-24T00:00:00Z".into(),
            seen_count: 1,
            last_review_score: event.review_score,
            last_recommendation: event.codex_recommendation.clone(),
            last_title: event.title.clone(),
            last_kb_topic_key: kb_topic_key_for(&event),
        };
        assert_eq!(classify_occurrence(&event, Some(&previous)), "seen");
        assert!(event_should_publish_to_kb(&event, "accepted"));
        assert!(kb_topic_key_for(&event).starts_with("research-sentinel-event-"));
    }

    #[test]
    fn investment_data_single_source_is_watch_not_trade_advice() {
        let item = item_from_candidate(NewsCandidate {
            title: "Bitcoin ETF outflows pressure crypto market liquidity".into(),
            url: "https://www.investing.com/rss/example".into(),
            snippet: "ETF outflows and lower liquidity may affect market volatility.".into(),
            published_at: None,
            source_name: "Investing.com Cryptocurrency News".into(),
            source_group: "investment_data".into(),
            source_trust: 70,
            source_categories: vec!["market_flow".into(), "etf_flow".into()],
            official_source: false,
        });
        let events = aggregate_events(&[item]);
        assert_eq!(events[0].event_type, "etf_flow");
        assert_eq!(events[0].codex_recommendation, "watch");
        assert!(!events[0].relevance.to_lowercase().contains("buy"));
        assert!(!events[0].relevance.to_lowercase().contains("sell"));
    }
}
