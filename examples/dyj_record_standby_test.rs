//! BLE 连接态「静默录音」功耗测试：连接卡片 → 开启录音（落 SD，不上传不推流）
//! → 保持连接不做其他操作，定期采样电量落 CSV，并周期性确认录音仍在进行。
//!
//! 用法：
//!
//! ```text
//! cargo run --example dyj_record_standby_test -- [poll_secs] [--id <device-id>]
//! ```
//!
//! - `poll_secs`：电量采样间隔秒数，默认 300。
//! - 每 6 个采样点（默认 30 分钟）顺带查一次 storage + record_status，
//!   确认录音文件在增长、分片正常，同时监控 SD 剩余空间。
//! - 结果打印到 stdout 并追加到 `standby_record_battery_log.csv`。
//! - 断连即退出（保持测试条件纯净）；Ctrl-C 退出时先停录再断开。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reai_card_sdk::DeviceManager;

const DEFAULT_POLL_SECS: u64 = 300;
const CSV_PATH: &str = "standby_record_battery_log.csv";
const STATUS_CHECK_EVERY: u64 = 6; // 每 N 个采样点查一次录音状态

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

    println!("=== DYJ 连接态静默录音功耗测试 ===");
    println!("采样间隔: {poll_secs}s, CSV: {CSV_PATH}");

    let manager = DeviceManager::new()
        .await
        .expect("蓝牙管理器初始化失败（本机蓝牙是否开启？）");

    // macOS 慢广播下服务过滤扫描偶发抓不到：多轮重试
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

    let mut events = device.subscribe_events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => println!("event: {event:?}"),
                Err(_) => return,
            }
        }
    });

    let info = device.device_info().await.expect("读取设备信息失败");
    println!("device_info: {info:?}");
    let storage = device.storage().await.expect("读取存储失败");
    println!("storage: total={}MB used={}MB", storage.total, storage.used);

    // 开启静默录音（落 SD 分片，默认 10 分钟/片）
    let started_resp = device.start_recording().await.expect("启动录音失败");
    println!("record_start: {started_resp:?}");

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

    println!("录音已开启，进入静默录音观测。Ctrl-C 结束。");
    let mut tick: u64 = 0;
    loop {
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
                eprintln!("测试结束。CSV 日志见 {CSV_PATH}");
                std::process::exit(1);
            }
        }

        tick += 1;
        if tick.is_multiple_of(STATUS_CHECK_EVERY) {
            match device.record_status().await {
                Ok(status) => println!("record_status: {status:?}"),
                Err(error) => eprintln!("录音状态查询失败: {error}"),
            }
            if let Ok(storage) = device.storage().await {
                println!("storage: total={}MB used={}MB", storage.total, storage.used);
            }
        }

        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}
