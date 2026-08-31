# AgoraMarket Physical iPhone Acceptance

## Scope

- Production site: `https://agoramarket.purrtechllc.com/`
- Safari route: `safari_store`
- Telegram route: `telegram_bot`, using the actual `AgoraLoginBot` storefront
  entry rather than a generic embedded browser.
- Browse only. Do not log in, send a bot message, add to cart, create an order,
  or pay.

All phone observation and actions go through the Sirin iPhone MCP tools. Do not
call Sirin's internal Driver provider directly.

## Preconditions

For a fixed Safari or Telegram entry, use one `ios_acceptance_run` with a
stable `run_id`; it owns the fresh safety checks and lease release. Call
`ios_device_status` separately only to diagnose a failure or before a later
multi-action flow, and preserve the four independent evidence results:

1. `DEVICE_DETECTED`
2. `INFO_READABLE`
3. `SCREEN_CAPTURE`
4. `SCREEN_CONTROL`

Ask the user to unlock, trust the computer, approve Developer Mode, or accept a
Telegram launch prompt on the phone whenever Sirin returns a human-only blocker.

Acquire one manual `acceptance_browse_only` control lease only when subsequent
scrolling or another multi-action flow is needed. Use stable `action_id` values
and always release that manual lease.

## Safari direct acceptance

1. Call `ios_acceptance_run(route="safari_store")` and require a successful
   lease release.
2. Use its fresh evidence to inspect the top of the storefront.
3. Confirm the product region is populated and top and bottom components exist.
4. Check horizontal offset, clipping, or content outside the viewport.
5. Call `ios_swipe` for one vertical scroll and inspect its fresh before/after
   evidence.

Flutter or CanvasKit pages can ignore synthetic WDA gestures even when native
iOS controls respond. If Sirin cannot prove movement, ask the user for one
physical finger swipe and compare two new Sirin screenshots. An automated
gesture with no movement is not enough by itself to classify a page defect.

If a confirmed human swipe produces no content change while the same gesture
scrolls the Telegram Mini App, classify Safari scrolling as `DEFECT`.

## Telegram Mini App acceptance

1. Call `ios_acceptance_run(route="telegram_bot")` and require a successful
   lease release.
2. The fixed route may tap a chat-list `OPEN` only when the live Telegram
   accessibility tree contains exactly one `AgoraLoginBot` and one `OPEN`
   control in the same visible row. Inside that bot chat it may instead tap the
   exact branded `進入 AgoraMarket 商城` control. One route action may tap at
   most one entry control; it must never use a stale screenshot coordinate or
   a generic tap.
3. If Sirin returns `NEEDS_HUMAN_BOT_START`, an ambiguous-entry state, or
   Telegram shows a launch confirmation, stop for the user. Do not send
   `/start`, choose among ambiguous controls, or approve on their behalf.
   Bounded `diagnostic_rows` in an ambiguous or missing-entry result are
   untrusted accessibility evidence only; they do not authorize a generic tap
   or a stale coordinate retry.
   When Telegram omits separate accessibility nodes for its inline keyboard,
   the fixed route may use Sirin's local Windows OCR only if two fresh frames
   each contain one aligned three-row set of `進入 AgoraMarket 商城`,
   `聯絡 @sirin555`, and `月舞老虎機` under the exact bot-chat header. The second
   frame is the tap source; any duplicate, missing, moved, or misaligned label
   must fail closed. Telegram's observed WDA chat identity is one exact bot
   header in the top navigation band plus a second same-name message container;
   do not require an accessibility node for the inline keyboard or compose
   field because this Telegram build does not expose them.
4. After the fixed route opens the real storefront entry, capture the Mini App
   top view and verify the same layout requirements.
5. Perform one harmless vertical `ios_swipe` and inspect fresh before/after
   evidence.

When the real Telegram Mini App is visibly in the foreground, call
`ios_agoramarket_webview_evidence` for fixed read-only WebView evidence. The
tool is restricted to the exact production host and a fixed expression; it
does not accept arbitrary JavaScript, read credentials, or mutate the page.

If that probe reports `html`, `body`, `flutter-view`, or `flt-glass-pane` as
fixed-height or `overflow: hidden`, and the document scroll height equals the
viewport, this explains why DOM `window.scrollBy` cannot move the page. It does
not prove that Flutter's internal scroll works, fails, or moved. Keep the
physical-finger or fresh before/after requirement above.

Opening the production URL in a generic embedded browser is not proof of the
real Telegram entry flow.

## Version evidence

The required client version is `1.0.4261+4261` or newer. Prefer visible device
UI or a live response tied to the physical-device session. HTML metadata alone
does not prove that the rendered Mini App used that build.

`ios_agoramarket_webview_evidence` may report the version as `PASS` only when a
fresh paired device frame shows Telegram in the foreground, the inspected page
uses the exact production host, the document is complete, a live Flutter root
exists, and its version metadata exactly matches the fixed bootstrap version
gate and meets the required minimum. Script and resource timing booleans are
diagnostic only because loaded bootstrap elements can disappear from the DOM
or timing buffer after startup.

Report the observed version and evidence source. Use `MISSING_PROOF` when the
physical session cannot be bound to the version.

## Old-cache update behavior

This test requires a known older cached build on the iPhone before loading the
newer production build. Acceptance requires:

- no manual cache clearing
- at most one automatic reload
- final rendered version is the new build
- storefront remains usable after the update

Without a known-old precondition and observed reload count, report
`MISSING_PROOF`. Service-worker source or a fresh install is insufficient.
The fixed WebView evidence probe must likewise leave this result as
`MISSING_PROOF`; it cannot reconstruct a known-old precondition or reload
count after the fact.

## Evidence and status

For each requirement report exactly one of:

- `PASS`: direct physical-device evidence meets it.
- `DEFECT`: direct physical-device evidence demonstrates failure.
- `MISSING_PROOF`: the required state, capability, or precondition is absent.

Name the Sirin evidence path or observable result and explicitly state that no
login, message sending, cart, order, or payment occurred.
