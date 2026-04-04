//! Language detection utilities — CJK detection, mixed-language heuristics,
//! and Chinese fallback reply generation.

pub fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        (ch >= '\u{4E00}' && ch <= '\u{9FFF}')
            || (ch >= '\u{3400}' && ch <= '\u{4DBF}')
            || (ch >= '\u{F900}' && ch <= '\u{FAFF}')
    })
}

pub fn is_mixed_language_reply(text: &str) -> bool {
    let mut cjk_count = 0usize;
    let mut latin_count = 0usize;

    for ch in text.chars() {
        if (ch >= '\u{4E00}' && ch <= '\u{9FFF}')
            || (ch >= '\u{3400}' && ch <= '\u{4DBF}')
            || (ch >= '\u{F900}' && ch <= '\u{FAFF}')
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
