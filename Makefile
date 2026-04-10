# Sirin — 開發快捷指令
# 用法：make <target>
# 需要 GNU Make（macOS 用 brew install make，Windows 用 choco install make）

.PHONY: help setup run build check test clean logs

# 預設目標：顯示說明
help:
	@echo ""
	@echo "  Sirin 開發指令"
	@echo "  ─────────────────────────────────"
	@echo "  make setup    安裝 Rust 工具鏈（還沒裝 Rust 才需要）"
	@echo "  make run      開發模式啟動"
	@echo "  make build    編譯 Release 版本"
	@echo "  make check    快速型別檢查（不產生 binary）"
	@echo "  make test     執行單元測試"
	@echo "  make clean    清除編譯產物"
	@echo ""

# 安裝 Rust（沒有 Rust 才需要執行）
setup:
	@bash setup.sh

# 開發模式
run:
	cargo run

# Release 版本
build:
	cargo build --release
	@echo ""
	@echo "  輸出：./target/release/sirin"

# 型別檢查（快）
check:
	cargo check

# 單元測試
test:
	cargo test

# 清除
clean:
	cargo clean
