#!/bin/bash
# scripts/install_kb_freshness_hook.sh
#
# Install the post-commit KB freshness check as a git hook.
#
# Usage:
#   ./scripts/install_kb_freshness_hook.sh             # install
#   ./scripts/install_kb_freshness_hook.sh --uninstall # remove
#
# Idempotent — safe to re-run.  Won't clobber an existing post-commit hook
# unless it was previously installed by this script (detected via marker).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK_PATH="$REPO_ROOT/.git/hooks/post-commit"
TARGET="$REPO_ROOT/scripts/check_kb_freshness.sh"
MARKER="# installed by scripts/install_kb_freshness_hook.sh"

if [[ "${1:-}" == "--uninstall" ]]; then
    if [[ -f "$HOOK_PATH" ]] && grep -q "$MARKER" "$HOOK_PATH"; then
        rm -f "$HOOK_PATH"
        echo "✓ Removed: $HOOK_PATH"
    else
        echo "ℹ️  No installed hook found at $HOOK_PATH (or it wasn't ours)"
    fi
    exit 0
fi

if [[ ! -x "$TARGET" ]]; then
    echo "✗ $TARGET not found or not executable"
    exit 1
fi

if [[ -f "$HOOK_PATH" ]] && ! grep -q "$MARKER" "$HOOK_PATH"; then
    echo "✗ $HOOK_PATH already exists and was NOT installed by this script."
    echo "  Move it aside or chain it manually:"
    echo "  $TARGET  # call from your existing hook"
    exit 1
fi

cat > "$HOOK_PATH" <<EOF
#!/bin/bash
$MARKER
exec "$TARGET" "\$@"
EOF
chmod +x "$HOOK_PATH"

echo "✓ Installed: $HOOK_PATH → $TARGET"
echo ""
echo "Behaviour:"
echo "  - On every git commit, scans changed files vs KB fileRefs"
echo "  - Marks affected KB entries stale via mcp__agora-trading__kbMarkStale"
echo "  - Silently skips if KB MCP server is unreachable (no spam)"
echo "  - Set KB_FRESHNESS_DRY=1 for dry-run mode"
echo "  - Set KB_FRESHNESS_DISABLE=1 to bypass entirely"
echo ""
echo "Uninstall: $0 --uninstall"
