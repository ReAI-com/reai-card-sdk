//! dyj 示例共用的日志初始化：中文 + emoji 控制台格式，并可选地把日志推给网页 SSE。
//!
//! 约定：SDK 的日志消息以 emoji 开头、target 用
//! `dyj.<模块>`；这里负责把时间戳、模块中文名和键值对拼成一行。
//!
//! 本模块被 dyj_cli 与 dyj_web 通过 #[path] 各自编译一份：
//! dyj_cli 只用 stdout 输出，level/line 字段与 log_sender 在那边是死代码。
#![allow(dead_code)]

use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// 一条日志的结构化形态：格式化好的整行 + 供网页过滤的级别。
#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: String,
    pub line: String,
}

struct LogHub {
    tx: tokio::sync::broadcast::Sender<LogLine>,
}

static HUB: OnceLock<LogHub> = OnceLock::new();

/// 初始化全局日志。`verbosity`: 0=INFO（默认）、1=DEBUG（--verbose）、2+=TRACE（--trace），
/// `RUST_LOG` 环境变量可覆盖。返回订阅句柄前日志即可用；网页端用 [`log_sender`] 拿广播。
pub fn init_tracing(verbosity: u8) {
    let default_filter = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_owned());
    let filter = EnvFilter::try_new(filter).expect("RUST_LOG 语法错误");
    let (tx, _) = tokio::sync::broadcast::channel(1024);
    let _ = HUB.set(LogHub { tx });
    tracing_subscriber::registry()
        .with(filter)
        .with(ConsoleHubLayer)
        .init();
}

/// 日志广播的发送端（网页 SSE 订阅用）。须先调用 [`init_tracing`]。
pub fn log_sender() -> tokio::sync::broadcast::Sender<LogLine> {
    HUB.get().expect("init_tracing 未调用").tx.clone()
}

struct ConsoleHubLayer;

impl<S: tracing::Subscriber> tracing_subscriber::layer::Layer<S> for ConsoleHubLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let level = event.metadata().level().to_string();
        let module = module_label(event.metadata().target());
        let line = format_line(module, &visitor.message, &visitor.fields);

        {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
        }
        if let Some(hub) = HUB.get() {
            let _ = hub.tx.send(LogLine { level, line });
        }
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<(&'static str, String)>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push((field.name(), format!("{value:?}")));
        }
    }
}

/// `HH:MM:SS.mmm {emoji} [{模块}] 消息 key=value`。
/// SDK 约定消息以 emoji 开头：首个空格前的非 ASCII token 视为 emoji。
fn format_line(module: &str, message: &str, fields: &[(&'static str, String)]) -> String {
    let now = chrono::Local::now().format("%H:%M:%S%.3f");
    let (emoji, rest) = match message.split_once(' ') {
        Some((head, rest)) if !head.is_ascii() => (head, rest),
        _ => ("•", message),
    };
    let mut line = format!("{now} {emoji} [{module}] {rest}");
    for (key, value) in fields {
        line.push_str(&format!(" {key}={value}"));
    }
    line
}

fn module_label(target: &str) -> &str {
    match target {
        "dyj.ble" => "BLE",
        "dyj.proto" => "协议",
        "dyj.device" => "设备",
        "dyj.file" => "文件",
        "dyj.wifi" => "WiFi",
        "dyj.ap" => "AP",
        "dyj.event" => "事件",
        other => other,
    }
}
