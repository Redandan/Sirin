# Sirin Trading Guardian

Sirin Trading Guardian is the external watchdog path for AgoraMarketAPI issue
#448. Its job is to detect cross-service risk while staying independent from
the backend JVM.

Current implementation status: Phase 1 dry-run foundation.

## Safety Model

- Default config is `enabled: false`, `dry_run: true`.
- Live write actions are denied unless both `dry_run: false` and
  `allow_live_actions: true`.
- The guardian may only reduce risk: alert, pause, tighten OCO stop loss, or
  disable POC strategies.
- Risk-expanding actions such as enabling strategies or placing orders are
  hard-denied in code.

## Command

```powershell
.\target\release\sirin.exe trading-guardian --once --format=json
```

The current command runs one evaluation pass. It probes backend health and
prints proposed actions plus authorization decisions. It does not execute live
trading writes.

Backend-down drill without touching the real backend:

```powershell
.\target\release\sirin.exe trading-guardian --simulate-backend-down --format=json
```

## Config

Source: `config/trading_guardian.yaml`

Installed/dev runtime path is resolved through `platform::config_path`, so the
installed copy lives under `%LOCALAPPDATA%\Sirin\config\trading_guardian.yaml`.

## Implemented Rules

- R4 backend health check: probes `/actuator/health` and proposes a critical TG
  alert when the backend is down.

## Guardrails Implemented

- Dry-run denies `pauseStrategy`, `modifyOco`, and `disableStrategy` writes.
- `enableStrategy` and market-order style actions are always denied.
- R3 candidate filtering only includes POC-like strategy names (`NoSL*` or
  names containing `POC`).
- Audit payload formatter records enabled/dry-run state, evaluated rules,
  proposed actions, and authorization decisions for future KB writes.
- Each CLI run writes a local audit JSON file under Sirin app data
  `trading_guardian/` when `audit_enabled: true`.

## Remaining Work Before 24/7 Vacation Mode

- Add R1/R2/R3/R5 live data readers through AgoraMarketAPI MCP.
- Add Telegram Bot API send path for critical-only vacation alerts.
- Add local audit persistence and KB sync.
- Add systemd or Windows service deployment.
- Run seven days in dry-run and measure false-positive rate before any live
  write action is enabled.
