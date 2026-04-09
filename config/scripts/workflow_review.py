#!/usr/bin/env python3
"""
workflow_review.py — Review 階段（程式碼審查）

原則：Shift Left——問題在 Review 發現比上線後發現便宜 100 倍。
Chesterton's Fence：不理解的程式碼先問，不要直接改掉。
"""
import json, sys

def main():
    try:
        req = json.load(sys.stdin)
    except Exception:
        req = {}

    task = req.get("user_input", "（未提供任務描述）")

    print(f"""# Review — 程式碼審查

**審查目標**：{task}

---

## ① 安全性（Security）

- [ ] **輸入驗證**：所有外部輸入（用戶、API、腳本 stdin）都有驗證？
- [ ] **命令注入**：`Command::new()` 的參數是否可能被用戶控制？
- [ ] **路徑穿越**：腳本路徑是否限定在 `config/scripts/`？
- [ ] **敏感資料**：Log 中有無洩漏 token / 密碼 / 個資？
- [ ] **錯誤訊息**：錯誤訊息是否洩漏內部實作細節？
- [ ] **依賴安全**：新增的 crate 是否有已知漏洞？

```bash
cargo audit  # 檢查依賴漏洞（若已安裝）
```

---

## ② 可讀性（Readability）

- [ ] **命名清晰**：變數 / 函數名稱能自解釋，不需要注釋？
- [ ] **函數長度**：單一函數 ≤ 50 行？否則是否應該拆分？
- [ ] **注釋品質**：注釋解釋「為什麼」而非「做什麼」？
- [ ] **Chesterton's Fence**：刪掉或修改的程式碼，是否理解其原本用途？
- [ ] **死代碼**：有無 `#[allow(dead_code)]` 應該改為真正刪除？
- [ ] **TODO / FIXME**：遺留的 TODO 是否已處理或建立 issue？

---

## ③ 效能（Performance）

- [ ] **不必要的 clone()**：是否有多餘的記憶體複製？
- [ ] **Blocking in async**：async 函數中是否有 blocking call（`std::thread::sleep`, 同步 IO）？
- [ ] **迴圈複雜度**：N² 或更高複雜度的演算法是否可接受？
- [ ] **快取利用**：是否有重複計算可以快取？

---

## ④ Hyrum's Law 合規

> 「你的 API 所有可觀察的行為，不論是否文件化，都會被依賴。」

- [ ] **API 變更**：對外的函數簽名 / 返回格式是否改變？
- [ ] **行為變更**：現有行為（包括錯誤訊息格式）是否改變？
- [ ] **隱性依賴**：有無其他模組可能依賴這個改動影響的「副作用」？
- [ ] **版本通知**：如有 breaking change，是否需要通知下游？

---

## ⑤ 架構一致性

- [ ] 新代碼符合現有的分層架構（agents / adk / telegram / ui）？
- [ ] 沒有繞過既有的抽象層（例：直接從 UI 層呼叫資料庫）？
- [ ] 錯誤處理模式一致（`Result<T, String>` vs 自定義 Error type）？

---

## 審查結論

```
安全性：    ✅ 無問題 / ⚠️ 有疑慮 / ❌ 需修改
可讀性：    ✅ / ⚠️ / ❌
效能：      ✅ / ⚠️ / ❌ / N/A
Hyrum法：   ✅ / ⚠️ / ❌
架構：      ✅ / ⚠️ / ❌
```

**阻斷項**（必須修改才能 Ship）：
-

**建議項**（可在後續迭代改善）：
-

---

**下一步** → 阻斷項全部解決後進入 **Ship**（上線）
""")

if __name__ == "__main__":
    main()
