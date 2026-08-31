# Sirin Windows Daemon

Use this when Sirin should stay alive as a local operations AI after Windows
login. The daemon keeps the same web UI and MCP server behavior:

- Web UI: `http://127.0.0.1:7700/ui/`
- MCP: `http://127.0.0.1:7700/mcp`
- Local ops schedule runner: checks due schedules once per minute

## Build

```powershell
cargo build --release
```

The daemon task runs:

```powershell
.\target\release\sirin.exe --headless
```

Headless only suppresses auto-opening the browser. The UI remains available in
your browser, and the ops schedule loop still runs.

## Install For A Logged-In Session

```powershell
.\scripts\install-sirin-daemon-task.ps1 -Action Install -RunNow
```

This creates a per-user Windows Task Scheduler entry named
`Sirin Local Ops Daemon`. By default it starts the release binary directly:

```powershell
C:\path\to\Sirin\target\release\sirin.exe --headless
```

Normal runtime has one long-lived `sirin.exe` process. Windows Task Scheduler
is the lifecycle launcher, not a second Sirin worker. The task has a logon
trigger plus a five-minute wake-up guarded by `MultipleInstances=IgnoreNew`,
one-minute restart retries, `StartWhenAvailable`, and no execution-time limit.
An already-running Sirin process is never duplicated.

For development sessions where the binary is rebuilt often, the optional
watchdog mode still exists:

```powershell
.\scripts\install-sirin-daemon-task.ps1 -Action Install -RunNow -UseWatchdog
```

That mode runs `scripts/watch-sirin.ps1` and can relaunch Sirin when
`target\release\sirin.exe` is rebuilt. Do not use it for the normal
always-on local ops setup.

## Status

```powershell
.\scripts\install-sirin-daemon-task.ps1 -Action Status
```

Then open:

```text
http://127.0.0.1:7700/ui/
```

In `運營 Scout`, due schedules can run automatically or via `執行 due` /
`立即執行`. Approved findings appear in the approved handoff queue.

## Stop Or Remove

Remove the startup task:

```powershell
.\scripts\install-sirin-daemon-task.ps1 -Action Remove
```

Stop the current Sirin process:

```powershell
Get-Process sirin -ErrorAction SilentlyContinue | Stop-Process -Force
```

## Boundaries

The daemon is local-first. It keeps schedules, review state, candidates, and
handoffs under Sirin app data. It does not modify AgoraMarketAPI code. Ops
inspection tasks may call existing read-only MCP tools when configured, and
fail into local `FAILED + last_error` state if credentials or endpoints are
missing.

The intended single-process model is therefore: one `sirin.exe` owns all Sirin
work while the user session remains logged in. A crash is handled by Task
Scheduler restart settings, and an unexpected clean exit is covered by the
five-minute wake-up. Cold boot still requires one user logon; Sirin does not
store a Windows password or configure automatic logon.
