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
#
# 本机私密配置放 .env.local（已 git 忽略，不要提交），make 会自动读入并导出给测试台：
#   REAI_CARD_AP_PASSWORD=xxx     设备热点密码（控制台快传提示里显示；SDK 不内置）
#   REAI_CARD_WIFI_SSID=xxx       WiFi 升级专页预填的路由器 SSID
#   REAI_CARD_WIFI_PASSWORD=xxx   对应密码
# 也可以直接 make web REAI_CARD_AP_PASSWORD=xxx 临时传入。

PORT ?= 8787
ARGS ?= scan
-include .env.local
export REAI_CARD_AP_PASSWORD REAI_CARD_WIFI_SSID REAI_CARD_WIFI_PASSWORD

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
	cargo run --example dyj_web -- --port $(PORT)
