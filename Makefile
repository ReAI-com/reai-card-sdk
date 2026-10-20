# reai-card-sdk 快捷命令
#
#   make verify            完整验收（fmt --check + check + test + clippy -D warnings）
#   make check             cargo check
#   make fmt               自动格式化
#   make lint              clippy（-D warnings）
#   make test              单元测试
#   make cli ARGS=check    CLI 测试工具（ARGS 传给 dyj_cli，如 realtime 30 rt.wav）
#   make web [PORT=8787]   网页控制台（浏览器打开 http://127.0.0.1:8787）
#
# 日志级别：RUST_LOG=debug/trace 可调细。
# 设备热点密码不内置：make web REAI_CARD_AP_PASSWORD=xxx 可让控制台提示里显示密码。

PORT ?= 8787
ARGS ?= scan

.DEFAULT_GOAL := help
.PHONY: help verify fmt-check check fmt lint test cli web

help:
	@sed -n '2,12p' $(MAKEFILE_LIST)

verify: fmt-check check test lint

fmt-check:
	cargo fmt --check

check:
	cargo check --all-targets

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

cli:
	cargo run --example dyj_cli -- $(ARGS)

web:
	REAI_CARD_AP_PASSWORD=$(REAI_CARD_AP_PASSWORD) cargo run --example dyj_web -- --port $(PORT)
