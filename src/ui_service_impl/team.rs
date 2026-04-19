//! MultiAgentService impl — 直接呼叫 `multi_agent` 模組。

use crate::multi_agent::{self, queue::{self, TaskStatus}, SessionInfo};
use crate::ui_service::{TeamDashView, TeamMemberView, TeamTaskView, TokenUsageView};

// ── helpers ───────────────────────────────────────────────────────────────────

/// 解析小隊工作目錄：優先用 claude_session 偵測，fallback current_dir。
fn team_cwd() -> String {
    crate::claude_session::repo_path("sirin")
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        })
}

fn to_member_view(info: &SessionInfo) -> TeamMemberView {
    TeamMemberView {
        role:       info.role.clone(),
        session_id: info.session_id.clone(),
        turns:      info.turns,
        resume_cmd: info.resume_cmd.clone(),
    }
}

fn blank_member(role: &str) -> TeamMemberView {
    TeamMemberView {
        role:       role.to_string(),
        session_id: None,
        turns:      0,
        resume_cmd: "(未初始化)".to_string(),
    }
}

// ── public API (called by mod.rs impl block) ──────────────────────────────────

pub fn team_dashboard(_svc: &super::RealService) -> TeamDashView {
    let cwd = team_cwd();

    // 初始化（或復原）小隊；取得成員狀態後立即釋放鎖
    let (pm, engineer, tester) = {
        let guard = multi_agent::get_or_init(&cwd);
        match guard.as_ref() {
            Some(team) => {
                let s = team.status();
                (to_member_view(&s.pm), to_member_view(&s.engineer), to_member_view(&s.tester))
            }
            None => (blank_member("pm"), blank_member("engineer"), blank_member("tester")),
        }
    };

    // 統計佇列
    let all = queue::list_all();
    let queued  = all.iter().filter(|t| t.status == TaskStatus::Queued).count();
    let running = all.iter().filter(|t| t.status == TaskStatus::Running).count();
    let done    = all.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed  = all.iter().filter(|t| t.status == TaskStatus::Failed).count();

    TeamDashView {
        pm, engineer, tester,
        worker_running: multi_agent::worker::is_running(),
        queued, running, done, failed,
    }
}

pub fn team_queue(_svc: &super::RealService) -> Vec<TeamTaskView> {
    queue::list_all()
        .into_iter()
        .map(|t| TeamTaskView {
            id:          t.id,
            description: t.description,
            status:      t.status.to_string(),
            result:      t.result,
            created_at:  t.created_at,
            finished_at: t.finished_at,
        })
        .collect()
}

pub fn team_enqueue(_svc: &super::RealService, description: &str) -> String {
    queue::enqueue(description)
}

pub fn team_start_worker(_svc: &super::RealService) {
    let cwd = team_cwd();
    multi_agent::worker::spawn(&cwd);
}

pub fn team_clear_completed(_svc: &super::RealService) {
    queue::clear_completed();
}

pub fn team_reset_member(_svc: &super::RealService, role: &str) {
    let cwd = team_cwd();
    let mut guard = multi_agent::get_or_init(&cwd);
    if let Some(team) = guard.as_mut() {
        team.reset_role(role);
    }
}

pub fn team_token_usage(_svc: &super::RealService, window_secs: u64) -> TokenUsageView {
    let s = crate::multi_agent::usage::snapshot(window_secs);
    let w = window_secs.max(1) as f64;
    TokenUsageView {
        window_secs,
        api_calls:       s.api_calls,
        tokens_per_min:  s.tokens_per_min,
        input_per_min:   (s.input_tokens  as f64 * 60.0 / w) as u64,
        output_per_min:  (s.output_tokens as f64 * 60.0 / w) as u64,
        cache_r_per_min: (s.cache_read    as f64 * 60.0 / w) as u64,
        cache_w_per_min: (s.cache_write   as f64 * 60.0 / w) as u64,
        cost_per_hour:   s.cost_per_hour,
        cache_hit_pct:   s.cache_hit_pct,
    }
}
