#!/usr/bin/env python3
"""
workflow_define.py — Define 階段（規格撰寫）

Shift Left 原則：問題越早發現越便宜。
在動任何程式碼之前，先把規格寫清楚。
"""
import json, sys

def main():
    try:
        req = json.load(sys.stdin)
    except Exception:
        req = {}

    task = req.get("user_input", "（未提供任務描述）")

    print(f"""# Define — 規格撰寫

**任務**：{task}

---

## ① 目標（What & Why）
> 這個功能要解決什麼問題？為誰解決？

- [ ] 問題陳述：
- [ ] 目標用戶：
- [ ] 成功指標：

---

## ② 驗收條件（Acceptance Criteria）
> 完成的定義是什麼？測試人員如何驗證？

- [ ] AC-1：給定 ___ 當 ___ 則 ___
- [ ] AC-2：
- [ ] AC-3：

---

## ③ 範圍（Scope）

**包含** ✅
-

**不包含** ❌（Chesterton's Fence：先理解邊界再動手）
-

---

## ④ 技術考量
> Hyrum's Law：所有可觀察的行為都會被依賴，API 改動要謹慎。

- 影響的模組：
- 向下相容性：
- 效能要求：
- 安全考量：

---

## ⑤ 開放問題
> 尚未決定的事項，必須在 Plan 前解決。

- Q1：
- Q2：

---

**下一步** → 規格確認後執行 **Plan**（任務拆解）
""")

if __name__ == "__main__":
    main()
