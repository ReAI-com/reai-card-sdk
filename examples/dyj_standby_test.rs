//! BLE 连接态待机功耗测试：连接卡片后保持连接、不做任何业务操作，
//! 仅定期查询电量（BASE_INFO）并追加写入 CSV，用于观测掉电斜率。
//!
//! 用法：
//!
//! ```text
//! cargo run --example dyj_standby_test -- [poll_secs] [--id <device-id>]
//! ```
//!
//! - `poll_secs`：采样间隔秒数，默认 300（5 分钟）。采样本身是几条 BLE 命令，
//!   对整体功耗影响可忽略。
//! - 结果同时打印到 stdout 并追加到 `standby_battery_log.csv`（含表头，重启测试自动续写）。
//! - 断连即退出并提示（不做自动重连，保持「连接态」测试条件纯净）。
//! - Ctrl-C 退出时写入最后一行并断开连接。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reai_card_sdk::DeviceManager;

const DEFAULT_POLL_SECS: u64 = 300;
const CSV_PATH: &str = "standby_battery_log.csv";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let poll_secs: u64 = args
        .iter()
        .find_map(|arg| arg.parse().ok())
        .unwrap_or(DEFAULT_POLL_SECS);
    let want_id = args
        .windows(2)
        .find(|pair| pair[0] == "--id")
        .and_then(|pair| pair.get(1).cloned());

    println!("=== DYJ 连接态待机功耗测试 ===");
    println!("采样间隔: {poll_secs}s, CSV: {CSV_PATH}");

    let manager = DeviceManager::new()
        .await
        .expect("蓝牙管理器初始化失败（本机蓝牙是否开启？）");

    // macOS 在慢广播（1s 档）下服务过滤扫描偶尔抓不到：多轮重试，每轮 10s，共最多 6 轮
    let mut devices = Vec::new();
    for round in 1..=6 {
        devices = manager
            .scan(Duration::from_secs(10))
            .await
            .expect("BLE 扫描失败");
        if !devices.is_empty() {
            break;
        }
        println!("第 {round} 轮扫描未发现 DYJ 设备，重试...");
    }
    let found = match &want_id {
        Some(id) => devices.iter().find(|d| &d.id == id),
        None => devices.first(),
    }
    .expect("未发现 DYJ 设备：确认卡片已开机且在附近");

    println!("connecting {} ({}) ...", found.name, found.id);
    let device = manager.connect(found).await.expect("连接失败");

    // 挂一个事件打印：断连/按键等主动上报可见
    let mut events = device.subscribe_events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => println!("event: {event:?}"),
                Err(_) => return,
            }
        }
    });

    // 记录一次设备信息作为测试条件存档
    let info = device.device_info().await.expect("读取设备信息失败");
    println!("device_info: {info:?}");

    let started = SystemTime::now();
    let mut csv = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(CSV_PATH)
        .expect("无法打开 CSV 日志文件");
    use std::io::Write;
    if std::fs::metadata(CSV_PATH)
        .map(|m| m.len() == 0)
        .unwrap_or(true)
    {
        writeln!(
            csv,
            "unix_ts,elapsed_min,level_pct,voltage_mv,adc_raw,charging"
        )
        .unwrap();
    }

    println!("已连接，进入待机观测。Ctrl-C 结束。");
    loop {
        // 先采样后等待：连接后立即得到第一行数据
        match device.battery().await {
            Ok(b) => {
                let unix_ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let elapsed_min = started.elapsed().map(|d| d.as_secs()).unwrap_or(0) as f64 / 60.0;
                println!(
                    "[t+{elapsed_min:7.1}min] level={}%, voltage={}mV, adc_raw={}, charging={}",
                    b.level, b.voltage, b.adc_raw, b.is_charging
                );
                writeln!(
                    csv,
                    "{unix_ts},{elapsed_min:.1},{},{},{},{}",
                    b.level, b.voltage, b.adc_raw, b.is_charging
                )
                .unwrap();
            }
            Err(error) => {
                eprintln!("电量查询失败（连接可能已断开）: {error}");
                eprintln!("待机测试结束。CSV 日志见 {CSV_PATH}");
                std::process::exit(1);
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}
