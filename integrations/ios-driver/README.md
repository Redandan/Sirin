# Sirin iOS Driver

This directory is the maintained source for Sirin's private Windows iPhone
driver. It is not a second MCP server or public Codex entry point. Codex calls
only Sirin at `http://127.0.0.1:7700/mcp`; Sirin calls this loopback provider.

The low-level `phone_harness` modules were derived from SideTap commit
`033e567f89fca27a3d31fa2c891ca80c7b04e9d8` plus the local safety hardening
validated on 2026-08-23. The upstream MIT license is preserved in
`THIRD_PARTY_LICENSE`.

Included:

- USB discovery, tunnel/DDI/WDA lifecycle primitives;
- passive-by-default attach: idle status checks do not start tunnel, DDI, WDA,
  or port forwards; link activation occurs only for an explicit Sirin action;
- fresh screenshot and phone-information reads;
- fixed AgoraMarket, Telegram and Codex iPhone Remote routes, vertical scrolling,
  Home and app launch;
- optional arbitrary tap only when both provider and Sirin supervised-control
  gates are explicitly enabled;
- loopback-only HTTP, redacted rotating logs, no placeholder screenshots.

The private provider exposes a cheap `/api/healthz` process check and one
`/api/snapshot` read that reuses the same USB/link observation for status and
phone information. The Sirin supervisor uses the process check instead of
polling the phone every minute. Before any cold link activation, the provider
verifies the existing inbound LAN block; if it is absent or cannot be proven,
activation fails closed without creating a rule or changing the phone.
Releasing the Sirin lease also stops the temporary forwards, WDA runner and
USB tunnel that Sirin itself started. The private Driver process remains alive
for cheap health checks, so a connected test phone returns to passive idle.

Excluded:

- SideTap's standalone MCP, website, viewer GUI and console;
- passcode entry, unlock, messaging, clipboard, arbitrary typing;
- signing, trust approval, installation, jailbreak, order and payment flows;
- certificates, provisioning profiles, device IDs, signed IPAs and downloaded
  installer executables.

Runtime files live under `%LOCALAPPDATA%\Sirin\ios-driver`; they are generated
by Sirin's installer and are not committed to this repository.

The installer reuses an already verified private runtime or downloads the
pinned official `go-ios@1.3.2` npm package. It verifies the registry package
SHA-512 and Windows binary SHA-256 before activation. A SideTap checkout is
not an installer dependency.

For the unavoidable WDA renewal handoff, the installer also downloads Appium's
pinned official `WebDriverAgentRunner-Runner.zip`, verifies its GitHub release
SHA-256, repacks that unchanged `.app` payload as an **unsigned** IPA under
`%LOCALAPPDATA%\Sirin\ios-driver\operator-assets`, and stores the upstream BSD
license beside it. Sirin never signs or installs that IPA. Sideloadly, Apple ID
prompts and device trust remain human-operated.
