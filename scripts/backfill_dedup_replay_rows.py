#!/usr/bin/env python3
"""Backfill helper for #276 — dedup the replay-row double-write bug.

## Background

Pre-`6acaa9c` (2026-05-05), every successful replay-mode run inserted
TWO rows into `test_runs`:
  - One from `executor.rs::run_test_inner` replay branch
    (is_replay=1, iterations=0, status=passed, started_at=replay finish)
  - One from `mod.rs::run_test_with_run_id` (is_replay=0, full
    iterations, started_at=run start)

Result: `replay_health.passed_via_replay` was reading 2× actual.
Forward fix landed in 6acaa9c; this script cleans up historical rows.

## What it does (conservative)

Identifies pairs that EXACTLY match the bug pattern:
  - 2 rows for the same run_id
  - one is_replay=1 AND iterations=0
  - one is_replay=0
  - same status
  - same duration_ms

For each match:
  - UPDATE the kept (is_replay=0) row → is_replay=1 (correct mode)
  - DELETE the duplicate (is_replay=1, iter=0) row

Pairs that DON'T match the strict pattern (e.g. retries with
different iter / status — see #241 flakiness retry policy) are
LEFT ALONE.  Verified manually: those are legitimate retries, not
the double-write bug.

## Usage

  python scripts/backfill_dedup_replay_rows.py

The script:
1. Auto-backs up the SQLite to `<db>.bak.<timestamp>` first.
2. Reports counts before changes.
3. Applies changes in a single transaction.
4. Reports counts after.

Idempotent: running again with no qualifying pairs is a no-op.

## Verified once on 2026-05-06

  Before: 4096 rows, 420 dup groups
  After:  3695 rows, 19 dup groups (legit retries)
  Deleted: 401 rows
  replay_health rate: 31.5% → 48.7% (matches the ~2× inflation hypothesis)

## Closes

  #276 — Backfill duplicate replay rows
"""
import sqlite3
import os
import shutil
import sys
import time

def main():
    db_path = os.path.expandvars(r'%LOCALAPPDATA%\Sirin\memory\test_memory.db')
    if not os.path.isfile(db_path):
        print(f"ERROR: db not found at {db_path}", file=sys.stderr)
        sys.exit(1)

    backup = f"{db_path}.bak.{time.strftime('%Y%m%d_%H%M%S')}"
    shutil.copy2(db_path, backup)
    print(f"backup: {backup}")

    con = sqlite3.connect(db_path)
    con.row_factory = sqlite3.Row

    pre_total = con.execute("SELECT COUNT(*) FROM test_runs").fetchone()[0]
    pre_dups = con.execute("""SELECT COUNT(*) FROM (
        SELECT run_id FROM test_runs
        WHERE run_id IS NOT NULL GROUP BY run_id HAVING COUNT(*) > 1
    )""").fetchone()[0]
    print(f"pre: {pre_total} rows, {pre_dups} dup groups")

    target_ids = con.execute("""
        WITH pairs AS (
            SELECT run_id, COUNT(*) as cnt,
                   SUM(CASE WHEN is_replay=1 AND COALESCE(iterations,0)=0 THEN 1 ELSE 0 END) as replay_ones,
                   SUM(CASE WHEN is_replay=0 THEN 1 ELSE 0 END) as nonreplay_zeros,
                   COUNT(DISTINCT duration_ms) as distinct_durations,
                   COUNT(DISTINCT status) as distinct_statuses
            FROM test_runs WHERE run_id IS NOT NULL
            GROUP BY run_id HAVING COUNT(*) > 1
        )
        SELECT run_id FROM pairs
        WHERE replay_ones = 1 AND nonreplay_zeros = 1
          AND distinct_durations = 1 AND distinct_statuses = 1
    """).fetchall()
    target_run_ids = [r['run_id'] for r in target_ids]
    print(f"identified {len(target_run_ids)} run_ids matching the 6acaa9c bug pattern")

    deleted, updated = 0, 0
    for rid in target_run_ids:
        pair = con.execute("""SELECT id, is_replay, COALESCE(iterations,0) as it
                              FROM test_runs WHERE run_id=? ORDER BY id""", (rid,)).fetchall()
        if len(pair) != 2:
            continue
        keeper = next((r for r in pair if r['is_replay'] == 0), None)
        dup    = next((r for r in pair if r['is_replay'] == 1 and r['it'] == 0), None)
        if not keeper or not dup:
            continue
        con.execute("UPDATE test_runs SET is_replay=1 WHERE id=?", (keeper['id'],))
        con.execute("DELETE FROM test_runs WHERE id=?", (dup['id'],))
        updated += 1
        deleted += 1

    con.commit()
    print(f"updated {updated} keeper rows (set is_replay=1)")
    print(f"deleted {deleted} duplicate rows")

    post_total = con.execute("SELECT COUNT(*) FROM test_runs").fetchone()[0]
    post_dups = con.execute("""SELECT COUNT(*) FROM (
        SELECT run_id FROM test_runs
        WHERE run_id IS NOT NULL GROUP BY run_id HAVING COUNT(*) > 1
    )""").fetchone()[0]
    print(f"post: {post_total} rows, {post_dups} dup groups (legit retries)")

if __name__ == "__main__":
    main()
