#!/usr/bin/env python3
"""
workflow_build.py — Build 階段（TDD 增量實作）

原則：測試先行、小步前進、每步可驗證。
絕不一次寫完所有程式碼——紅燈 → 綠燈 → 重構。
"""
import json, sys

def main():
    try:
        req = json.load(sys.stdin)
    except Exception:
        req = {}

    task = req.get("user_input", "（未提供任務描述）")

    print(f"""# Build — TDD 增量實作

**任務**：{task}

---

## TDD 循環（每個功能點都要走完）

```
🔴 Red   → 先寫一個失敗的測試
🟢 Green → 寫最少的程式碼讓測試通過
🔵 Blue  → 重構（保持測試綠燈）
```

---

## 實作清單

### 當前任務
- [ ] **Step 1：寫測試（先不寫實作）**
  ```
  // 範例：
  #[test]
  fn test_xxx_given_yyy_should_zzz() {{
      // Arrange
      // Act
      // Assert
  }}
  ```
  - 確認：`cargo test` → 🔴 失敗（預期行為）

- [ ] **Step 2：最小實作（只讓測試通過）**
  - 不要過度設計
  - 不要實作測試沒要求的東西
  - 確認：`cargo test` → 🟢 全綠

- [ ] **Step 3：重構（改善設計，保持綠燈）**
  - 消除重複
  - 改善命名
  - 提取函數 / 模組
  - 確認：`cargo test` → 🟢 仍然全綠

---

## 邊界條件檢查（每個功能都要確認）

- [ ] 空輸入 / null / 空集合？
- [ ] 最大值 / 最小值？
- [ ] 並發安全？（若涉及 async / shared state）
- [ ] 錯誤路徑是否有清晰的 error message？

---

## 整合要點

- [ ] 是否破壞現有 API？（Hyrum's Law：所有行為都可能被依賴）
- [ ] 是否有向下相容問題？
- [ ] 是否需要 migration？

---

## 禁止事項

- ❌ 跳過測試直接實作
- ❌ 一次實作超過一個功能點
- ❌ 「等之後再補測試」
- ❌ 注釋掉失敗的測試

---

**下一步** → 所有測試綠燈後進入 **Verify**（系統化驗證）
""")

if __name__ == "__main__":
    main()
