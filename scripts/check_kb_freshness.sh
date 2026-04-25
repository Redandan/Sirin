#!/bin/bash
# scripts/check_kb_freshness.sh
#
# Post-commit hook: scan files changed in the last commit and mark any KB
# entries whose `fileRefs` overlap as STALE via mcp__agora-trading__kbMarkStale.
#
# Why:
#   Today (2026-04-25) we manually audited the Sirin KB after a release pass
#   and found 4 entries that were factually outdated against the new commits
#   (sub-trait counts, env vars, action lists).  Auto-mark stale closes that
#   audit gap so the next session sees the staleness without re-discovering it.
#
# How:
#   1. Read changed files from `git show --stat HEAD`
#   2. Use kbHealth + kbSearch to enumerate entries whose fileRefs reference
#      any changed file
#   3. Call kbMarkStale for each, citing the commit hash as the reason
#
# Configuration:
#   KB_PROJECT          — project slug (default: "sirin")
#   KB_MCP_URL          — MCP endpoint (default: http://localhost:3001/mcp)
#   KB_FRESHNESS_DRY    — set to 1 to print intended actions without calling KB
#   KB_FRESHNESS_DISABLE — set to 1 to skip entirely (useful in CI / cherry-pick)
#
# Install as a hook:
#   ln -s ../../scripts/check_kb_freshness.sh .git/hooks/post-commit
#   chmod +x .git/hooks/post-commit
#
# Failures are non-fatal — script always exits 0 so the commit isn't blocked.

set -uo pipefail

if [[ "${KB_FRESHNESS_DISABLE:-0}" == "1" ]]; then
    exit 0
fi

PROJECT="${KB_PROJECT:-sirin}"
URL="${KB_MCP_URL:-http://localhost:3001/mcp}"
DRY="${KB_FRESHNESS_DRY:-0}"

# Get list of files changed by HEAD commit, ignoring deletions and worktree noise.
COMMIT_HASH=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
CHANGED_FILES=$(git diff-tree --no-commit-id --name-only -r HEAD 2>/dev/null \
    | grep -v "^.claude/worktrees/" \
    | grep -v "^.git/" \
    | grep -v "^/dev/null" || true)

if [[ -z "$CHANGED_FILES" ]]; then
    exit 0
fi

# JSON-RPC helper: POST a tools/call to the MCP endpoint, return raw JSON.
mcp_call() {
    local tool="$1"
    local args="$2"
    curl -sS --max-time 30 -X POST "$URL" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$tool\",\"arguments\":$args}}" \
        2>/dev/null
}

# Probe MCP server availability — bail silently if it's not up (dev machine
# without the agora-trading service running shouldn't see error spam).
if ! curl -sS --max-time 3 -o /dev/null "$URL" 2>/dev/null; then
    [[ "$DRY" == "1" ]] && echo "[kb-freshness] MCP at $URL unreachable — skipping"
    exit 0
fi

# Pull all confirmed entries for this project — we use kbSearch with broad
# query and high limit since there's no "list all" primitive.  20 is the
# practical cap (KB stays small for sirin/agora-backend/flutter projects).
ENTRIES=$(mcp_call "kbSearch" \
    "{\"query\":\"$PROJECT\",\"domain\":\"\",\"layer\":\"\",\"status\":\"confirmed\",\"limit\":50,\"project\":\"$PROJECT\"}")

# Extract per-entry blocks: each starts with `▶ [topicKey]` and may have a
# `files: ...` line listing fileRefs comma-separated.  We use awk to walk
# block-by-block and emit `topicKey<TAB>file1,file2,...` for each hit.
PARSED=$(echo "$ENTRIES" | awk '
    /^▶ \[/ {
        # Extract topicKey from "▶ [key] Title"
        match($0, /\[([^]]+)\]/, k)
        cur_key = k[1]
        cur_files = ""
    }
    /^  files: / {
        # Extract everything after "  files: "
        cur_files = substr($0, 10)
    }
    /^$/ && cur_key != "" {
        if (cur_files != "") {
            print cur_key "\t" cur_files
        }
        cur_key = ""
        cur_files = ""
    }
    END {
        if (cur_key != "" && cur_files != "") {
            print cur_key "\t" cur_files
        }
    }
')

[[ -z "$PARSED" ]] && exit 0

# For each (topicKey, files) row, check whether any changed file appears in
# the entry's fileRefs.  Substring match is intentional — KB fileRefs often
# carry line suffixes ("src/foo.rs:42") and we want the unsuffixed match.
STALED=0
while IFS=$'\t' read -r topic files; do
    [[ -z "$topic" || -z "$files" ]] && continue
    for f in $CHANGED_FILES; do
        if [[ "$files" == *"$f"* ]]; then
            REASON="commit ${COMMIT_HASH}: changed ${f}"
            if [[ "$DRY" == "1" ]]; then
                echo "[kb-freshness] DRY: would mark stale: $topic ($REASON)"
            else
                resp=$(mcp_call "kbMarkStale" \
                    "{\"topicKey\":\"$topic\",\"reason\":\"$REASON\",\"project\":\"$PROJECT\"}")
                if echo "$resp" | grep -q '"error"'; then
                    echo "[kb-freshness] $topic mark-stale FAILED: $resp"
                else
                    echo "[kb-freshness] marked stale: $topic ($REASON)"
                fi
            fi
            STALED=$((STALED + 1))
            break  # one fileRef hit is enough per topic
        fi
    done
done <<< "$PARSED"

if [[ "$STALED" -gt 0 ]]; then
    echo "[kb-freshness] ${STALED} KB entr$([ "$STALED" -eq 1 ] && echo "y" || echo "ies") flagged stale by commit ${COMMIT_HASH}"
fi
exit 0
