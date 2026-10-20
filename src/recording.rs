//! 录音文件落盘：按固件时间戳命名、`.part` 原子改名、ADPCM → WAV 转换。
//!
//! BLE 取走（[`crate::DyjDevice::take_file_to_dir`]）与 AP 快传下载
//! （[`crate::AccessPointClient::download_to_dir`]）都走这里，保证两条通道落盘行为一致。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone};

use crate::{
    Result,
    audio::{AdpcmDecoder, SAMPLE_RATE_HZ, wav_bytes},
    types::RecordingStamp,
};

/// 落盘文件名及其时间信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFileName {
    pub file_name: String,
    /// 固件名里解析出的时间戳；原名（设备上没有同名 `.txt`）时为 `None`。
    pub stamp: Option<RecordingStamp>,
    /// 仅当时钟已同步（`synced`）才有：录音开始的本地时间。
    pub recorded_at: Option<DateTime<Local>>,
}

/// 根据固件给的时间戳名决定落盘文件名：
///
/// - 时钟已同步：`YYYYMMDD-HHMMSS_<原编号>.apm`（本地时间，一眼看出录音时间）
/// - 时钟未同步（`nosync`）：`nosync-<ts>_<原编号>.apm`（该数不是真实日期，保留原样并标出）
/// - 没有时间戳（设备上无同名 `.txt`，固件退回原名）：原名
///
/// 保留原编号是为了和设备文件列表对应（删除/再次取走时要用）。
pub fn recording_file_name(original: &str, firmware_name: Option<&str>) -> RecordingFileName {
    let original = original.rsplit('/').next().unwrap_or(original);
    let (stem, ext) = original
        .rsplit_once('.')
        .map(|(stem, ext)| (stem, ext.to_ascii_lowercase()))
        .unwrap_or((original, "apm".to_owned()));
    let Some(stamp) = firmware_name.and_then(RecordingStamp::parse) else {
        return RecordingFileName {
            file_name: original.to_owned(),
            stamp: None,
            recorded_at: None,
        };
    };
    if stamp.synced
        && let Some(when) = Local.timestamp_opt(stamp.timestamp, 0).single()
    {
        return RecordingFileName {
            file_name: format!("{}_{stem}.{ext}", when.format("%Y%m%d-%H%M%S")),
            stamp: Some(stamp),
            recorded_at: Some(when),
        };
    }
    RecordingFileName {
        file_name: format!("nosync-{}_{stem}.{ext}", stamp.timestamp),
        stamp: Some(stamp),
        recorded_at: None,
    }
}

/// 已落盘的录音文件。
#[derive(Debug, Clone)]
pub struct SavedRecording {
    /// 最终路径（已按录音时间命名）。
    pub path: PathBuf,
    pub bytes_written: u64,
    /// 请求时的设备文件名（如 `059F19DF.APM`）。
    pub original_name: String,
    /// 固件给的名字：BLE start 帧 `name` / AP `Content-Disposition`，可能就是原名。
    pub device_name: Option<String>,
    pub stamp: Option<RecordingStamp>,
    pub recorded_at: Option<DateTime<Local>>,
}

impl SavedRecording {
    /// 与 `.apm` 同目录同名的 `.wav` 路径。
    pub fn wav_path(&self) -> PathBuf {
        self.path.with_extension("wav")
    }

    /// 把这份录音（IMA ADPCM）解码成 16kHz 单声道 WAV，放在 `.apm` 旁边。
    pub async fn to_wav(&self) -> Result<WavFile> {
        apm_to_wav(&self.path, &self.wav_path()).await
    }
}

/// 已生成的 WAV 文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavFile {
    pub path: PathBuf,
    pub samples: usize,
}

impl WavFile {
    pub fn duration_secs(&self) -> f64 {
        self.samples as f64 / SAMPLE_RATE_HZ as f64
    }
}

/// 离线转换：固件 `.apm`（裸 IMA ADPCM 流）→ 16kHz/16bit 单声道 WAV。
pub async fn apm_to_wav(apm: &Path, wav: &Path) -> Result<WavFile> {
    let adpcm = tokio::fs::read(apm).await?;
    let samples = AdpcmDecoder::new().decode(&adpcm);
    tokio::fs::write(wav, wav_bytes(&samples, SAMPLE_RATE_HZ)).await?;
    let file = WavFile {
        path: wav.to_path_buf(),
        samples: samples.len(),
    };
    tracing::info!(
        target: "dyj.file",
        "🎵 已转 WAV → {}（{:.2}s @16kHz）",
        wav.display(),
        file.duration_secs()
    );
    Ok(file)
}

/// 落盘用的临时文件路径：`<dir>/<original>.part`。
pub(crate) fn part_path(dir: &Path, original: &str) -> PathBuf {
    dir.join(format!("{original}.part"))
}

/// 下载/取走成功后：`.part` 按固件名改成最终文件名。
pub(crate) async fn finalize(
    dir: &Path,
    part: &Path,
    original: &str,
    device_name: Option<&str>,
    bytes_written: u64,
) -> Result<SavedRecording> {
    let naming = recording_file_name(original, device_name);
    let path = dir.join(&naming.file_name);
    tokio::fs::rename(part, &path).await?;
    tracing::info!(
        target: "dyj.file",
        "💾 已保存 → {}（{bytes_written} 字节{}）",
        path.display(),
        match (&naming.recorded_at, naming.stamp) {
            (Some(at), _) => format!("，录音时间 {}", at.format("%Y-%m-%d %H:%M:%S")),
            (None, Some(stamp)) if !stamp.synced => "，录音时设备时钟未同步".to_owned(),
            _ => String::new(),
        }
    );
    Ok(SavedRecording {
        path,
        bytes_written,
        original_name: original.to_owned(),
        device_name: device_name.map(ToOwned::to_owned),
        stamp: naming.stamp,
        recorded_at: naming.recorded_at,
    })
}

/// 失败时清掉半截 `.part`，不留垃圾。
pub(crate) async fn discard_part(part: &Path) {
    let _ = tokio::fs::remove_file(part).await;
}

/// 把 `dir/records/x.apm` 一类请求路径化成裸文件名。
pub(crate) fn base_name(path: &str) -> &str {
    path.trim_start_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::test_support::{AdpcmEncoder, sine};

    #[test]
    fn synced_names_use_local_time_and_keep_the_device_id() {
        let name = recording_file_name("059F19DF.APM", Some("synced_1792392529.apm"));
        // 本地时区不固定，断言结构：8 位日期-6 位时间_原编号.apm
        let (prefix, rest) = name.file_name.split_once('_').unwrap();
        assert_eq!(prefix.len(), 15);
        assert_eq!(&prefix[8..9], "-");
        assert!(prefix[..8].chars().all(|c| c.is_ascii_digit()));
        assert_eq!(rest, "059F19DF.apm");
        assert_eq!(name.stamp.unwrap().timestamp, 1792392529);
        assert!(name.recorded_at.is_some());
    }

    #[test]
    fn unsynced_clock_is_marked_instead_of_being_formatted_as_a_date() {
        let name = recording_file_name("/059F19DF.APM", Some("nosync_42.apm"));
        assert_eq!(name.file_name, "nosync-42_059F19DF.apm");
        assert!(name.recorded_at.is_none());
        assert!(!name.stamp.unwrap().synced);
    }

    #[test]
    fn raw_device_name_is_kept_when_no_timestamp_exists() {
        let name = recording_file_name("059F19DF.APM", Some("059F19DF.APM"));
        assert_eq!(name.file_name, "059F19DF.APM");
        assert!(name.stamp.is_none());
        assert_eq!(recording_file_name("x.apm", None).file_name, "x.apm");
    }

    #[tokio::test]
    async fn converts_apm_to_a_playable_wav() {
        let dir = std::env::temp_dir().join(format!(
            "reai-card-sdk-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let apm = dir.join("rec.apm");
        let encoded = AdpcmEncoder::new().encode_buffer(&sine(440.0, 1_600));
        tokio::fs::write(&apm, &encoded).await.unwrap();

        let wav = apm_to_wav(&apm, &apm.with_extension("wav")).await.unwrap();
        assert_eq!(wav.samples, 1_600);
        assert!((wav.duration_secs() - 0.1).abs() < 1e-9);
        let bytes = tokio::fs::read(&wav.path).await.unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 1_600 * 2);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
