#!/usr/bin/env bash
# squad-cleanup-old-done.sh
# Prune done/failed tasks older than N days from the multi_agent task queue.
#
# Usage: bash scripts/squad-cleanup-old-done.sh [DAYS]
#   DAYS  — how many days to keep (default 7, clamped 1-90)
#
# Env:
#   SQUAD_CLEANUP_DRY_RUN=1  — print what would be pruned without modifying files

set -euo pipefail

# ── Args ────────────────────────────────────────────────────────────────────
DAYS="${1:-7}"
if ! [[ "$DAYS" =~ ^[0-9]+$ ]]; then
  echo "ERROR: DAYS must be a positive integer, got: $DAYS" >&2
  exit 1
fi
# Clamp 1-90
DAYS=$(( DAYS < 1 ? 1 : DAYS > 90 ? 90 : DAYS ))

DRY_RUN="${SQUAD_CLEANUP_DRY_RUN:-0}"

# ── Locate queue file ────────────────────────────────────────────────────────
QUEUE=""
if [[ -n "${LOCALAPPDATA:-}" ]]; then
  # Windows (Git Bash / MSYS2)
  WIN_PATH="$LOCALAPPDATA/Sirin/data/multi_agent/task_queue.jsonl"
  # Convert backslashes if needed
  QUEUE="${WIN_PATH//\\//}"
elif [[ -f "$HOME/.local/share/sirin/data/multi_agent/task_queue.jsonl" ]]; then
  QUEUE="$HOME/.local/share/sirin/data/multi_agent/task_queue.jsonl"
elif [[ -f "$HOME/Library/Application Support/Sirin/data/multi_agent/task_queue.jsonl" ]]; then
  QUEUE="$HOME/Library/Application Support/Sirin/data/multi_agent/task_queue.jsonl"
fi

if [[ -z "$QUEUE" || ! -f "$QUEUE" ]]; then
  echo "ERROR: task_queue.jsonl not found. Checked:" >&2
  [[ -n "${LOCALAPPDATA:-}" ]] && echo "  $LOCALAPPDATA/Sirin/data/multi_agent/task_queue.jsonl" >&2
  echo "  $HOME/.local/share/sirin/data/multi_agent/task_queue.jsonl" >&2
  echo "  $HOME/Library/Application Support/Sirin/data/multi_agent/task_queue.jsonl" >&2
  exit 1
fi

echo "Queue: $QUEUE"
echo "Keeping done/failed tasks from last $DAYS day(s)."
[[ "$DRY_RUN" == "1" ]] && echo "[DRY RUN — no files will be modified]"

# ── Backup ───────────────────────────────────────────────────────────────────
BACKUP="${QUEUE}.bak.$(date +%Y%m%d_%H%M%S)"
if [[ "$DRY_RUN" != "1" ]]; then
  cp "$QUEUE" "$BACKUP"
  echo "Backup: $BACKUP"
fi

# ── Filter via node ──────────────────────────────────────────────────────────
TMP="${QUEUE}.tmp"

node - "$QUEUE" "$DAYS" "$DRY_RUN" "$TMP" <<'NODE'
const fs   = require('fs');
const path = require('path');

const [,, queuePath, daysArg, dryRun, tmpPath] = process.argv;
const DAYS    = parseInt(daysArg, 10);
const cutoff  = Date.now() - DAYS * 24 * 60 * 60 * 1000;
const PRUNABLE = new Set(['done', 'failed']);

const raw   = fs.readFileSync(queuePath, 'utf8');
const lines = raw.split('\n').filter(l => l.trim() !== '');

let kept = 0, pruned = 0;
const kept_lines = [];

for (const line of lines) {
  let task;
  try {
    task = JSON.parse(line);
  } catch {
    // Malformed line — keep it (don't silently destroy unknown data)
    kept_lines.push(line);
    kept++;
    continue;
  }

  const status = (task.status || '').toLowerCase();

  if (!PRUNABLE.has(status)) {
    // Always keep queued / running / retrying / etc.
    kept_lines.push(line);
    kept++;
    continue;
  }

  // Prunable status — check age via finished_at or updated_at or created_at
  const ts = task.finished_at || task.updated_at || task.created_at || null;
  let taskTime = ts ? new Date(ts).getTime() : 0;

  if (!isNaN(taskTime) && taskTime >= cutoff) {
    // Recent enough — keep
    kept_lines.push(line);
    kept++;
  } else {
    // Old done/failed — prune
    if (dryRun === '1') {
      const id = task.id || task.task_id || '?';
      console.log(`  WOULD PRUNE [${status}] id=${id} finished_at=${ts || 'unknown'}`);
    }
    pruned++;
  }
}

if (dryRun !== '1') {
  fs.writeFileSync(tmpPath, kept_lines.join('\n') + (kept_lines.length ? '\n' : ''));
}

console.log(`Pruned ${pruned} tasks (kept ${kept}).`);
NODE

# ── Atomic replace ───────────────────────────────────────────────────────────
if [[ "$DRY_RUN" != "1" ]]; then
  mv "$TMP" "$QUEUE"
  echo "Done. Backup: $BACKUP"
fi
