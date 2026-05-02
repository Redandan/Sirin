# Sirin Web UI — Design Notes

設計參考的競品 + 套用對應到 Sirin 哪一塊。給未來 AI session 看：
新增 panel / 重排 layout 前先讀這份，避免發明輪子。

## 美學基調

**Linear** 是主要的視覺語言來源：
- Dark background (`#0E1116` 級別，我們用 `#1A1A1A`)
- Sidebar **永遠可見** + 有 label（不只 icon）
- 等寬數字（IDs / counts / timestamps）
- ⌘K 是主要操作，比點擊優先
- Status pill 用「灰底 + 彩色 glyph + 大寫 mono」
- Hover 影響邊框 / 背景，不影響大小（無 transform）

**Vercel Dashboard** 補充：
- Section 之間 32px 留白（比 16px 更舒展）
- 監控數字用 `tabular-nums` 避免抖動
- Subtle border `rgba(255,255,255,0.06)` 比硬邊好看

## 各 view 的競品對照

### Dashboard
| Section | 主要參考 | 套用 |
|---------|---------|------|
| Top bar | Linear (workspace switcher / ⌘K hint) | 已有 |
| Sidebar | Linear (labels visible by default) | **TODO: labels** |
| Active runs | Playwright Test UI (live trace + screenshot) | **TODO: 嵌截圖 + pulse** |
| Recent runs | GitHub Actions runs page (icon · name · duration · time ago) | **TODO: 改 row layout** |
| Coverage card | Codecov + 自有 funnel | 已有 |
| Browser card | Playwright trace viewer (screenshot 縮圖 + url) | **TODO: 嵌縮圖** |

### Testing — full page
| Section | 主要參考 |
|---------|---------|
| Run list | Playwright Test UI 左 pane（tree → file → test）|
| Run detail (selected) | Playwright Test UI 中 pane（timeline + 步驟）|
| Browser | Playwright trace viewer 右下 pane（DOM snapshot）|

### Coverage — full page
| Section | 主要參考 |
|---------|---------|
| 3-tier funnel | 自有設計（沒競品直接對應） |
| Per-group breakdown | Codecov hierarchy（dir → file → coverage %）|
| Discovery gaps | Sentry "missing instrumentation" 警告卡 |

### Workspace（per-agent detail）
| Section | 主要參考 |
|---------|---------|
| 對話 tab | Slack thread / Linear comment thread |
| 概覽 tab | Linear issue side panel |
| 待確認 tab | GitHub PR review queue |

### Modals
- **Settings**: macOS System Settings 風格 — sidebar 分類 + 右側內容
- **Logs**: Datadog log explorer — virtualized list + search bar
- **Dev Squad**: Linear 的 team page

## 配色語意（與其他工具對齊）

| 色值 | 語意 | 用在哪 | 競品對照 |
|------|------|--------|---------|
| `#00FFA3` ACCENT | running / passed / 主要 CTA | active dots, primary buttons | Vercel green |
| `#FF4B4B` DANGER | failed / stopped | error badges, destructive actions | Sentry red |
| `#FFD93D` YELLOW | partial / timeout / warning | flaky badge, warnings | GitHub Actions amber |
| `#4DA6FF` INFO | scripted / link / neutral status | links, scripted indicator | Linear blue |
| `#808080` TEXT_DIM | secondary text | timestamps, captions | Linear gray-2 |

**禁忌**：
- 不混用 hue（不要為了好看再加紫 / 橘）
- 紅色只用在 destructive，不用在 highlight
- 黃色不要 < 16px（小字會糊）

## 字型階層（更嚴格）

```
24px → 主畫面唯一一個 H1（DASHBOARD / TESTING / WORKSPACE）
15px → section heading（少用，多用 11.5 mono）
13px → body / button text
11.5px MONO 大寫 → section label / ALL CAPS / table header
10px → caption / timestamp / 輔助文字
```

數字一律 mono + tabular-nums。

## 互動原則

**Hover**：
- Card → 背景 `--card` → `--hover`
- Button → 邊框出現
- 不要動 transform / scale（避免 layout shift）

**Active / Pressed**：
- 左邊出現 3px accent bar（Linear 慣例）

**Loading**：
- Spinner 只在「會超過 200ms」的操作出現
- 短 fetch 用 skeleton 替代 spinner

**動畫**：
- transition `100ms` 給 hover, `200ms` 給 modal/palette
- easing 用 `ease-out`（fade in）跟 `ease-in`（fade out）

## 競品「不要學」的東西

- Linear 的彩色 highlight bar（太花，跟 Sirin hardcore 不合）
- Vercel 的漸層背景（不要 bg-gradient）
- GitHub Actions 的 emoji 開頭（用 monochrome glyph 或 Lucide 線稿圖示）
- Datadog 的密集表格（Sirin 偏向卡片式留白）

## 圖示來源

未來想加圖示用 **Lucide** (https://lucide.dev) — line-art, 1.5px stroke,
MIT, 直接複製 SVG。命名遵循 Lucide 名（`play`, `circle-check`,
`alert-triangle`, `terminal`...）。

## 文件版本控管

每次重大 redesign 都在這 file 寫 changelog：

- 2026-05-02: Phase 1 — initial port from egui，照搬視覺
- (next) Phase 2: 套用 Linear sidebar labels + Playwright active run card +
  GitHub Actions recent runs row
