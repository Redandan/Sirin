# Codex iPhone Remote physical-iPhone acceptance

Use this procedure for the fixed Sirin route `codex_remote`, which opens
`https://remote.purrtechllc.com/`. This is a product-specific acceptance route,
not permission to open an arbitrary URL, deploy a release, reset pairing, or
change the Windows Host.

The Windows Codex/ChatGPT Desktop session remains the execution end. Sirin
controls only the physical iPhone used to observe the remote PWA. Do not create
a second Codex session or enter pairing secrets, credentials, private text, or
clipboard contents.

## Evidence contract

Keep Sirin device evidence separate from Codex iPhone Remote product evidence:

| Requirement | PASS evidence |
| --- | --- |
| `ROUTE-01` | `ios_acceptance_run(route="codex_remote")` succeeds, reports its lease released, and its fresh evidence shows the fixed remote origin or expected PWA UI. |
| `UI-01` | A fresh capture shows a usable portrait layout with connection controls visible and no blocking browser error. |
| `CONNECTION-01` | Fresh phone UI evidence and the product's own protected status agree that this phone and the Windows Host are connected. A screenshot alone is insufficient. |
| `VIDEO-01` | Two fresh observations plus product receiver telemetry prove advancing presented frames. Two different full-frame hashes alone are insufficient. |
| `INPUT-01` | One explicitly authorized gesture is followed by product telemetry proving a new control was sent and applied to Windows. Sirin visual change alone is insufficient. |
| `RESUME-01` | The same product session and advancing frames are observed after the requested background/reopen interval. Reopening a Safari tab does not prove installed Home Screen PWA lifecycle. |

Classify each requirement as exactly `PASS`, `DEFECT`, or `MISSING_PROOF`.
If the current deployment, protected Server status, receiver telemetry, or
physical-device action evidence cannot be observed, keep the affected gate as
`MISSING_PROOF`.

## Safe passive run

1. If the target checkout is available, read its current `docs/ACCEPTANCE.md`
   and `docs/STATUS.md`. Treat source and historic deployment notes as context,
   not current runtime proof.
2. Call `ios_acceptance_run` with `route="codex_remote"` and one stable unique
   `run_id`. Reuse the ID only for an ambiguous reply. Do not add separate
   status, capture, lease-start, or lease-stop calls around this fixed route.
3. Confirm the result reports `lease_release.ok=true`, then classify only
   `ROUTE-01` and `UI-01` from its fresh before/after phone evidence.
   If pairing, login, Apple trust, or another approval is required, stop and
   return the exact `NEEDS_HUMAN_*`, `SECURITY_BLOCK_*`, or `MISSING_PROOF`
   state.
4. Optional Safari-resume checking is a separate multi-action flow: observe
   status, acquire one manual acceptance lease, use `ios_home`, wait for the
   requested interval, then `ios_open_app(app="safari")`. Label this as Safari
   lifecycle evidence; never promote it to installed Home Screen PWA proof.
5. Release that manual lease with `ios_control_session_stop`, including on
   failure.

The passive run must not tap the remote desktop, type, toggle Wi-Fi/cellular,
lock Windows, change pairing, restart services, deploy, or alter firewall and
Driver settings.

## Interactive extension

Run this only when the user separately requests physical input acceptance for
this product. `supervised_control` must already be enabled by the Sirin host,
the provider must not be acceptance-only, and the user request must identify
the bounded gesture to test. Do not change those host settings from this skill.

Re-capture immediately before every coordinate action. For `ios_tap`, provide
that frame's exact `expected_frame_sha256`; for every action use a stable unique
`action_id`. Re-observe the remote surface instead of reusing old coordinates.
Release the lease on every exit path.

Do not claim `INPUT-01` from a changed screenshot or cursor position. Correlate
the action with the product's authenticated session, sent-control counter and
Windows-applied counter. If those protected metrics are unavailable, report
`MISSING_PROOF`.

Wi-Fi/5G roaming, Windows lock/unlock, pairing, text/IME submission, clipboard,
60Hz/120Hz finger feel, and installed Home Screen PWA lifecycle remain
user-owned or separately authorized acceptance steps. Never infer them from an
Android emulator, Safari tab, automated test count, or historical report.

## Report

Report the six requirement IDs above with timestamps and current evidence
paths or observable results. State explicitly that credentials, pairing,
messaging, clipboard contents, network settings, orders and payment were not
touched.
