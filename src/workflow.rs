//! Development workflow tracker — Define → Plan → Build → Verify → Review → Ship.
//!
//! State is persisted to `data/workflow.json` so progress survives restarts.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Stage registry ────────────────────────────────────────────────────────────

pub struct StageInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub script: &'static str,
}

pub const STAGES: &[StageInfo] = &[
    StageInfo { id: "define", label: "Define", desc: "規格撰寫",  script: "config/scripts/workflow_define.py" },
    StageInfo { id: "plan",   label: "Plan",   desc: "任務拆解",  script: "config/scripts/workflow_plan.py"   },
    StageInfo { id: "build",  label: "Build",  desc: "TDD 實作",  script: "config/scripts/workflow_build.py"  },
    StageInfo { id: "verify", label: "Verify", desc: "系統驗證",  script: "config/scripts/workflow_verify.py" },
    StageInfo { id: "review", label: "Review", desc: "程式碼審查", script: "config/scripts/workflow_review.py" },
    StageInfo { id: "ship",   label: "Ship",   desc: "上線發布",  script: "config/scripts/workflow_ship.py"   },
];

pub fn stage_by_id(id: &str) -> Option<&'static StageInfo> {
    STAGES.iter().find(|s| s.id == id)
}

// ── Persisted state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Short display name of the skill being built.
    pub feature: String,
    /// Longer user-written description of what the skill does.
    #[serde(default)]
    pub description: String,
    /// The skill_id this workflow is building (e.g. "vip_maintain").
    #[serde(default)]
    pub skill_id: String,
    pub current_stage: String,
    pub completed: Vec<String>,
    pub started_at: String,
    /// Accepted AI output for each completed stage (used as context in later stages).
    #[serde(default)]
    pub stage_outputs: HashMap<String, String>,
}

pub enum StageStatus {
    Done,
    Current,
    Pending,
}

const STATE_PATH: &str = "data/workflow.json";

impl WorkflowState {
    pub fn new(
        feature: impl Into<String>,
        description: impl Into<String>,
        skill_id: impl Into<String>,
    ) -> Self {
        Self {
            feature: feature.into(),
            description: description.into(),
            skill_id: skill_id.into(),
            current_stage: "define".to_string(),
            completed: Vec::new(),
            started_at: chrono::Local::now().format("%Y-%m-%d").to_string(),
            stage_outputs: HashMap::new(),
        }
    }

    pub fn load() -> Option<Self> {
        let content = std::fs::read_to_string(STATE_PATH).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(STATE_PATH, json);
        }
    }

    pub fn stage_status(&self, id: &str) -> StageStatus {
        if self.completed.iter().any(|c| c == id) {
            StageStatus::Done
        } else if self.current_stage == id {
            StageStatus::Current
        } else {
            StageStatus::Pending
        }
    }

    /// Mark current stage complete and advance to the next one.
    /// Returns `false` when already at the last stage.
    pub fn advance(&mut self) -> bool {
        if !self.completed.contains(&self.current_stage) {
            self.completed.push(self.current_stage.clone());
        }
        let idx = STAGES.iter().position(|s| s.id == self.current_stage).unwrap_or(0);
        if idx + 1 < STAGES.len() {
            self.current_stage = STAGES[idx + 1].id.to_string();
            self.save();
            true
        } else {
            self.save();
            false
        }
    }

    pub fn all_done(&self) -> bool {
        self.completed.len() >= STAGES.len()
    }

    pub fn current_stage_info(&self) -> Option<&'static StageInfo> {
        stage_by_id(&self.current_stage)
    }
}

// ── Script runner ─────────────────────────────────────────────────────────────

/// Build the LLM system prompt for the given workflow stage.
/// Includes previous stage outputs as context so the AI can refer back to prior work.
pub fn stage_context(
    stage_id: &str,
    skill_id: &str,
    feature: &str,
    description: &str,
    stage_outputs: &HashMap<String, String>,
) -> String {
    let desc_line = if description.trim().is_empty() {
        String::new()
    } else {
        format!("\n功能描述：{description}")
    };
    let mut parts: Vec<String> = vec![format!(
        "你是 Sirin 的 AI Skill 開發助手。\n\
         目標：開發 AI Skill `{skill_id}`。\n\
         功能名稱：{feature}{desc_line}"
    )];

    // Inject previous stage outputs as context
    let order = ["define", "plan", "build", "verify", "review"];
    let current_idx = order.iter().position(|&s| s == stage_id).unwrap_or(0);
    let mut ctx_parts: Vec<String> = Vec::new();
    for prev in &order[..current_idx] {
        if let Some(out) = stage_outputs.get(*prev) {
            if !out.trim().is_empty() {
                let lbl = stage_by_id(prev).map(|s| s.label).unwrap_or(prev);
                ctx_parts.push(format!("### {lbl} 階段成果\n{out}"));
            }
        }
    }
    if !ctx_parts.is_empty() {
        parts.push(format!("\n## 前置階段參考\n\n{}", ctx_parts.join("\n\n")));
    }

    let instr: String = match stage_id {
        "define" => "\n## 當前任務：Define — 理解確認\n\
             用戶描述了他想要的 AI Skill。\n\
             請用繁體中文、口語化的方式確認你的理解，格式如下：\n\n\
             **我的理解**\n\
             - 功能：<一句話說明>\n\
             - 觸發場景：<用戶什麼時候會說這個>\n\
             - 預期輸出：<腳本輸出什麼給用戶>\n\
             - 建議觸發詞：<3-5 個關鍵詞>\n\n\
             最後問一句：「以上理解正確嗎？有需要補充或調整的地方嗎？」"
            .to_string(),
        "plan" => format!(
            "\n## 當前任務：Plan（實作規劃）\n\
             根據 Define 規格，規劃 Python 腳本 `config/scripts/{skill_id}.py` 的實作：\n\
             - 腳本通過 stdin 接收 JSON：`{{\"skill_id\": ..., \"user_input\": ..., \"agent_id\": ...}}`\n\
             - 通過 stdout 輸出結果（純文字或 Markdown）\n\
             - 通過 stderr 輸出 `sirin_log:` 前綴的調試日誌\n\
             請列出：主要處理步驟、需要的資料來源、輸出格式範例。"
        ),
        "build" => format!(
            "\n## 當前任務：Build（撰寫腳本）\n\
             請撰寫完整的 Python 腳本 `config/scripts/{skill_id}.py`。\n\n\
             規範：\n\
             - 從 stdin 讀取 JSON：`import json, sys; data = json.load(sys.stdin)`\n\
             - 通過 stdout 輸出結果（純文字或 Markdown）\n\
             - 通過 stderr 輸出調試訊息（前綴 `sirin_log:`）\n\
             - 腳本應自包含，只用標準庫或常見套件\n\n\
             請用 ```python 代碼塊輸出完整腳本。"
        ),
        "verify" => format!(
            "\n## 當前任務：Verify（驗證）\n\
             腳本已寫入 `config/scripts/{skill_id}.py`。\n\
             點擊「執行腳本」按鈕實際運行，確認輸出正確。\n\
             如有問題請修改腳本後重新執行。"
        ),
        "review" => "\n## 當前任務：Review（程式碼審查）\n\
             請審查 Build 階段撰寫的 Python 腳本：\n\
             1. 程式碼品質（可讀性、錯誤處理）\n\
             2. 安全性（潛在風險）\n\
             3. 效能問題\n\
             4. 具體改進建議"
            .to_string(),
        "ship" => format!(
            "\n## 當前任務：Ship（發布配置）\n\
             請生成此 AI Skill 的 YAML 配置文件 `config/skills/{skill_id}.yaml`。\n\n\
             格式：\n\
             ```yaml\n\
             id: {skill_id}\n\
             name: <技能中文名稱>\n\
             description: <功能描述>\n\
             script_file: config/scripts/{skill_id}.py\n\
             example_prompts:\n\
               - <從 Define 階段複製觸發語句>\n\
             trigger_keywords:\n\
               - <關鍵詞>\n\
             ```\n\n\
             請用 ```yaml 代碼塊輸出完整配置。"
        ),
        _ => String::new(),
    };
    parts.push(instr);
    parts.join("")
}

/// Build the prompt for Define Phase 2: generate formal spec based on confirmed understanding.
pub fn define_spec_prompt(
    skill_id: &str,
    feature: &str,
    description: &str,
    understanding: &str,
    user_additions: &str,
) -> String {
    let desc_line = if description.trim().is_empty() {
        String::new()
    } else {
        format!("\n功能描述：{description}")
    };
    let additions = if user_additions.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n用戶補充說明：{user_additions}")
    };
    format!(
        "你是 Sirin 的 AI Skill 開發助手。\n\
         目標：開發 AI Skill `{skill_id}`。\n\
         功能名稱：{feature}{desc_line}\n\n\
         ## 已確認的理解\n{understanding}{additions}\n\n\
         ## 任務：生成正式規格\n\
         基於上述已確認的理解，請生成詳細的技能規格：\n\
         1. 功能說明（2-3 句話）\n\
         2. 觸發場景\n\
         3. 預期輸出格式\n\
         4. 5-10 個 example_prompts（YAML list 格式）\n\
         5. 建議 trigger_keywords（3-5 個詞）"
    )
}

/// Build the prompt to ask AI to generate a snake_case skill_id.
pub fn skill_id_gen_prompt(feature: &str, description: &str) -> String {
    format!(
        "根據以下技能名稱和描述，生成一個英文的 snake_case skill_id。\n\
         要求：只使用小寫字母、數字、底線，2-4 個英文詞，不要標點或空格。\n\
         名稱：{feature}\n\
         描述：{description}\n\n\
         只輸出 skill_id，不要任何其他文字或解釋。\n\
         範例格式：vip_maintain"
    )
}

/// Extract a fenced code block of the given language tag from text.
/// e.g. `extract_code_block(text, "python")` finds ` ```python ... ``` `.
pub fn extract_code_block(text: &str, lang: &str) -> Option<String> {
    let marker = format!("```{lang}");
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

// ── Script runner ─────────────────────────────────────────────────────────────

/// Run the stage's Python script in a blocking call.
/// Tries `python` (Windows) then `python3` (Unix/macOS).
/// stdin: JSON `{ skill_id, user_input }` → stdout: markdown template.
pub fn run_stage_script(stage: &StageInfo, feature: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let payload = serde_json::json!({
        "skill_id": format!("workflow_{}", stage.id),
        "user_input": feature,
        "agent_id": null,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;

    let try_run = |interp: &str| -> std::io::Result<std::process::Output> {
        let mut child = Command::new(interp)
            .arg(stage.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&bytes);
        }
        child.wait_with_output()
    };

    let output = try_run("python")
        .or_else(|_| try_run("python3"))
        .map_err(|e| format!("無法啟動 Python：{e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "腳本錯誤（exit {:?}）：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
