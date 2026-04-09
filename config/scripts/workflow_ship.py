#!/usr/bin/env python3
"""
workflow_ship.py — Ship 階段（上線清單）

原則：上線是一個儀式，不是一個意外。
所有清單項目都必須打勾，才能按下 merge / deploy。
"""
import json, sys
from datetime import datetime

def main():
    try:
        req = json.load(sys.stdin)
    except Exception:
        req = {}

    task = req.get("user_input", "（未提供功能描述）")
    today = datetime.now().strftime("%Y-%m-%d")

    print(f"""# Ship — 上線清單

**功能**：{task}
**日期**：{today}

---

## ① Git 準備

- [ ] **分支命名**：`feat/xxx` / `fix/xxx` / `refactor/xxx`
- [ ] **Commit 訊息格式**：
  ```
  <type>(<scope>): <subject>

  <body — 解釋為什麼，不是做什麼>

  Closes #<issue>
  ```
  type：`feat` / `fix` / `refactor` / `docs` / `test` / `chore`

- [ ] **Commit 原子性**：每個 commit 只做一件事
- [ ] **無調試代碼**：`println!` / `dbg!` / `todo!()` 已移除
- [ ] **Diff 自我審查**：`git diff main` 確認無意外修改

```bash
git log --oneline main..HEAD  # 確認 commit 清單
git diff main --stat          # 確認修改範圍
```

---

## ② CI / 本地驗證

- [ ] `cargo check` — 無警告（或警告已知且可接受）
- [ ] `cargo test` — 全部通過
- [ ] `cargo clippy` — 無新增警告（可選）
- [ ] `cargo build --release` — release build 成功

```bash
cargo test && cargo build --release && echo "✅ Ship ready"
```

---

## ③ 安全掃描

- [ ] 無硬編碼憑證 / token / 密碼
- [ ] `config/` 目錄中的敏感文件未進入版本控制（.gitignore 確認）
- [ ] 新增的腳本路徑在 `config/scripts/`（白名單範圍內）

```bash
git diff main -- "*.yaml" "*.json" | grep -i "token\|password\|secret"
# 期望：無輸出
```

---

## ④ 架構決策記錄（ADR）

> 如果這個改動涉及重要的技術決策，記錄下來：

**決策**：（如適用）
**背景**：為什麼需要這個改動？
**選項**：考慮過哪些方案？
**結論**：選擇這個方案的理由？
**代價**：這個選擇的取捨是什麼？

- [ ] ADR 已記錄（或確認此改動不需要 ADR）

---

## ⑤ 上線後確認

- [ ] **功能驗證**：在 production 環境確認功能正常
- [ ] **日誌監控**：`sirin_log` 無異常錯誤
- [ ] **回滾計劃**：如果出問題，如何快速回滾？
  ```
  git revert HEAD  # 或 git checkout main
  ```

---

## 上線確認簽核

```
Git 準備：    ✅ / ❌
CI 驗證：     ✅ / ❌
安全掃描：    ✅ / ❌
ADR：         ✅ / N/A
```

**⚠️ requires_approval = true**
> 所有項目打勾後，人工確認方可 merge。

---

**完成！** 本次迭代循環：Define → Plan → Build → Verify → Review → **Ship** ✅
下一個功能 → 重新開始 **Define**
""")

if __name__ == "__main__":
    main()
