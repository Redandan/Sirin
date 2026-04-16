//! Locale strings for test runner prompts.
//!
//! The templates themselves are structural English (field names, instructions);
//! what varies by locale is:
//! 1. Language the LLM should use in `thought` / `reason` / `final_answer` fields
//! 2. Default criteria text when `success_criteria` is empty
//! 3. Triage category labels (untranslated — LLM reads English schema)
//!
//! Keeping variation minimal reduces drift.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    ZhTw,  // default — 繁體中文
    En,
    ZhCn,  // 简体中文
}

impl Locale {
    pub fn from_yaml(s: &str) -> Self {
        match s.to_lowercase().replace('_', "-").as_str() {
            "en" | "en-us" | "english" => Self::En,
            "zh-cn" | "zh-hans" | "zh" | "chinese-simplified" => Self::ZhCn,
            _ => Self::ZhTw,
        }
    }

    /// Short code for logging / telemetry.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ZhTw => "zh-TW",
            Self::En   => "en",
            Self::ZhCn => "zh-CN",
        }
    }

    /// The language to tell the LLM to respond in.
    pub fn reasoning_language(&self) -> &'static str {
        match self {
            Self::ZhTw => "繁體中文",
            Self::En   => "English",
            Self::ZhCn => "简体中文",
        }
    }

    /// Default criteria line when a test has empty `success_criteria`.
    pub fn default_criteria(&self) -> &'static str {
        match self {
            Self::ZhTw => "- 頁面正常載入，目標描述的動作成功執行",
            Self::En   => "- The page loads successfully and the described action completes",
            Self::ZhCn => "- 页面正常加载，目标描述的动作成功执行",
        }
    }

    /// Fallback success-criteria for `evaluate_success` when empty.
    pub fn evaluate_default_criteria(&self) -> &'static str {
        match self {
            Self::ZhTw => "- 目標描述的動作成功執行",
            Self::En   => "- The described action in the goal was completed successfully",
            Self::ZhCn => "- 目标描述的动作成功执行",
        }
    }

    /// Header text for the evaluate_success prompt.
    pub fn evaluate_prompt_header(&self) -> &'static str {
        match self {
            Self::ZhTw => "判斷這個瀏覽器測試是否通過。",
            Self::En   => "Judge whether this browser test passed.",
            Self::ZhCn => "判断这个浏览器测试是否通过。",
        }
    }

    /// Expected JSON shape hint for evaluate_success.
    pub fn evaluate_judgment_hint(&self) -> &'static str {
        match self {
            Self::ZhTw => "根據 criteria 嚴格判斷，回傳 JSON:",
            Self::En   => "Judge strictly against the criteria and reply with JSON:",
            Self::ZhCn => "根据 criteria 严格判断，返回 JSON:",
        }
    }

    pub fn evaluate_reason_hint(&self) -> &'static str {
        match self {
            Self::ZhTw => "1-3 句解釋",
            Self::En   => "1-3 sentence explanation",
            Self::ZhCn => "1-3 句解释",
        }
    }

    pub fn triage_prompt_header(&self) -> &'static str {
        match self {
            Self::ZhTw => "分析下面瀏覽器測試失敗屬於哪一類，輸出 JSON。",
            Self::En   => "Classify the browser test failure below. Output JSON.",
            Self::ZhCn => "分析下面浏览器测试失败属于哪一类，输出 JSON。",
        }
    }

    /// Category definitions (used inside triage prompt). Values are stable
    /// English keys; what changes is the explanation.
    pub fn triage_categories_doc(&self) -> &'static str {
        match self {
            Self::ZhTw => "\
- ui_bug:   前端 UI 錯誤 (元素渲染錯、按鈕無反應、頁面空白、JS error)
- api_bug:  後端 API 錯誤 (network log 顯示 4xx/5xx、response body 錯誤)
- flaky:   偶發、時序、非確定性 (但歷史上不常失敗)
- env:     瀏覽器崩潰、網路 timeout、DNS 失敗等基礎設施
- obsolete: Selector 找不到元素、UI 改版，測試本身需要更新",
            Self::En => "\
- ui_bug:   Frontend UI error (element not rendering, unresponsive button, blank page, JS error)
- api_bug:  Backend API error (network log shows 4xx/5xx, bad response body)
- flaky:   Intermittent, timing-related (not historically frequent)
- env:     Browser crash, network timeout, DNS failure, infrastructure
- obsolete: Selector not found, UI changed — the test itself needs update",
            Self::ZhCn => "\
- ui_bug:   前端 UI 错误 (元素未渲染、按钮无反应、页面空白、JS error)
- api_bug:  后端 API 错误 (network log 显示 4xx/5xx、response body 错误)
- flaky:   偶发、时序、非确定性 (但历史上不常失败)
- env:     浏览器崩溃、网络 timeout、DNS 失败等基础设施
- obsolete: Selector 找不到元素、UI 改版，测试本身需要更新",
        }
    }

    pub fn triage_reason_hint(&self) -> &'static str {
        match self {
            Self::ZhTw => "1-2 句解釋",
            Self::En   => "1-2 sentence explanation",
            Self::ZhCn => "1-2 句解释",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_yaml_maps_common_aliases() {
        assert_eq!(Locale::from_yaml("en"), Locale::En);
        assert_eq!(Locale::from_yaml("EN-US"), Locale::En);
        assert_eq!(Locale::from_yaml("English"), Locale::En);
        assert_eq!(Locale::from_yaml("zh-CN"), Locale::ZhCn);
        assert_eq!(Locale::from_yaml("zh_cn"), Locale::ZhCn);
        assert_eq!(Locale::from_yaml("zh-hans"), Locale::ZhCn);
        assert_eq!(Locale::from_yaml("zh-TW"), Locale::ZhTw);
        assert_eq!(Locale::from_yaml(""), Locale::ZhTw);  // default
        assert_eq!(Locale::from_yaml("xx"), Locale::ZhTw);  // unknown → default
    }

    #[test]
    fn code_round_trip() {
        for &l in &[Locale::ZhTw, Locale::En, Locale::ZhCn] {
            assert_eq!(Locale::from_yaml(l.code()), l);
        }
    }

    #[test]
    fn en_templates_are_english() {
        assert!(Locale::En.reasoning_language().contains("English"));
        assert!(Locale::En.default_criteria().contains("page loads"));
        assert!(Locale::En.triage_prompt_header().contains("Classify"));
    }

    #[test]
    fn zh_tw_templates_remain_chinese() {
        assert!(Locale::ZhTw.reasoning_language().contains("繁體"));
    }
}
