//! Language detection utilities — CJK detection, mixed-language heuristics,
//! and Chinese fallback reply generation.

pub fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{F900}'..='\u{FAFF}').contains(&ch)
    })
}

#[allow(dead_code)]
pub fn is_mixed_language_reply(text: &str) -> bool {
    let mut cjk_count = 0usize;
    let mut latin_count = 0usize;

    for ch in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{F900}'..='\u{FAFF}').contains(&ch)
        {
            cjk_count += 1;
        } else if ch.is_ascii_alphabetic() {
            latin_count += 1;
        }
    }

    if cjk_count == 0 || latin_count == 0 {
        return false;
    }

    let total = cjk_count + latin_count;
    let latin_ratio = latin_count as f32 / total as f32;

    // Treat as mixed when there are enough Latin letters to impact readability.
    latin_count >= 8 && latin_ratio > 0.35
}

pub fn is_direct_answer_request(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    normalized.contains("直接跟我說")
        || normalized.contains("直接說")
        || normalized.contains("直接講")
        || normalized.contains("不要貼連結")
        || normalized.contains("別貼連結")
        || normalized.contains("不用連結")
        || normalized.contains("just tell me")
        || normalized.contains("no links")
}

pub fn is_identity_question(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    normalized.contains("你是誰")
        || normalized.contains("你是谁")
        || normalized.contains("你叫什麼")
        || normalized.contains("你叫什么")
        || normalized.contains("你的身份")
        || normalized.contains("who are you")
        || normalized.contains("what are you")
}

pub fn is_code_access_question(text: &str) -> bool {
    let normalized = text.trim().to_lowercase();
    let asks_about_seeing = normalized.contains("你能看到")
        || normalized.contains("你可以看到")
        || normalized.contains("能看到")
        || normalized.contains("可以看到")
        || normalized.contains("看得到")
        || normalized.contains("看不到")
        || normalized.contains("能看")
        || normalized.contains("能讀")
        || normalized.contains("能不能看")
        || normalized.contains("看到什麼")
        || normalized.contains("看得到什麼")
        || normalized.contains("can you see")
        || normalized.contains("can you read")
        || normalized.contains("do you see");
    let mentions_code = normalized.contains("程式碼")
        || normalized.contains("代码")
        || normalized.contains("代碼")
        || normalized.contains("當前代碼")
        || normalized.contains("目前代碼")
        || normalized.contains("檔案")
        || normalized.contains("文件")
        || normalized.contains("源码")
        || normalized.contains("源碼")
        || normalized.contains("source code")
        || normalized.contains("the code")
        || normalized.contains("codebase")
        || normalized.contains("專案")
        || normalized.contains("项目")
        || normalized.contains("項目")
        || normalized.contains("project files")
        || normalized.contains("this project");

    (asks_about_seeing && mentions_code)
        || normalized.contains("看程式碼")
        || normalized.contains("看代碼")
        || normalized.contains("讀程式碼")
        || normalized.contains("讀代碼")
        || normalized.contains("看不到代碼")
        || normalized.contains("證明你看得到")
        || normalized.contains("這是啥項目")
        || normalized.contains("這是什麼項目")
        || normalized.contains("這個專案是什麼")
}

pub fn chinese_fallback_reply(user_text: &str, execution_result: Option<&str>) -> String {
    let mut base = if user_text.trim().len() <= 12 {
        "收到，我在這裡。你想先從哪一點開始？".to_string()
    } else {
        "收到，我理解你的需求了；我先幫你整理重點，接著給你可執行的下一步。".to_string()
    };

    if let Some(result) = execution_result {
        base.push_str(&format!("\n{result}"));
    }

    base
}

#[cfg(test)]
mod tests {
    use super::{is_code_access_question, is_identity_question};

    #[test]
    fn detects_identity_questions() {
        assert!(is_identity_question("你是誰"));
        assert!(is_identity_question("Who are you?"));
        assert!(!is_identity_question("你可以幫我研究 Rust 嗎"));
    }

    #[test]
    fn detects_code_access_questions() {
        assert!(is_code_access_question("你能看到程序運行的代碼嗎"));
        assert!(is_code_access_question("能看到當前代碼嗎"));
        assert!(is_code_access_question("為啥看不到代碼"));
        assert!(is_code_access_question("你現在能看到什麼檔案"));
        assert!(is_code_access_question("這是啥項目"));
        assert!(is_code_access_question(
            "can you see the code for this app?"
        ));
        assert!(!is_code_access_question("幫我看看這段市場分析"));
    }
}
