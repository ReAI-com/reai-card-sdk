//! 真机回归：录音分片时长可配 + FILE_UPLOAD_OLDEST（主控 ≥0.3.13 / SDK 0.0.8）。
//!
//! ```text
//! cargo run --example dyj_segment_test            # 缺省取扫描到的第一台 DYJ 设备
//! cargo run --example dyj_segment_test -- --id <device-id>
//! ```
//!
//! 流程（全程 ~2.5 分钟，设备需开机且 SD 卡在 nRF 侧）：
//! 1. scan → connect → BASE_INFO（校验 versionCode ≥313）
//! 2. FILE_LIST 基线
//! 3. start_recording_with_duration(60)，校验响应 duration=60
//! 4. 等 70 秒 → FILE_LIST 应出现新会话的段 1（60s 切片）+ 段 2（在录）
//! 5. 录音中 take_oldest_file_to_dir → 取到的应是 mtime 最旧且非在录段的文件，取后 FILE_LIST 少一条
//! 6. record_status 应仍 isRecording=true（取文件不影响录音）
//! 7. 再等 65 秒 → FILE_LIST 应出现段 3；stop_recording 收尾
//! 8. 逐项打 ✅/❌ 汇总，全部通过退出码 0

use std::error::Error;
use std::time::Duration;

use reai_card_sdk::{DeviceManager, FileEntry};

#[path = "common/mod.rs"]
mod common;

/// 测试落盘目录（相对当前工作目录）。
const DOWNLOADS: &str = "downloads-segment-test";

fn fail<T>(msg: impl Into<String>) -> Result<T, Box<dyn Error>> {
    Err(msg.into().into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    common::init_tracing(0);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let id = args
        .windows(2)
        .find(|w| w[0] == "--id")
        .map(|w| w[1].clone());

    let manager = DeviceManager::new().await?;
    println!("📡 扫描 DYJ 设备（5s）…");
    let devices = manager.scan(Duration::from_secs(5)).await?;
    let found = match &id {
        Some(id) => devices
            .iter()
            .find(|d| &d.id == id)
            .ok_or_else(|| format!("未找到指定设备 {id}"))?,
        None => devices
            .first()
            .ok_or_else(|| "扫描不到任何 DYJ 设备，请确认设备开机")?,
    };
    println!("🔗 连接 {} ({}) …", found.name, found.id);
    let device = manager.connect(found).await?;

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut summary = Vec::new();
    macro_rules! check {
        ($name:expr, $cond:expr, $($detail:tt)*) => {{
            let detail = format!($($detail)*);
            if $cond {
                println!("  ✅ {} {}", $name, detail);
                summary.push(format!("✅ {}", $name));
                passed += 1;
            } else {
                println!("  ❌ {} {}", $name, detail);
                summary.push(format!("❌ {}", $name));
                failed += 1;
            }
        }};
    }

    // 1. 版本校验
    println!("\n[1/7] BASE_INFO 版本校验");
    let info = device.device_info().await?;
    let code = info.version_code;
    check!("versionCode ≥313", code >= 313, "(实际 {code})");
    if code < 313 {
        return fail(format!("固件 {code} 不支持新特性，先升级到 0.3.13"));
    }

    // 2. 文件基线
    println!("\n[2/7] FILE_LIST 基线");
    let before = device.list_files(None).await?;
    println!("  卡上现有 {} 个条目", before.len());

    // 3. duration=60 开始录音
    println!("\n[3/7] start_recording_with_duration(60)");
    let resp = device.start_recording_with_duration(60).await?;
    let echo = resp.extra.get("duration").and_then(|v| v.as_u64());
    check!("响应回显 duration=60", echo == Some(60), "(实际 {echo:?})");

    // 4. 等 70 秒看切片
    println!("\n[4/7] 等 70 秒验证 60s 分片…");
    tokio::time::sleep(Duration::from_secs(70)).await;
    let mid = device.list_files(None).await?;
    let added_mid = added(&before, &mid);
    let seg1 = added_mid.iter().find(|e| e.seq == Some(1)).cloned();
    let seg2 = added_mid.iter().find(|e| e.seq == Some(2)).cloned();
    check!(
        "新会话出现分段",
        !added_mid.is_empty(),
        "(新增 {} 条: {:?})",
        added_mid.len(),
        names(&added_mid)
    );
    check!(
        "60s 段 1 已落定",
        seg1.is_some(),
        "({:?})",
        seg1.as_ref().map(|e| (e.name.clone(), e.session.clone(), e.seq))
    );
    check!(
        "段 2 在录",
        seg2.is_some(),
        "({:?})",
        seg2.as_ref().map(|e| (e.name.clone(), e.session.clone(), e.seq))
    );

    // 5. 录音中取最旧
    println!("\n[5/7] 录音中 take_oldest_file_to_dir");
    // 预期取到的：卡上 mtime 最旧且不是段 2（在录段）的 .APM
    let expect = mid
        .iter()
        .filter(|e| e.is_dir != Some(1))
        .filter(|e| {
            !(seg2.as_ref()
                .is_some_and(|s2| e.session == s2.session && e.seq == s2.seq))
        })
        .min_by_key(|e| e.created_at.unwrap_or(i64::MAX))
        .and_then(|e| e.name.clone());
    println!("  预期取到: {expect:?}");
    // 预期条目的 (session, seq)，供与设备改名结果核对
    let expect_meta = expect.as_ref().and_then(|name| {
        mid.iter()
            .find(|e| e.name.as_deref() == Some(name.as_str()))
            .map(|e| (e.session.clone(), e.seq))
    });
    let saved = match device.take_oldest_file_to_dir(DOWNLOADS).await {
        Ok(saved) => saved,
        Err(e) => {
            check!("取最旧成功", false, "({e})");
            let _ = device.stop_recording().await;
            return fail(format!("take_oldest 失败（{e}），已停止录音，中止后续步骤"));
        }
    };
    println!(
        "  实际取到: {:?} → {}（{} 字节）",
        saved.device_name,
        saved.path.display(),
        saved.bytes_written
    );
    // 设备 start 帧名字是 synced_/nosync_<时间戳>_<会话>_<段序>.apm，卡上名字是随机
    // 8 位 hex——改名不含原名，按 (session, seq) 对应关系核对。
    let taken_meta = parse_device_name(&saved.device_name);
    check!(
        "取到预期最旧文件",
        taken_meta.is_some() && taken_meta == expect_meta,
        "(taken {taken_meta:?} vs expect {expect_meta:?})"
    );

    let after_take = device.list_files(None).await?;
    check!(
        "源文件已从卡上删除",
        expect
            .as_deref()
            .map(|want| !after_take
                .iter()
                .any(|e| e.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(want))))
            .unwrap_or(false),
        ""
    );

    // 6. 录音未受影响
    println!("\n[6/7] record_status 录音应仍在进行");
    let status = device.record_status().await?;
    check!("isRecording=true", status.is_recording, "({status:?})");

    // 7. 再等 65 秒验证切片仍在推进，收尾
    println!("\n[7/7] 再等 65 秒验证继续切片…");
    tokio::time::sleep(Duration::from_secs(65)).await;
    let late = device.list_files(None).await?;
    // 传输耗时不可控（一条 60s 段要传 ~2 分钟），切片序号早已滚过 3——改为核对
    // 在录会话的最大段序号比步骤 5 时至少前进 1。
    let session = seg2.as_ref().and_then(|e| e.session.clone());
    let max_seq_now = late
        .iter()
        .filter(|e| session.as_ref().is_some_and(|s| e.session.as_ref() == Some(s)))
        .filter_map(|e| e.seq)
        .max();
    let max_seq_take = mid
        .iter()
        .filter(|e| session.as_ref().is_some_and(|s| e.session.as_ref() == Some(s)))
        .filter_map(|e| e.seq)
        .max();
    check!(
        "录音期间持续切片",
        max_seq_now.unwrap_or(0) > max_seq_take.unwrap_or(0),
        "(取文件时最大段序 {max_seq_take:?} → 现在 {max_seq_now:?})"
    );
    device.stop_recording().await?;
    println!("  ⏹ 已停止录音");

    println!("\n========== 结果: {passed} 通过 / {failed} 失败 ==========");
    for line in &summary {
        println!("  {line}");
    }
    if failed > 0 {
        return fail(format!("{failed} 项未通过"));
    }
    Ok(())
}

/// 从设备改名结果 `synced_/nosync_<时间戳>_<会话>_<段序>.apm` 里提取 (session, seq)。
fn parse_device_name(name: &Option<String>) -> Option<(Option<String>, Option<u32>)> {
    let name = name.as_deref()?;
    let stem = name.strip_suffix(".apm").or_else(|| name.strip_suffix(".APM"))?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 4 {
        return None;
    }
    let session = parts.get(parts.len() - 2)?.to_string();
    let seq = parts.last()?.parse::<u32>().ok();
    Some((Some(session), seq))
}

/// `before` 之后新出现的条目（按名字差集）。
fn added(before: &[FileEntry], now: &[FileEntry]) -> Vec<FileEntry> {
    let old: Vec<_> = before.iter().filter_map(|e| e.name.clone()).collect();
    now.iter()
        .filter(|e| {
            let name = e.name.clone().unwrap_or_default();
            e.is_dir != Some(1) && !old.iter().any(|o| o.eq_ignore_ascii_case(&name))
        })
        .cloned()
        .collect()
}

fn names(entries: &[FileEntry]) -> Vec<String> {
    entries.iter().filter_map(|e| e.name.clone()).collect()
}
