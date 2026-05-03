//! Test YAML scaffolding (Issue #239).
//!
//! Generates a baseline AgoraMarket regression YAML for one of the four
//! roles (buyer / seller / delivery / admin) with the correct viewport,
//! init sequence, role-pinning goto, and a placeholder success_criteria.
//!
//! The output passes `lint::lint(&goal)` clean — i.e., it doesn't trip any
//! of the five trap-class warnings new tests routinely fall into.
//!
//! ## Why a scaffold instead of a template file
//!
//! Per-role differences (viewport size, mobile flag, base URL path) make a
//! single template error-prone — it's easy to copy a buyer template and
//! forget to remove `mobile: true` for an admin test.  A code-generated
//! scaffold guarantees the right values per role without runtime branching
//! in the YAML itself.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestRole {
    Buyer,
    Seller,
    Delivery,
    Admin,
}

impl TestRole {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "buyer"    => Some(Self::Buyer),
            "seller"   => Some(Self::Seller),
            "delivery" => Some(Self::Delivery),
            "admin"    => Some(Self::Admin),
            _          => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Buyer    => "buyer",
            Self::Seller   => "seller",
            Self::Delivery => "delivery",
            Self::Admin    => "admin",
        }
    }

    /// Whether this role uses the H5 mobile viewport.
    fn is_mobile(self) -> bool {
        !matches!(self, Self::Admin)
    }
}

/// Generate a baseline YAML body for the given role and test_id.
///
/// Output is plain UTF-8 with `\n` line endings; the caller is responsible
/// for writing it to disk (we want the function pure so it's trivial to
/// unit-test against `lint::lint()`).
pub fn scaffold_yaml(role: TestRole, test_id: &str, name: Option<&str>) -> String {
    let role_str = role.as_str();
    let display_name = name.unwrap_or("TODO: 描述測試目的");
    let mobile = role.is_mobile();

    let viewport_block = if mobile {
        // 390×844 mobile=true (KB: trap-agoramarket-buyer-h5-viewport)
        "\n# H5 mobile viewport — AgoraMarket 會員端 / 商家端 / 外送端是手機版 App\n\
         viewport:\n  width: 390\n  height: 844\n  scale: 2.0\n  mobile: true\n"
    } else {
        // Admin desktop default (1280×900 mobile=false)
        "\n# Admin 後台桌面版 viewport\n\
         viewport:\n  width: 1280\n  height: 900\n  scale: 1.0\n  mobile: false\n"
    };

    // Init sequence: wait→enable→wait→enable→wait avoids host-empty AX tree
    // (MEMORY.md design rule).  No back-to-back enable_a11y → won't trip
    // convergence_guard.
    //
    // Step 1 = goto with __test_role= → defends against session pollution
    // from the previous test.  Required for the clear_state_reauth lint to
    // stay clean, even though we don't use clear_state here.
    //
    // Step 7 = placeholder action — TODO marker so the user knows where to
    // fill in their test logic.  Step count = 9 keeps max_iterations=20
    // comfortably above the 1.0× floor.
    let goal = format!(
        "目標：{display_name}\n\n  \
        步驟（線性，最後一步無條件 done=true，不得提早）：\n  \
        1. goto target=\"https://redandan.github.io/?__test_role={role_str}\"（防前一測試 session 污染）\n  \
        2. wait 5000（等 auto-login + Flutter {role_str} 首頁載入）\n  \
        3. enable_a11y\n  \
        4. wait 2000\n  \
        5. enable_a11y（二次確保 AX tree 就緒）\n  \
        6. wait 1000\n  \
        7. screenshot_analyze \"截圖：TODO 描述要驗證的 UI 元素\"\n  \
        8. wait 1000\n  \
        9. done=true",
    );

    format!(
        "id: {test_id}\n\
         name: \"{display_name}\"\n\
         url: \"https://redandan.github.io/?__test_role={role_str}\"\n\
         \n\
         max_iterations: 20\n\
         timeout_secs: 300\n\
         max_retries: 1  # H5 Flutter 偶發 Chrome 初始化延遲，自動重試一次\n\
         {viewport_block}\n\
         locale: zh-TW\n\
         \n\
         goal: |\n  {goal}\n\
         \n\
         success_criteria:\n  \
         - \"成功進入 {role_str} 首頁（未停在登入頁）\"\n  \
         - \"看到 TODO: 描述應該看到的 UI 元素（必須是正向確認，不能只寫『無錯誤』）\"\n\
         \n\
         tags: [regression, {role_str}, agora]\n",
    )
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lint;
    use super::super::parser::TestGoal;

    fn parse(yaml: &str) -> TestGoal {
        serde_yaml::from_str::<TestGoal>(yaml)
            .unwrap_or_else(|e| panic!("scaffold YAML must parse: {e}\n---\n{yaml}"))
    }

    #[test]
    fn scaffold_buyer_passes_all_lints() {
        let yaml = scaffold_yaml(TestRole::Buyer, "agora_smoke_buyer_xxx", Some("Buyer smoke"));
        let goal = parse(&yaml);
        let issues = lint::lint(&goal);
        assert!(issues.is_empty(), "buyer scaffold must lint clean, got: {:?}", issues);
    }

    #[test]
    fn scaffold_seller_passes_all_lints() {
        let yaml = scaffold_yaml(TestRole::Seller, "agora_smoke_seller_xxx", Some("Seller smoke"));
        let goal = parse(&yaml);
        let issues = lint::lint(&goal);
        assert!(issues.is_empty(), "seller scaffold must lint clean, got: {:?}", issues);
    }

    #[test]
    fn scaffold_delivery_passes_all_lints() {
        let yaml = scaffold_yaml(TestRole::Delivery, "agora_smoke_delivery_xxx", Some("Delivery smoke"));
        let goal = parse(&yaml);
        let issues = lint::lint(&goal);
        assert!(issues.is_empty(), "delivery scaffold must lint clean, got: {:?}", issues);
    }

    #[test]
    fn scaffold_admin_uses_desktop_viewport() {
        let yaml = scaffold_yaml(TestRole::Admin, "agora_smoke_admin_xxx", Some("Admin smoke"));
        assert!(yaml.contains("width: 1280"));
        assert!(yaml.contains("height: 900"));
        assert!(yaml.contains("mobile: false"));
        let goal = parse(&yaml);
        let issues = lint::lint(&goal);
        assert!(issues.is_empty(), "admin scaffold must lint clean, got: {:?}", issues);
    }

    #[test]
    fn scaffold_buyer_uses_mobile_viewport() {
        let yaml = scaffold_yaml(TestRole::Buyer, "x", None);
        assert!(yaml.contains("width: 390"));
        assert!(yaml.contains("mobile: true"));
    }

    #[test]
    fn role_from_str_matches_lowercase_and_trim() {
        assert_eq!(TestRole::from_str("buyer"),    Some(TestRole::Buyer));
        assert_eq!(TestRole::from_str("Buyer"),    Some(TestRole::Buyer));
        assert_eq!(TestRole::from_str("  ADMIN "), Some(TestRole::Admin));
        assert_eq!(TestRole::from_str(""),         None);
        assert_eq!(TestRole::from_str("manager"),  None);
    }

    #[test]
    fn scaffold_includes_required_role_query_param() {
        for role in [TestRole::Buyer, TestRole::Seller, TestRole::Delivery, TestRole::Admin] {
            let yaml = scaffold_yaml(role, "x", None);
            assert!(
                yaml.contains(&format!("__test_role={}", role.as_str())),
                "{:?} scaffold missing __test_role query param", role
            );
        }
    }
}
