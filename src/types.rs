use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DiscoveredDevice {
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub version: String,
    pub version_code: u32,
    pub device_id: String,
    pub device_name: String,
    pub timestamp: i64,
    pub wifi_version: Option<String>,
    pub wifi_version_code: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    pub auth_state: bool,
    pub record_realtime_enabled: bool,
    pub wifi_open: bool,
    pub boot_type: String,
    pub low_power_mode: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BatteryInfo {
    pub level: u8,
    #[serde(rename = "levelText")]
    pub level_text: String,
    #[serde(rename = "isCharging")]
    pub is_charging: bool,
    pub voltage: u32,
    pub adc_raw: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct StorageInfo {
    pub total: u32,
    pub used: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecordStatus {
    #[serde(rename = "isRecording")]
    pub is_recording: bool,
    #[serde(rename = "realtimeEnabled")]
    pub realtime_enabled: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FileEntry {
    pub index: i32,
    pub name: Option<String>,
    pub size: Option<u64>,
    #[serde(rename = "is_dir")]
    pub is_dir: Option<u8>,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WifiNetwork {
    pub index: i32,
    pub ssid: Option<String>,
    pub rssi: Option<i16>,
    pub encryption: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WifiStatus {
    pub status: String,
    pub enabled: bool,
    pub connected: bool,
    pub mode: Option<String>,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub rssi: Option<i16>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AccessPointInfo {
    pub status: Option<String>,
    pub active: Option<bool>,
    pub ssid: Option<String>,
    pub password: Option<String>,
    pub ip: Option<String>,
    pub clients: Option<u32>,
    pub uptime: Option<u64>,
    #[serde(rename = "webServerRunning")]
    pub web_server_running: Option<bool>,
    #[serde(rename = "webPort")]
    pub web_port: Option<u16>,
    pub code: Option<i32>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AccessPointFileEntry {
    pub name: String,
    pub size: u64,
    #[serde(rename = "sizeFormatted")]
    pub size_formatted: Option<String>,
    pub timestamp: Option<i64>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessPointFilePage {
    pub total: u32,
    pub page: u32,
    pub page_size: u32,
    pub total_pages: u32,
    pub files: Vec<AccessPointFileEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessPointDeviceInfo {
    pub version: String,
    pub version_code: u32,
    pub chip_model: String,
    pub free_heap: u64,
    pub uptime: u64,
    pub current_time: String,
    pub timestamp: i64,
    pub sd_card: AccessPointSdCardStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AccessPointSdCardStatus {
    pub available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPointDownloadedFile {
    pub requested_path: String,
    pub bytes_written: u64,
    pub content_type: Option<String>,
    /// 固件在 `Content-Disposition` 里给出的建议文件名（有同名 `.txt` 时为
    /// `synced_<timestamp>.apm` / `nosync_<timestamp>.apm`，否则为原名）。
    pub suggested_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ModuleCheck {
    pub key: String,
    pub result: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct OtaProgress {
    pub status: String,
    pub progress: u64,
    pub total: u64,
    pub percentage: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CommandResponse {
    pub code: Option<i32>,
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenFile {
    pub name: String,
    pub bytes_written: u64,
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Button(Value),
    ToggleSwitch(Value),
    RecordStatus(RecordStatus),
    WifiNetwork(WifiNetwork),
    WifiOff(CommandResponse),
    UploadComplete(CommandResponse),
    WifiOtaProgress(OtaProgress),
    AccessPointReady(CommandResponse),
    Unhandled { command: String, payload: Value },
}

/// 固件按同名 `.txt` 的时间戳给录音起的名字：`synced_<unix秒>.apm` / `nosync_<unix秒>.apm`。
///
/// BLE 取走时 `FILE_UPLOAD` 的 start 帧 `name`、AP 下载时 `Content-Disposition` 的
/// filename 都遵循该规则（`.txt` 缺失时固件退回原名，此时解析得到 `None`）。
/// `synced == false` 表示录音时设备时钟未同步，`timestamp` 不能当真实日期用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingStamp {
    pub timestamp: i64,
    pub synced: bool,
}

impl RecordingStamp {
    /// 从固件文件名解析；不符合规则（如原始 `XXXXXXXX.APM`）返回 `None`。
    pub fn parse(name: &str) -> Option<Self> {
        let stem = name.rsplit('/').next().unwrap_or(name);
        let stem = stem
            .strip_suffix(".apm")
            .or_else(|| stem.strip_suffix(".APM"))
            .unwrap_or(stem);
        let (synced, digits) = if let Some(rest) = stem.strip_prefix("synced_") {
            (true, rest)
        } else if let Some(rest) = stem.strip_prefix("nosync_") {
            (false, rest)
        } else {
            return None;
        };
        let timestamp = digits.parse::<i64>().ok()?;
        Some(Self { timestamp, synced })
    }
}

#[cfg(test)]
mod recording_stamp_tests {
    use super::RecordingStamp;

    #[test]
    fn parses_firmware_timestamp_names() {
        assert_eq!(
            RecordingStamp::parse("synced_1792392529.apm"),
            Some(RecordingStamp {
                timestamp: 1792392529,
                synced: true
            })
        );
        assert_eq!(
            RecordingStamp::parse("/nosync_42.APM"),
            Some(RecordingStamp {
                timestamp: 42,
                synced: false
            })
        );
    }

    #[test]
    fn rejects_raw_device_names() {
        assert_eq!(RecordingStamp::parse("059F19DF.APM"), None);
        assert_eq!(RecordingStamp::parse("synced_abc.apm"), None);
        assert_eq!(RecordingStamp::parse("recording.wav"), None);
    }
}
