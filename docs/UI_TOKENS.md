# Sirin Web UI — Design Tokens & Component Rules

Strict ruleset for `web/index.html` + `web/style.css` + `web/app.js`.
Source-of-truth for competitor inspiration / colour semantics / "do not
adopt" anti-patterns is [`web/DESIGN.md`](../web/DESIGN.md) — read it
**before** designing any new panel.

## 視覺語調

- 風格:極簡、硬核、高性能感(Linear / Vercel dashboard 風)
- 配色固定,意義穩定(accent=running, danger=failed, info=link/scripted)

## 配色方案 (CSS variables in `web/style.css`)

```
--bg:        #1A1A1A
--card:      #222222
--hover:     #2A2A2A
--border:    #333333
--text:      #E0E0E0
--text-dim:  #808080
--accent:    #00FFA3   (running / passed / 主要 CTA)
--danger:    #FF4B4B   (failed / stopped)
--yellow:    #FFD93D   (partial / timeout / warning)
--info:      #4DA6FF   (scripted / link / neutral status)
--value:     #FFFFFF   (numbers, monospace)
```

## 字型階層

```
24px → H1 (DASHBOARD / TESTING / WORKSPACE)
15px → modal section title
13px → body / button text
11.5px MONO 大寫 → section label
10px  → caption / timestamp
```

數字一律 `var(--font-mono)` + `font-variant-numeric: tabular-nums`.

## 組件規範

- **Card**: `background: var(--card); border-radius: 4px; padding: 12px`
- **Status dot**: 7-8px `<span class="dot dot-ok|dot-bad">`
- **Pulse dot**: live indicator with CSS keyframe animation
- **Button hover**: bg → `var(--hover)`, do NOT animate transform/scale
- **Active row**: 2px `var(--accent)` left bar (Linear convention)
- **Section label**: `text-transform: uppercase`, mono, 0.04em letter-spacing

## AI 新頁面流程

1. **Read [`web/DESIGN.md`](../web/DESIGN.md)** — pick competitor reference for the new view
2. **Add HTML to `web/index.html`** — single-file UI, all views in there
3. **Add state to `web/app.js`** — extend `sirin()` factory
4. **Add CSS classes to `web/style.css`** — reuse existing tokens
5. **Verify in browser** — no rebuild needed for UI-only changes;
   F5 refresh is the entire iteration loop. Backend changes still
   need `cargo build --release` + restart.

## i18n

User-facing UI is Traditional Chinese (per user preference).  Use
direct translation — no i18n framework.  Examples: `通過` / `失敗` /
`執行中` / `通過率` / `今日執行`.
