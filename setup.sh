#!/usr/bin/env bash
# Sirin — Rust 工具鏈安裝腳本（可選）
#
# 只有在你還沒安裝 Rust 的情況下才需要執行此腳本。
# 裝好 Rust 之後，直接 `cargo run` 即可——其他初始化由程序本身完成。
#
# 用法：bash setup.sh
set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
ok()   { echo -e "${GREEN}  ✔ $*${NC}"; }
warn() { echo -e "${YELLOW}  ⚠ $*${NC}"; }
info() { echo -e "${CYAN}  → $*${NC}"; }

echo ""
echo -e "${CYAN}Sirin — 安裝 Rust 工具鏈${NC}"
echo ""

# ── Rust ──────────────────────────────────────────────────────────────────────
if command -v cargo &>/dev/null; then
    ok "Rust 已安裝：$(cargo --version)"
else
    warn "未偵測到 cargo，正在安裝 rustup…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    source "$HOME/.cargo/env"
    ok "Rust 安裝完成：$(cargo --version)"
fi

# ── macOS：Xcode CLT ──────────────────────────────────────────────────────────
if [[ "$(uname)" == "Darwin" ]] && ! xcode-select -p &>/dev/null; then
    warn "Xcode Command Line Tools 尚未安裝，正在觸發安裝…"
    xcode-select --install
    warn "安裝視窗已跳出，完成後重新執行此腳本。"
    exit 1
fi

echo ""
ok "環境就緒。執行以下指令啟動 Sirin："
echo ""
echo "    cargo run"
echo ""
info "首次啟動時，Sirin 會自動："
info "  • 建立 .env（從 .env.example 複製）"
info "  • 建立所需資料目錄"
info "  • 偵測本機 LLM 服務（Ollama / LM Studio）"
echo ""
