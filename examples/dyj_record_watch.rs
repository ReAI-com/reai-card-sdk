//! 录音自停现象受控观察：连上设备 → duration=60 开录 → 保持 BLE 连接 6 分钟，
//! 订阅全部事件推送（能抓到 RECORD_STATUS error 帧）+ 每 20s 轮询状态，
//! 观察"录音 3-4 分钟后自己停"是否复现、何时停、带什么错误码。
//!
//! cargo run --example dyj_record_watch

use std::time::Duration;

use reai_card_sdk::{DeviceEvent, DeviceManager};

#[path = "common/mod.rs"]
mod common;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::init_tracing(1); // debug 级，多打日志
    let manager = DeviceManager::new().await?;
    let devices = manager.scan(Duration::from_secs(5)).await?;
    let found = devices
        .first()
        .ok_or_else(|| "扫描不到 DYJ 设备".to_string())?;
    println!("🔗 连接 {} …", found.name);
    let device = manager.connect(found).await?;

    // 事件订阅：所有设备主动推送全打出来（重点盯 RecordStatus 带 error/reason）
    let mut events = device.subscribe_events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => match event {
                    DeviceEvent::RecordStatus(s) => {
                        println!("🔔 RECORD_STATUS 推送: {s:?}");
                    }
                    other => println!("🔔 事件 {other:?}"),
                },
                Err(e) => {
                    println!("🔔 事件通道关闭: {e}");
                    break;
                }
            }
        }
    });

    println!("▶ start_recording_with_duration(60)");
    let resp = device.start_recording_with_duration(60).await?;
    println!("  响应: {}", serde_json::to_string(&resp)?);

    // 轮询 6 分钟
    for tick in 1..=18 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let status = device.record_status().await?;
        println!(
            "⏱ t={}s isRecording={} session={:?}",
            tick * 20,
            status.is_recording,
            status.session
        );
        if !status.is_recording {
            println!("‼️ 录音已不在进行——自停复现，错误码见上方推送");
            break;
        }
    }

    // 若仍在录，取一次最旧文件再停
    if device.record_status().await?.is_recording {
        println!("\n📄 录音中 take_oldest…");
        match device.take_oldest_file_to_dir("downloads-segment-test").await {
            Ok(saved) => println!(
                "  取到 {:?}（{} 字节）→ {}",
                saved.device_name,
                saved.bytes_written,
                saved.path.display()
            ),
            Err(e) => println!("  取文件失败: {e}"),
        }
        device.stop_recording().await?;
        println!("⏹ 已停止录音");
    }
    Ok(())
}
