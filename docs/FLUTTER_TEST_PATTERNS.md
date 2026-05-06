# Flutter / AgoraMarket — Test Authoring Patterns

Hard-won patterns from the AgoraMarket E2E corpus.  Each row represents
a specific failure mode that has bitten us at least once.  Updated when
a new YAML breaks for a reason worth memorising.

## Common operations

| 操作 | 正確方式 | 錯誤方式 |
|---|---|---|
| off-screen textbox 輸入 | `ax_find` → `ax_focus` → `flutter_type` | `ax_click`(不 scroll) |
| scroll 後找按鈕 | `scroll` → `wait 800` → **再 shadow_dump** | 用舊 shadow_dump 結果 |
| Flutter tab 切換(PageView) | `shadow_click role=tab` → `wait 2000` → `enable_a11y` → `wait 1000` | 切換後立即操作(PageView 動畫未完成) |
| Flutter tab 切換(auto_route) | `shadow_click role=tab` | `click_point`(CDP 座標) |
| 確認輸入值 | `ax_value backend_id=<id>` | `screenshot_analyze`(近似) |
| CJK/中文輸入 | `flutter_type text="你好"` — 自動路由到 JS ClipboardEvent paste | `shadow_type`(InsertText 不保證 Flutter 接收) |
| ExpansionTile 點擊 | `shadow_click role=group name_regex="..."` | `shadow_click role=button`(ExpansionTile AX role=group) |
| ExpansionTile 展開後操作 | 展開後加 `wait 1500` + `enable_a11y` + `wait 800` | 立即截圖(動畫 ~300ms 未完成) |

## headless_chrome 已知修復

- **TargetInfoChanged cascade crash** (PR #118):
  `transport/mod.rs break→continue`,
  git fork: `Redandan/rust-headless-chrome@sirin-transport-fix`
- **idle_browser_timeout** (PR #124):
  300s→1800s, 避免 browser event loop 超時
- **YAML config 自動同步** (PR #124):
  啟動時自動 diff+copy `./config/` → LOCALAPPDATA

## Related

- KB `sirin-trap-flutter-ax-role-button-not-group` — Flutter widget role 排查
- KB `sirin-process-debug-screenshot-self-verify` — 視覺驗證流程
- [`docs/AGORA_VIEWPORT.md`](AGORA_VIEWPORT.md) — viewport per role (Buyer / Seller / Delivery / Admin)
