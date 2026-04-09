#!/usr/bin/env python3
"""
workflow_verify.py — Verify 階段（系統化驗證）

原則：不跳過任何驗證步驟。
每個 AC 都必須有對應的驗證結果。
"""
import json, sys

def main():
    try:
        req = json.load(sys.stdin)
    except Exception:
        req = {}

    task = req.get("user_input", "（未提供任務描述）")

    print(f"""# Verify — 系統化驗證

**功能**：{task}

---

## ① 單元測試

- [ ] `cargo test` 全部通過（0 failures, 0 errors）
- [ ] 新增測試的覆蓋率達標
- [ ] 沒有 `#[ignore]` 的測試被跳過（除非有明確理由）

```bash
cargo test 2>&1 | tail -5
# 期望：test result: ok. X passed; 0 failed
```

---

## ② 驗收條件逐一確認

| AC | 描述 | 結果 |
|----|------|------|
| AC-1 | 給定 ___ 當 ___ 則 ___ | ⬜ |
| AC-2 | | ⬜ |
| AC-3 | | ⬜ |

> 每個 AC 都必須有實際測試結果（不能只是「看起來沒問題」）

---

## ③ 邊界條件測試

- [ ] 空輸入：行為符合預期？
- [ ] 最大負載：是否有效能問題？
- [ ] 錯誤路徑：錯誤訊息清晰？不洩漏內部細節？
- [ ] 並發場景：資料競爭？死鎖？

---

## ④ 回歸測試

- [ ] 現有功能未被破壞（跑完整測試套件）
- [ ] 相鄰模組行為未改變
- [ ] API / 介面向下相容（Hyrum's Law 合規）

```bash
cargo test --all 2>&1 | grep -E "FAILED|error"
# 期望：無輸出
```

---

## ⑤ 手動驗證（如適用）

- [ ] UI 行為符合設計稿
- [ ] Telegram 指令回應正確
- [ ] 日誌輸出正常（`sirin_log!` 有記錄）
- [ ] 錯誤情境下 UI 不崩潰

---

## ⑥ 效能基準（如涉及效能敏感路徑）

- [ ] 回應時間 ≤ 預期閾值
- [ ] 記憶體用量無異常增長
- [ ] 無不必要的 blocking call

---

## 驗證結果摘要

```
單元測試：  ✅ / ❌
AC 全通過：  ✅ / ❌
回歸測試：  ✅ / ❌
手動驗證：  ✅ / ❌
效能：      ✅ / ❌ / N/A
```

---

**下一步** → 全部綠燈後進入 **Review**（程式碼審查）
""")

if __name__ == "__main__":
    main()
