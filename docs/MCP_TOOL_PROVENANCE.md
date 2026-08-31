# MCP Tool Provenance and Clean-Build Contract

## Accepted baseline

The accepted local Sirin runtime on 2026-08-31 exposed **190 MCP tools**.
The canonical, sorted name inventory is
[`config/mcp_tool_baseline.json`](../config/mcp_tool_baseline.json).

This baseline was recovered from the deployed source snapshot recorded by:

- deployment manifest:
  `%LOCALAPPDATA%\Sirin\deployments\sirin-f595647d37ac\deployment-manifest.json`
- deployed binary SHA-256:
  `f595647d37ac073175afc00b69927c9b69262b7b6a2c6afda9d657b036adb35b`
- source branch before recovery:
  `codex/codex-work-supervisor` at
  `7de9a4395aeee02f279ecb9aae961f2a5f15335e`

The old clean branch exposed 110 tools. The deployed runtime added 80 tools
without committing all of their source. They were distributed across these
domains:

| Domain | Added tools |
|---|---:|
| A2A handoff and review | 11 |
| Audit and audit handoff | 11 |
| Generic device and geolocation | 15 |
| Physical iPhone and iOS location | 31 |
| Internal operations and schedules | 11 |
| Research monitor status | 1 |

Only 13 of the 80 additions had an existing Git commit. The remaining 67
existed only in the deployed snapshot and the user's dirty main worktree.
The recovery commit therefore checkpoints the exact deployed project source,
configuration, documentation, integration runtime, and install scripts.

The recovery intentionally excludes all non-source state:

- `target/` and release/test build output;
- `.sirin/`, `data/`, and `test_data/` runtime state;
- candidate logs, supervisor ledgers, token-trend files, screenshots, and
  temporary run IDs;
- downloaded iOS Developer Disk Images, trustcache files, and the machine-local
  `selfIdentity.plist`;
- credentials and the real `.env` file.

## Regression guard

`src/mcp_parity_tests.rs::deployed_tool_inventory_matches_baseline` constructs
the actual registry directly and adds the remaining legacy-listed tool. It
must equal the 190-name baseline exactly. This complements the existing
list/call parity test: parity alone stays green if an entire tool domain is
deleted from both paths.

For a deliberate tool addition, rename, or removal:

1. update the implementation and tool schema;
2. update `config/mcp_tool_baseline.json` in the same commit;
3. update `docs/MCP_API.md` and the Sirin development skill;
4. run the parity/inventory tests, full `cargo test --bin sirin`, and a clean
   release candidate `tools/list` check;
5. require the deployment candidate to match the committed inventory before
   switching the Windows daemon.

Never repair inventory drift by lowering a count gate or copying a build
directory. The accepted source commit must reproduce the tool names from a
clean checkout.
