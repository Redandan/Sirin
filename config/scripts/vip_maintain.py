#!/usr/bin/env python3
"""
VIP 維護腳本 — Sirin 技能腳本示例。

協議：
  stdin  → JSON { skill_id, user_input, agent_id }
  stdout → 純文字或 JSON 結果
  exit 0 → 成功
  exit 1 → 錯誤（stderr 說明原因）

開發者可在此加入任何業務邏輯，完全不影響主架構。
"""
import json
import sys
from datetime import datetime, timezone


def main():
    try:
        req = json.load(sys.stdin)
    except Exception as e:
        print(f"[vip_maintain] 無法解析 stdin JSON: {e}", file=sys.stderr)
        sys.exit(1)

    user_input = req.get("user_input", "")
    agent_id = req.get("agent_id", "unknown")

    # ── 業務邏輯在此實作 ─────────────────────────────────────────────────────
    # 示例：此處可查詢 CRM、讀取 CSV、呼叫外部 API 等
    # 任何 Python 依賴都可以安裝在系統或 venv，不需要修改 Rust 代碼

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    result_lines = [
        f"📋 VIP 維護報告（{now}）",
        f"請求來源：{agent_id}",
        f"用戶詢問：{user_input}",
        "",
        "── 近期需跟進 VIP ──────────────────",
        "• 張三  ｜上次聯繫：3 天前  ｜建議：今日致電",
        "• 李四  ｜上次聯繫：7 天前  ｜建議：發送跟進訊息",
        "• 王五  ｜上次聯繫：1 天前  ｜狀態：正常",
        "",
        "── 本月統計 ────────────────────────",
        "已聯繫商戶：12 次  ｜成功跟進：8 次  ｜轉化率：66%",
    ]

    print("\n".join(result_lines))


if __name__ == "__main__":
    main()
