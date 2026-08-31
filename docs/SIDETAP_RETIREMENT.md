# SideTap Retirement Record

This record is the non-sensitive migration manifest for retiring the standalone
`D:\SideTap` checkout. Sirin is the maintained control plane, package and Codex
entry point. This document does not authorize deleting, moving or archiving the
legacy checkout.

## Source provenance

- Audited SideTap revision: `033e567f89fca27a3d31fa2c891ca80c7b04e9d8`
- Audit date: 2026-08-23
- The SideTap checkout had 20 local changes and therefore must not be treated as
  a disposable clean clone.
- Sirin's derived low-level source remains covered by
  `integrations/ios-driver/THIRD_PARTY_LICENSE`.

## Maintained in Sirin

The current SideTap working copies of these modules were compared directly with
their Sirin counterparts:

| SideTap responsibility | Sirin maintained source | Result |
| --- | --- | --- |
| capture | `integrations/ios-driver/src/phone_harness/capture.py` | Local hardening present; only trailing whitespace differs. |
| configuration | `integrations/ios-driver/src/phone_harness/config.py` | Local hardening present, with Sirin runtime paths, no passcode, acceptance-only default and Sirin-owned port variables. |
| device lifecycle | `integrations/ios-driver/src/phone_harness/device.py` | Local hardening present, with the SideTap checkout fallbacks replaced by Sirin's private pinned runtime. |
| safe interaction helpers | `integrations/ios-driver/src/phone_harness/helpers.py` | Local hardening present; only trailing whitespace differs. |
| approval, trust and WDA client primitives | matching files under `integrations/ios-driver/src/phone_harness` | Maintained copies present; only trailing whitespace differs. |
| unattended provider | `integrations/ios-driver/scripts/unattended_host.py` | Reimplemented as a loopback-only Sirin provider instead of copying the SideTap viewer host. |
| source regression coverage | `integrations/ios-driver/tests` | Core tests are maintained; Sirin-specific provider safety is covered by `test_provider.py`. |

The provider tests cover loopback binding, acceptance-only rejection, fixed app
and route allowlists, vertical-only scrolling, supervised tap gating and device
identifier redaction. Sirin's installer and scheduled-task tests replace the
legacy windowless/watchdog scripts.

## Deliberately not imported

The following standalone SideTap surfaces are not Sirin dependencies:

- `admin.py`, `viewer.py`, their website/GUI and their standalone tests;
- `mcp_server.py`, `run.py` and `__main__.py`, because Sirin owns the MCP and
  process lifecycle;
- `windowless_entry.py`, `start_unattended.ps1` and `watch_unattended.ps1`,
  because Sirin owns the daemon and Driver task definitions;
- `signing.py` and `syslog.py`, because Sirin must not sign, install, trust,
  unlock or silently broaden physical-device authority;
- standalone SideTap downloads, virtual environments, caches and state.

Acceptance features added to the SideTap viewer are represented in Sirin by the
fixed `safari_store` and `telegram_bot` routes plus the provider's action
allowlist. Sirin does not import the legacy GUI in order to obtain those
features.

## Sensitive material boundary

Do not copy `.env`, `.certs`, provisioning profiles, identities, device IDs,
signed IPAs or WDA signing material into this repository, the Sirin installer,
a portable zip or a general-purpose archive. Their presence in the legacy
checkout was checked by path only; their contents were not inspected during
this audit.

The only WDA binary maintained by Sirin is a runtime-generated **unsigned**
operator IPA. The runtime installer downloads a pinned Appium release ZIP,
verifies its official SHA-256, repacks the unchanged app payload, and stores the
upstream license beside it under `%LOCALAPPDATA%\Sirin\ios-driver\operator-assets`.
It is not committed or bundled, and Sirin cannot sign or install it.

Any signed output, certificate or provisioning material retained for manual WDA
renewal still needs a separate credential-safe storage location. That decision
is independent of maintaining one Sirin source repository.

## Retirement gates

Retirement remains fail-closed until all of these are true:

1. Sirin records a fresh valid screenshot plus a changed safe vertical swipe,
   after the MCP control lease has been released. The proof is tied to the
   current Sirin binary and Driver source hashes.
2. The eight-hour read-only `ios_device_status` soak has a qualifying summary,
   and Sirin independently revalidates the raw JSONL for continuous sequence,
   all-PASS samples, minimum count, eight-hour observed span and bounded gaps.
3. The user explicitly authorizes a Windows reboot and Sirin passes the
   post-login recovery proof with the same reviewed binary.
4. `install-sirin-ios.ps1 -Action Status` reports no active SideTap dependency.
   This includes orphaned processes whose executable is still under the legacy
   task working directory; disabled task definitions alone are insufficient.
5. `install-sirin-ios.ps1 -Action RetireLegacy` removes only the disabled legacy
   task definitions and preserves its backup.
6. The user decides whether the dirty legacy checkout and its sensitive
   operator-owned assets are securely retained or disposed of.

Until gate 6 has an explicit user decision, do not delete, move or archive
`D:\SideTap`. Passing the service gates proves that Sirin no longer needs the
checkout; it does not decide how credentials or uncommitted history should be
handled.
