//! Workflow lifecycle — state inspection, stage transitions, LLM generation.

use super::RealService;
use crate::ui_service::*;

pub(super) fn workflow_state(_svc: &RealService) -> Option<WorkflowView> {
    let state = crate::workflow::WorkflowState::load()?;
    let stages = crate::workflow::STAGES.iter().map(|s| StageView {
        id: s.id.to_string(), label: s.label.to_string(), desc: s.desc.to_string(),
        status: match state.stage_status(s.id) {
            crate::workflow::StageStatus::Done => StageStatusView::Done,
            crate::workflow::StageStatus::Current => StageStatusView::Current,
            crate::workflow::StageStatus::Pending => StageStatusView::Pending,
        },
    }).collect();
    let all_done = state.all_done();
    Some(WorkflowView {
        feature: state.feature, description: state.description, skill_id: state.skill_id,
        current_stage: state.current_stage, started_at: state.started_at, stages, all_done,
    })
}

pub(super) fn workflow_create(svc: &RealService, feature: &str, description: &str) {
    let skill_id = feature.to_lowercase().replace(' ', "_");
    let state = crate::workflow::WorkflowState::new(feature, description, &skill_id);
    state.save();
    svc.push_toast(ToastLevel::Success, format!("Workflow「{feature}」已建立"));
}

pub(super) fn workflow_advance(svc: &RealService) -> bool {
    if let Some(mut state) = crate::workflow::WorkflowState::load() {
        let advanced = state.advance();
        if advanced {
            if let Some(info) = state.current_stage_info() {
                svc.push_toast(ToastLevel::Info, format!("進入階段: {}", info.label));
            }
        }
        advanced
    } else {
        false
    }
}

pub(super) fn workflow_stage_prompt(_svc: &RealService) -> Option<String> {
    let state = crate::workflow::WorkflowState::load()?;
    let prompt = crate::workflow::stage_context(
        &state.current_stage,
        &state.skill_id,
        &state.feature,
        &state.description,
        &state.stage_outputs,
    );
    Some(prompt)
}

pub(super) fn workflow_reset(_svc: &RealService) {
    let _ = std::fs::remove_file(crate::platform::app_data_dir().join("workflow.json"));
}

pub(super) fn workflow_generate(svc: &RealService) -> Option<String> {
    let prompt = workflow_stage_prompt(svc)?;
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let result = std::thread::spawn(move || {
        handle.block_on(crate::llm::call_prompt(
            &crate::llm::shared_http(), &crate::llm::shared_llm(), &prompt,
        ))
    }).join().ok()?.ok()?;
    Some(result)
}

pub(super) fn workflow_save_output(_svc: &RealService, stage_id: &str, output: &str) {
    if let Some(mut state) = crate::workflow::WorkflowState::load() {
        state.stage_outputs.insert(stage_id.to_string(), output.to_string());
        state.save();
    }
}
