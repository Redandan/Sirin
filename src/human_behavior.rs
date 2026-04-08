//! Human-behavior simulation engine.
//!
//! Computes whether a reply should be sent immediately, delayed, or skipped
//! entirely based on:
//!  - Random delay range (min/max seconds)
//!  - Per-hour / per-day frequency caps
//!  - Work schedule (work days, work hours, break periods)
//!
//! Timezone handling uses a simple UTC offset (integer hours) to avoid
//! the chrono-tz dependency.

use chrono::{Datelike, Timelike};

use crate::agent_config::HumanBehaviorConfig;

/// Result of asking the behavior engine whether to reply now.
#[derive(Debug, Clone)]
pub struct DelayDecision {
    /// `false` = skip this reply entirely (outside work hours, freq cap hit, etc.).
    pub should_reply: bool,
    /// Seconds to wait before sending (0 when `should_reply == false`).
    pub delay_secs: u64,
    /// Human-readable reason for logging.
    pub reason: String,
}

impl DelayDecision {
    fn skip(reason: impl Into<String>) -> Self {
        Self { should_reply: false, delay_secs: 0, reason: reason.into() }
    }
    fn send_after(delay_secs: u64) -> Self {
        Self {
            should_reply: true,
            delay_secs,
            reason: if delay_secs == 0 {
                "即時回覆".to_string()
            } else {
                format!("延遲 {delay_secs}s 後回覆")
            },
        }
    }
}

/// Evaluate whether and when to send a reply.
///
/// `sent_count_hour` and `sent_count_day` are the number of messages already
/// sent in the current rolling hour/day window respectively.
pub fn compute_delay(
    cfg: &HumanBehaviorConfig,
    sent_count_hour: u32,
    sent_count_day: u32,
) -> DelayDecision {
    if !cfg.enabled {
        return DelayDecision::send_after(0);
    }

    // ── Frequency caps ────────────────────────────────────────────────────────
    if cfg.max_messages_per_hour > 0 && sent_count_hour >= cfg.max_messages_per_hour {
        return DelayDecision::skip(format!(
            "超過每小時限制 ({}/{})",
            sent_count_hour, cfg.max_messages_per_hour
        ));
    }
    if cfg.max_messages_per_day > 0 && sent_count_day >= cfg.max_messages_per_day {
        return DelayDecision::skip(format!(
            "超過每日限制 ({}/{})",
            sent_count_day, cfg.max_messages_per_day
        ));
    }

    // ── Work schedule ─────────────────────────────────────────────────────────
    if let Some(sched) = &cfg.work_schedule {
        let now_utc = chrono::Utc::now();
        // Apply UTC offset to get local time.
        let offset_dur = chrono::Duration::hours(sched.utc_offset_hours as i64);
        let local = now_utc + offset_dur;

        // Check work day (number_from_monday: Mon=1 … Sun=7).
        let weekday_num = local.weekday().number_from_monday() as u8;
        if !sched.work_days.contains(&weekday_num) {
            return DelayDecision::skip(format!(
                "非工作日（週{}）",
                weekday_label(weekday_num)
            ));
        }

        let current_hm = hhmm(local.hour(), local.minute());

        // Check work hours.
        let start = parse_hhmm(&sched.work_start).unwrap_or(0);
        let end = parse_hhmm(&sched.work_end).unwrap_or(2359);
        if current_hm < start || current_hm >= end {
            return DelayDecision::skip(format!(
                "非工作時間（{}，工作時段 {}–{}）",
                format_hhmm(current_hm),
                &sched.work_start,
                &sched.work_end
            ));
        }

        // Check breaks.
        for brk in &sched.breaks {
            let brk_start = parse_hhmm(&brk.start).unwrap_or(0);
            let brk_end = parse_hhmm(&brk.end).unwrap_or(0);
            if current_hm >= brk_start && current_hm < brk_end {
                return DelayDecision::skip(format!("休息時間：{}", brk.name));
            }
        }
    }

    // ── Random delay ──────────────────────────────────────────────────────────
    let min = cfg.min_reply_delay_secs;
    let max = cfg.max_reply_delay_secs.max(min);
    let delay = if min == max {
        min
    } else {
        // Simple LCG-style pseudo-random using current nanos as seed.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        min + (nanos as u64 % (max - min + 1))
    };

    DelayDecision::send_after(delay)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Parse "HH:MM" into a comparable integer HHMM (e.g. "09:30" → 930).
fn parse_hhmm(s: &str) -> Option<u32> {
    let mut parts = s.splitn(2, ':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    Some(h * 100 + m)
}

/// Format HHMM integer back to "HH:MM".
fn format_hhmm(v: u32) -> String {
    format!("{:02}:{:02}", v / 100, v % 100)
}

fn hhmm(h: u32, m: u32) -> u32 { h * 100 + m }

fn weekday_label(n: u8) -> &'static str {
    match n {
        1 => "一",
        2 => "二",
        3 => "三",
        4 => "四",
        5 => "五",
        6 => "六",
        7 => "日",
        _ => "?",
    }
}
