//! end 帧丢失诊断探针：连接 → take_oldest（预期超时）→ 同一连接内继续探测设备是否存活。
//!
//! cargo run --example dyj_take_probe

use std::time::Duration;

use reai_card_sdk::{DeviceManager, SdkError};

#[path = "common/mod.rs"]
mod common;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::init_tracing(1);
    let manager = DeviceManager::new().await?;
    let devices = manager.scan(Duration::from_secs(5)).await?;
    let found = devices
        .first()
        .ok_or_else(|| "扫描不到 DYJ 设备".to_string())?;
    println!("🔗 连接 {} …", found.name);
    let device = manager.connect(found).await?;

    println!("\n[1] take_oldest（预期可能 30s 超时）…");
    let saved = match device.take_oldest_file_to_dir("downloads-segment-test").await {
        Ok(s) => {
            println!("    ✅ 完成 {:?} {}字节 → {}", s.device_name, s.bytes_written, s.path.display());
            return Ok(());
        }
        Err(SdkError::Timeout { command, seconds }) => {
            println!("    ⏱ 超时 cmd={command} {seconds}s");
        }
        Err(e) => {
            println!("    ❌ 其他错误: {e}");
            return Ok(());
        }
    };

    println!("\n[2] 超时后同一连接内 device_info（链路是否存活）…");
    match tokio::time::timeout(Duration::from_secs(10), device.device_info()).await {
        Ok(Ok(info)) => println!("    ✅ 链路存活 version={} code={}", info.version, info.version_code),
        Ok(Err(e)) => println!("    ❌ 链路异常: {e}"),
        Err(_) => println!("    ❌ device_info 也超时——设备端主循环卡死？"),
    }

    println!("\n[3] list_files（设备文件系统是否响应）…");
    match tokio::time::timeout(Duration::from_secs(10), device.list_files(None)).await {
        Ok(Ok(files)) => {
            println!("    ✅ {} 个条目", files.len());
        }
        Ok(Err(e)) => println!("    ❌ list_files 异常: {e}"),
        Err(_) => println!("    ❌ list_files 超时"),
    }

    println!("\n[4] 再等 15s 看迟到的 end 帧/设备是否自行恢复（只观察事件流）…");
    tokio::time::sleep(Duration::from_secs(15)).await;

    println!("\n[5] 复测 record_status（同连接）…");
    match tokio::time::timeout(Duration::from_secs(10), device.record_status()).await {
        Ok(Ok(s)) => println!("    ✅ isRecording={}", s.is_recording),
        Ok(Err(e)) => println!("    ❌ {e}"),
        Err(_) => println!("    ❌ 超时——设备端已无法响应"),
    }
    Ok(())
}
