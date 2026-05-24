//! News/event research sentinel for Codex-defined review goals.
//!
//! This module is intentionally read-only.  It searches external public
//! sources, classifies news relevance for an AgoraMarketAPI automation goal,
//! and stores a Codex-readable inbox item.  It does not read or mutate
//! AgoraMarketAPI trading state.

use std::collections::{HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::jsonl_log::JsonlLog;
use crate::skills::{ddg_search, SearchResult};

const DEFAULT_TOPIC: &str = "AgoraMarketAPI automated trading crypto news";
const DEFAULT_FOCUS: &str = "events that may affect automated trading volatility, liquidity, exchange risk, stablecoin risk, regulation, or macro risk";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

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
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchSentinelConfig {
    #[serde(default = "default_keywords")]
    pub default_keywords: Vec<String>,
    #[serde(default = "default_config_sources")]
    pub sources: Vec<ResearchSource>,
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
    pub relevance: String,
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
}

#[derive(Debug, Clone)]
struct NewsCandidate {
    title: String,
    url: String,
    snippet: String,
    published_at: Option<String>,
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

fn default_source_kind() -> String {
    "rss".into()
}

fn default_config_sources() -> Vec<ResearchSource> {
    default_rss_feeds()
        .into_iter()
        .enumerate()
        .map(|(idx, url)| ResearchSource {
            name: source_domain(&url),
            url,
            kind: "rss".into(),
            priority: idx as u8,
            enabled: true,
        })
        .collect()
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
    vec![
        "https://www.coindesk.com/arc/outboundfeeds/rss/".into(),
        "https://cointelegraph.com/rss".into(),
    ]
}

fn default_config() -> ResearchSentinelConfig {
    ResearchSentinelConfig {
        default_keywords: default_keywords(),
        sources: default_config_sources(),
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

fn rss_feeds_arg(args: &Value) -> Vec<String> {
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
    if feeds.is_empty() {
        let mut sources = load_config().sources;
        sources.sort_by_key(|source| source.priority);
        let configured = sources
            .into_iter()
            .filter(|source| source.enabled && source.kind == "rss")
            .map(|source| source.url)
            .collect::<Vec<_>>();
        if configured.is_empty() {
            default_rss_feeds()
        } else {
            configured
        }
    } else {
        feeds
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

fn classify_event(text: &str) -> String {
    let lower = text.to_lowercase();
    if contains_any(&lower, &["sec", "cftc", "lawsuit", "regulation", "regulatory", "etf", "treasury"]) {
        "regulation".into()
    } else if contains_any(&lower, &["federal reserve", "fomc", "cpi", "inflation", "rate cut", "interest rate", "nfp"]) {
        "macro".into()
    } else if contains_any(&lower, &["binance", "coinbase", "okx", "bybit", "kraken", "exchange", "listing", "delisting", "maintenance"]) {
        "exchange".into()
    } else if contains_any(&lower, &["hack", "exploit", "breach", "stolen", "security", "phishing"]) {
        "security".into()
    } else if contains_any(&lower, &["liquidity", "stablecoin", "usdt", "usdc", "depeg", "reserve", "redemption"]) {
        "liquidity".into()
    } else if contains_any(&lower, &["volatility", "open interest", "funding", "liquidation", "market maker"]) {
        "market_structure".into()
    } else {
        "other".into()
    }
}

fn detect_assets(text: &str) -> Vec<String> {
    let upper = text.to_uppercase();
    let mut assets = Vec::new();
    for (asset, needles) in [
        ("BTC", vec!["BTC", "BITCOIN"]),
        ("ETH", vec!["ETH", "ETHEREUM"]),
        ("USDT", vec!["USDT", "TETHER"]),
        ("USDC", vec!["USDC"]),
        ("BNB", vec!["BNB", "BINANCE"]),
        ("SOL", vec!["SOL", "SOLANA"]),
    ] {
        if needles.iter().any(|needle| upper.contains(needle)) {
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
        "regulation" => format!("{asset_text}: may change compliance, venue, or volatility assumptions for automated trading risk filters."),
        "macro" => format!("{asset_text}: may affect volatility regime, leverage appetite, and intraday risk limits."),
        "exchange" => format!("{asset_text}: may affect venue liquidity, listing/delisting risk, maintenance windows, or execution quality."),
        "security" => format!("{asset_text}: may trigger market stress, liquidity withdrawal, or exchange-specific risk."),
        "liquidity" => format!("{asset_text}: may affect stablecoin or order-book liquidity assumptions."),
        "market_structure" => format!("{asset_text}: may affect liquidation/funding/open-interest context used by strategy review."),
        _ => "Background item. Codex should only promote it if the source or follow-up evidence shows direct strategy impact.".into(),
    }
}

fn confidence_for(source: &str, event_type: &str, text: &str) -> String {
    let lower = text.to_lowercase();
    if contains_any(source, &["sec.gov", "cftc.gov", "federalreserve.gov", "binance.com", "coinbase.com"]) {
        "high".into()
    } else if event_type != "other" && contains_any(&lower, &["announces", "files", "approves", "halts", "delists", "exploit"]) {
        "medium".into()
    } else if event_type != "other" {
        "medium".into()
    } else {
        "low".into()
    }
}

fn impact_window_for(event_type: &str) -> String {
    match event_type {
        "macro" | "exchange" | "security" | "liquidity" => "intraday".into(),
        "regulation" => "multi-day".into(),
        "market_structure" => "intraday".into(),
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
            ResearchEvent {
                id: hash_id("rse", &event_key),
                event_key,
                event_type: lead.event_type.clone(),
                title: lead.title.clone(),
                summary: lead.snippet.clone(),
                assets: assets.clone(),
                impact_window: lead.impact_window.clone(),
                confidence,
                needs_codex_review: sources.iter().any(|item| item.needs_codex_review),
                relevance: relevance_for(&lead.event_type, &assets),
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
    })
}

fn item_from_candidate(candidate: NewsCandidate) -> ResearchItem {
    let text = format!("{} {}", candidate.title, candidate.snippet);
    let source = source_domain(&candidate.url);
    let event_type = classify_event(&text);
    let assets = detect_assets(&text);
    let confidence = confidence_for(&source, &event_type, &text);
    let needs_codex_review = event_type != "other" && (confidence != "low" || !assets.is_empty());
    ResearchItem {
        id: hash_id("rsi", &candidate.url),
        title: candidate.title,
        url: candidate.url,
        source,
        snippet: candidate.snippet,
        discovered_at: Utc::now().to_rfc3339(),
        published_at: candidate.published_at,
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

fn parse_rss_items(xml: &str) -> Vec<NewsCandidate> {
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
            })
        })
        .collect()
}

fn candidate_matches_goal(candidate: &NewsCandidate, goal: &ResearchGoal) -> bool {
    let haystack = format!("{} {}", candidate.title, candidate.snippet).to_lowercase();
    goal.keywords
        .iter()
        .any(|kw| haystack.contains(&kw.to_lowercase()))
        || haystack.contains("bitcoin")
        || haystack.contains("crypto")
        || haystack.contains("stablecoin")
}

async fn fetch_rss_candidates(feeds: &[String], goal: &ResearchGoal) -> (Vec<NewsCandidate>, Vec<String>) {
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
    for feed in feeds {
        match client.get(feed).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(ok) => match ok.text().await {
                    Ok(xml) => {
                        candidates.extend(
                            parse_rss_items(&xml)
                                .into_iter()
                                .filter(|item| candidate_matches_goal(item, goal)),
                        );
                    }
                    Err(e) => errors.push(format!("{feed}: read RSS body failed: {e}")),
                },
                Err(e) => errors.push(format!("{feed}: RSS status error: {e}")),
            },
            Err(e) => errors.push(format!("{feed}: RSS request failed: {e}")),
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
    let max_queries = args.get("max_queries").and_then(Value::as_u64).unwrap_or(4).clamp(1, 8) as usize;
    let create_inbox = args.get("create_inbox").and_then(Value::as_bool).unwrap_or(true);
    let rss_feeds = rss_feeds_arg(&args);

    let mut source_errors = Vec::new();
    let mut seen_urls = HashSet::new();
    let mut items = Vec::new();

    let (rss_candidates, rss_errors) = fetch_rss_candidates(&rss_feeds, &goal).await;
    source_errors.extend(rss_errors);
    for candidate in rss_candidates {
        if seen_urls.insert(candidate.url.to_lowercase()) {
            items.push(item_from_candidate(candidate));
        }
        if items.len() >= max_results {
            break;
        }
    }

    for query in search_queries(&goal, max_queries) {
        if items.len() >= max_results {
            break;
        }
        match ddg_search(&query).await {
            Ok(results) => {
                for result in results {
                    if seen_urls.insert(result.url.to_lowercase()) {
                        items.push(item_from_search(result));
                    }
                    if items.len() >= max_results {
                        break;
                    }
                }
            }
            Err(e) => source_errors.push(format!("{query}: {e}")),
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
    let events = aggregate_events(&items);
    let entry = ResearchInboxEntry {
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
    };

    if create_inbox {
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

pub(crate) fn call_research_sentinel_review(args: Value) -> Result<Value, String> {
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
        Some(entry) => Ok(json!({ "updated": true, "entry": entry })),
        None => Ok(json!({ "updated": false, "id": id })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_crypto_trading_news_context() {
        let text = "SEC approves spot Bitcoin ETF while Binance faces liquidity risk";
        assert_eq!(classify_event(text), "regulation");
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
        assert_eq!(item.event_type, "exchange");
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
        let items = parse_rss_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Bitcoin ETF flows affect market volatility");
        assert_eq!(items[0].published_at.as_deref(), Some("Sun, 24 May 2026 00:00:00 +0000"));
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
        assert!(cfg.sources.iter().any(|source| source.url.contains("coindesk")));
    }
}
