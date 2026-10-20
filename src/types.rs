use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::controls::{ButtonEvent, ToggleSwitchEvent};

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

/// 录音状态变化的来源（`RECORD_STATUS` 推送的 `reason` 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(from = "String", into = "String")]
pub enum RecordReason {
    /// 上位机命令（`RECORD_START/STOP/RECORD`）。
    App,
    /// 按键绑定的动作。
    Button,
    /// 侧边开关绑定的动作。
    Switch,
    /// 设备自动行为，例如录满 10 分钟自动分段。
    Auto,
    /// 固件未能归类。
    Unknown,
    /// 未来固件新增的来源，原字符串保留。
    Other(String),
}

impl RecordReason {
    pub fn wire_name(&self) -> &str {
        match self {
            Self::App => "app",
            Self::Button => "button",
            Self::Switch => "switch",
            Self::Auto => "auto",
            Self::Unknown => "unknown",
            Self::Other(name) => name,
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "app" => Self::App,
            "button" => Self::Button,
            "switch" => Self::Switch,
            "auto" => Self::Auto,
            "unknown" => Self::Unknown,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl From<String> for RecordReason {
    fn from(text: String) -> Self {
        Self::parse(&text)
    }
}

impl From<RecordReason> for String {
    fn from(reason: RecordReason) -> Self {
        match reason {
            RecordReason::Other(name) => name,
            known => known.wire_name().to_owned(),
        }
    }
}

impl std::fmt::Display for RecordReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// 录音状态。既是 `RECORD_STATUS` 查询的响应，也是设备在录音开始/停止/出错时的推送。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RecordStatus {
    #[serde(rename = "isRecording")]
    pub is_recording: bool,
    #[serde(rename = "realtimeEnabled")]
    pub realtime_enabled: bool,
    /// 仅推送携带：这次状态变化是谁触发的。主动查询的响应里为 `None`。
    pub reason: Option<RecordReason>,
    /// 仅录音出错的推送携带：固件错误码（此时 `is_recording == false`）。
    pub error: Option<u32>,
}

/// `RECORD_MARK` 的结果：在当前录音文件的第 `offset_secs` 秒打了一个标记。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct RecordMark {
    /// 设备上正在写的录音文件名。
    pub filename: String,
    /// 标记相对录音开始的秒数（固件字段名 `timestamp`）。
    #[serde(alias = "timestamp")]
    pub offset_secs: u64,
}

/// `LED` 命令的目标状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedState {
    On,
    Off,
    Toggle,
}

impl LedState {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Toggle => "toggle",
        }
    }
}

impl std::fmt::Display for LedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl std::str::FromStr for LedState {
    type Err = crate::SdkError;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        match text {
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            "toggle" => Ok(Self::Toggle),
            other => Err(crate::SdkError::InvalidCommand(format!(
                "LED state must be on/off/toggle, got `{other}`"
            ))),
        }
    }
}

/// 电源管理通知。固件把它们当作**无命令封装**的裸 JSON 直接写到 BLE 上
/// （`{"type":"shutdown_imminent","seconds":5}`），SDK 解码器会单独识别。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PowerEvent {
    /// 设备将在 `seconds` 秒后关机——`SHUTDOWN` 命令响应之后紧跟着就是它，
    /// 随后 BLE 会断开；应用据此展示"设备正在关机"而不是报连接错误。
    ShutdownImminent { seconds: u32 },
    /// 从深度睡眠唤醒。
    DeviceWakeup,
    /// 开机流程完成。
    StartupComplete,
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

/// `WIFI_STATUS`。三个必填字段加 `serde(default)`：旧 WiFi 固件的响应可能缺字段。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct WifiStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
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

/// `GET /api/ota/check`：AP 模式下 ESP32 当前固件信息（设备无外网，不含最新版本）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccessPointOtaStatus {
    pub current_version: String,
    pub current_version_code: u32,
    pub chip_model: String,
    pub free_heap: u64,
    pub ota_ready: bool,
}

/// `POST /api/ota/upload` 的响应。成功时 ESP32 在回复后 3 秒自行重启（热点随之重启）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AccessPointOtaUpload {
    pub success: bool,
    pub message: Option<String>,
    /// 写入的固件字节数。
    pub size: Option<u64>,
    pub restart: Option<bool>,
    pub error: Option<String>,
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

/// `OTA_WIFI_CHECK` 的结果：ESP32 向 `wifi_api_base_url` + `wifi_ota_info_path`
/// 拉取版本信息后，告知是否有新固件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WifiOtaCheck {
    pub update_available: bool,
}

/// [`crate::DyjDevice::check_and_update_wifi_firmware`] 的结果。
#[derive(Debug, Clone)]
pub enum WifiOnlineOtaOutcome {
    /// 设备已是最新（服务器 versionCode 不高于设备），未发起升级。
    UpToDate,
    /// 升级已走到终态：`Success` 后 ESP32 自行重启进新固件。
    Finished(OtaProgress),
}

/// WiFi 模块 OTA 状态机（`OTA_WIFI_STATUS` 响应与 `OTA_WIFI_PROCESS` 推送共用）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, Default)]
#[serde(from = "String", into = "String")]
pub enum OtaStatus {
    #[default]
    Idle,
    Checking,
    UpdateAvailable,
    Downloading,
    /// 下载并写入成功；ESP32 随即自行重启（AP 会重新启动）。
    Success,
    Failed,
    /// 未来固件新增的状态，原字符串保留。
    Other(String),
}

impl OtaStatus {
    pub fn wire_name(&self) -> &str {
        match self {
            Self::Idle => "IDLE",
            Self::Checking => "CHECKING",
            Self::UpdateAvailable => "UPDATE_AVAILABLE",
            Self::Downloading => "DOWNLOADING",
            Self::Success => "SUCCESS",
            Self::Failed => "FAILED",
            Self::Other(name) => name,
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "IDLE" => Self::Idle,
            "CHECKING" => Self::Checking,
            "UPDATE_AVAILABLE" => Self::UpdateAvailable,
            "DOWNLOADING" => Self::Downloading,
            "SUCCESS" => Self::Success,
            "FAILED" => Self::Failed,
            other => Self::Other(other.to_owned()),
        }
    }

    /// 升级流程是否已结束（成功或失败）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Success | Self::Failed)
    }
}

impl From<String> for OtaStatus {
    fn from(text: String) -> Self {
        Self::parse(&text)
    }
}

impl From<OtaStatus> for String {
    fn from(status: OtaStatus) -> Self {
        match status {
            OtaStatus::Other(name) => name,
            known => known.wire_name().to_owned(),
        }
    }
}

impl std::fmt::Display for OtaStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// OTA 进度。下载阶段 `progress`/`total` 为字节数，`percentage` 0–100；
/// 固件每满 5% 推一条 `OTA_WIFI_PROCESS`。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct OtaProgress {
    pub status: OtaStatus,
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

/// 设备主动上报的事件，经 [`crate::DyjDevice::subscribe_events`] 广播。
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// 按键完成了一个手势（`BTN_CLICK`）。
    Button(ButtonEvent),
    /// 侧边开关被拨动（`TOGGLE_SWITCH`）。
    ToggleSwitch(ToggleSwitchEvent),
    /// 录音开始/停止/出错（`RECORD_STATUS` 推送，带 `reason`）。
    /// 主动查询 `record_status()` 的响应同样会经此广播，但没有 `reason`。
    RecordStatus(RecordStatus),
    WifiNetwork(WifiNetwork),
    WifiOff(CommandResponse),
    UploadComplete(CommandResponse),
    WifiOtaProgress(OtaProgress),
    AccessPointReady(CommandResponse),
    /// 电源管理通知（关机倒计时等），见 [`PowerEvent`]。
    Power(PowerEvent),
    /// 认得命令名但载荷无法按已知结构解析（如未来固件新增的按键手势），
    /// 或无命令封装且 `type` 未知的裸 JSON——后者 `command` 为空字符串。
    Unhandled {
        command: String,
        payload: Value,
    },
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
        } else {
            (false, stem.strip_prefix("nosync_")?)
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

#[cfg(test)]
mod ota_status_tests {
    use super::OtaStatus;

    #[test]
    fn parses_firmware_vocabulary_and_keeps_unknown_values() {
        assert_eq!(OtaStatus::parse("DOWNLOADING"), OtaStatus::Downloading);
        assert_eq!(
            OtaStatus::parse("UPDATE_AVAILABLE"),
            OtaStatus::UpdateAvailable
        );
        assert_eq!(
            OtaStatus::parse("VERIFYING"),
            OtaStatus::Other("VERIFYING".into())
        );
        assert!(OtaStatus::Success.is_terminal());
        assert!(OtaStatus::Failed.is_terminal());
        assert!(!OtaStatus::Downloading.is_terminal());
        assert_eq!(
            serde_json::from_str::<OtaStatus>("\"FAILED\"").unwrap(),
            OtaStatus::Failed
        );
        assert_eq!(
            serde_json::to_string(&OtaStatus::Checking).unwrap(),
            "\"CHECKING\""
        );
    }
}
