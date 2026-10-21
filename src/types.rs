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
    /// 本次录音的会话编号（主控 0.3.8 起 started/stopped 推送携带）。
    /// 按键/开关触发录音时，App 靠它拿到设备生成的会话编号；旧固件为 `None`。
    pub session: Option<String>,
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

/// 电源管理通知。0.3.7 前固件把它们当**无命令封装**的裸 JSON 直接写到 BLE 上
/// （`{"type":"shutdown_imminent","seconds":5}`）；0.3.7 起按标准事件框架发送
/// （`POWER:{json}`）。SDK 同时接受两种形态，映射成同一个事件。
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
    /// 充电状态变化（0.3.7 新增：插入/拔出时推送，`level` 为当时电量百分比）。
    Charging { charging: bool, level: Option<u8> },
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FileEntry {
    pub index: i32,
    pub name: Option<String>,
    pub size: Option<u64>,
    #[serde(rename = "is_dir")]
    pub is_dir: Option<u8>,
    pub created_at: Option<i64>,
    /// 录音会话编号（8 位大写十六进制；主控 0.3.8 起，`.apm` 条目带同名 `.txt` 时携带）。
    /// 旧固件 / 旧文件为 `None`，调用方按时间启发式兜底。
    pub session: Option<String>,
    /// 该会话内的段序号，从 1 递增（每 10 分钟自动切片、暂停恢复都会递增；手动停止再开始是新会话）。
    pub seq: Option<u32>,
    /// 段开始时间的 Unix 秒（来自同名 `.txt` 的 `timestamp`）。`created_at` 是 FAT 修改时间
    ///（`.apm` 即切片结束时间），需要开始时间时用这个。
    pub start_at: Option<i64>,
    /// 录音时设备时钟是否已同步（与 `start_at` 同时携带；`false` 时时间不是真实日期）。
    pub synced: Option<bool>,
}

impl FileEntry {
    /// 本条目的录音会话标记 `(session, seq)`；旧固件或旧文件（无标记）返回 `None`。
    pub fn recording_marker(&self) -> Option<(&str, u32)> {
        Some((self.session.as_deref()?, self.seq?))
    }

    /// 派生的录音显示名，与固件上传/下载时生成的名字同规则：
    /// `synced_<unix>_<会话>_<段序号2位>.apm`（无会话标记时退回 `synced_<unix>.apm`）。
    /// 列表里的 [`FileEntry::name`] 始终是卡上原始名（上传/删除请求用），
    /// 展示用这个；旧文件（无 `start_at`/`synced`）返回 `None`。
    pub fn recording_name(&self) -> Option<String> {
        Some(generated_recording_name(
            self.synced?,
            self.start_at?,
            self.session.as_deref(),
            self.seq,
        ))
    }
}

/// 按固件同款规则拼出录音生成名：`<synced|nosync>_<unix秒>.apm`，
/// 带会话标记时 `<前缀>_<unix秒>_<会话>_<段序号2位>.apm`（段序号补零 2 位便于排序）。
pub fn generated_recording_name(
    synced: bool,
    timestamp: i64,
    session: Option<&str>,
    seq: Option<u32>,
) -> String {
    let prefix = if synced { "synced" } else { "nosync" };
    match (session, seq) {
        (Some(session), Some(seq)) => {
            format!("{prefix}_{timestamp}_{session}_{seq:02}.apm")
        }
        _ => format!("{prefix}_{timestamp}.apm"),
    }
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
    /// 模块固件实时版本（WiFi 0.1.0 起 `WIFI_STATUS` 自带；旧固件为 `None`，
    /// 那时只能看 `BASE_INFO.wifiVersion`——那是主控的缓存）。
    pub version: Option<String>,
    #[serde(default, rename = "versionCode")]
    pub version_code: Option<u32>,
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
    /// 录音会话编号（语义同 [`FileEntry::session`]；WiFi 0.1.2 起 `/api/files` 携带）。
    pub session: Option<String>,
    /// 该会话内的段序号（语义同 [`FileEntry::seq`]）。
    pub seq: Option<u32>,
    /// 段开始时间的 Unix 秒（来自同名 `.txt`；仅当条目带标记时有意义）。
    #[serde(rename = "startAt")]
    pub start_at: Option<i64>,
    /// 录音时设备时钟是否已同步（WiFi 0.1.2 起 `/api/files` 携带）。
    pub synced: Option<bool>,
}

impl AccessPointFileEntry {
    /// 本条目的录音会话标记 `(session, seq)`；旧固件或旧文件（无标记）返回 `None`。
    pub fn recording_marker(&self) -> Option<(&str, u32)> {
        Some((self.session.as_deref()?, self.seq?))
    }

    /// 派生的录音显示名，规则与语义同 [`FileEntry::recording_name`]。
    pub fn recording_name(&self) -> Option<String> {
        Some(generated_recording_name(
            self.synced?,
            self.start_at?,
            self.session.as_deref(),
            self.seq,
        ))
    }
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
    /// 设备侧对写入数据计算的 sha256（WiFi 0.1.1 起；与本地固件哈希对账）。
    pub sha256: Option<String>,
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

/// `OTA_WIFI_CHECK` 的结果：ESP32 向 `api_url` + `ota_path`
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
///
/// 主控 0.3.8 起，`.txt` 带会话标记时名字扩展为
/// `synced_<unix秒>_<会话8hex>_<段序号>.apm`；[`RecordingStamp::parse`] 忽略尾部标记仍然可用，
/// 完整解析见 [`RecordingSegmentName`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingStamp {
    pub timestamp: i64,
    pub synced: bool,
}

/// 拆出 `synced_` / `nosync_` 前缀后的时间戳与剩余尾部（如 `A1B2C3D4_02`）。
fn split_recording_name(name: &str) -> Option<(bool, i64, &str)> {
    let stem = name.rsplit('/').next().unwrap_or(name);
    let stem = stem
        .strip_suffix(".apm")
        .or_else(|| stem.strip_suffix(".APM"))
        .unwrap_or(stem);
    let (synced, rest) = if let Some(rest) = stem.strip_prefix("synced_") {
        (true, rest)
    } else {
        (false, stem.strip_prefix("nosync_")?)
    };
    let digits_end = rest
        .find('_')
        .unwrap_or(rest.len());
    let timestamp = rest[..digits_end].parse::<i64>().ok()?;
    Some((synced, timestamp, &rest[digits_end..]))
}

impl RecordingStamp {
    /// 从固件文件名解析；不符合规则（如原始 `XXXXXXXX.APM`）返回 `None`。
    /// 带会话标记的扩展名也能解析（尾部标记被忽略，完整解析用 [`RecordingSegmentName::parse`]）。
    pub fn parse(name: &str) -> Option<Self> {
        let (synced, timestamp, _) = split_recording_name(name)?;
        Some(Self { timestamp, synced })
    }
}

/// 带会话标记的录音名解析结果：`(session, seq)` + 基础时间戳。
///
/// 兼容两种格式：
/// - `synced_<unix秒>.apm`（旧固件）→ `session`/`seq` 为 `None`
/// - `synced_<unix秒>_<会话8hex>_<段序号>.apm`（主控 0.3.8 / WiFi 0.1.2 起）
///
/// 尾部不符合标记格式时同样降级为 `None`（不整体失败），便于对旧文件宽松处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSegmentName {
    pub stamp: RecordingStamp,
    pub session: Option<String>,
    pub seq: Option<u32>,
}

impl RecordingSegmentName {
    pub fn parse(name: &str) -> Option<Self> {
        let (synced, timestamp, tail) = split_recording_name(name)?;
        let stamp = RecordingStamp { timestamp, synced };

        let Some(tail) = tail.strip_prefix('_') else {
            // 旧格式没有尾部标记
            return Some(Self { stamp, session: None, seq: None });
        };
        let mut parts = tail.splitn(2, '_');
        let session = parts.next()?;
        let seq = parts.next();

        // 会话编号：8 位十六进制（大小写均可，规范输出为大写）
        if session.len() != 8 || !session.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(Self { stamp, session: None, seq: None });
        }
        let seq = match seq {
            Some(seq) if !seq.is_empty() => match seq.parse::<u32>() {
                Ok(seq) => Some(seq),
                Err(_) => return Some(Self { stamp, session: None, seq: None }),
            },
            _ => None,
        };
        Some(Self {
            stamp,
            session: Some(session.to_ascii_uppercase()),
            seq,
        })
    }
}

/// 校验会话编号格式：恰好 8 位十六进制（发送给固件前使用；固件对非法值直接拒绝）。
pub fn is_valid_session_id(session: &str) -> bool {
    session.len() == 8 && session.chars().all(|c| c.is_ascii_hexdigit())
}

/// 生成一个新的会话编号（8 位大写十六进制，取自 UUID v4）。
///
/// 用 [`crate::DyjDevice::start_recording_with_session`] 发起录音时由调用方生成传入；
/// 不传则固件自行随机生成，实际值以后续 `RECORD_STATUS` 推送为准。
pub fn new_session_id() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    format!("{:08X}", u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod recording_stamp_tests {
    use super::{
        AccessPointFileEntry, FileEntry, RecordingSegmentName, RecordingStamp, is_valid_session_id,
        new_session_id,
    };

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

    #[test]
    fn parses_extended_segment_names() {
        // 新格式：synced_<ts>_<session8>_<seq>.apm（主控 0.3.8 起）
        let name = RecordingSegmentName::parse("synced_1792392529_A1B2C3D4_02.apm").unwrap();
        assert_eq!(name.stamp.timestamp, 1792392529);
        assert!(name.stamp.synced);
        assert_eq!(name.session.as_deref(), Some("A1B2C3D4"));
        assert_eq!(name.seq, Some(2));

        // 小写会话编号规范成大写
        let name = RecordingSegmentName::parse("/nosync_42_a1b2c3d4_1.apm").unwrap();
        assert!(!name.stamp.synced);
        assert_eq!(name.session.as_deref(), Some("A1B2C3D4"));
        assert_eq!(name.seq, Some(1));

        // 旧格式：session/seq 为 None，但整体解析成功
        let name = RecordingSegmentName::parse("synced_1792392529.apm").unwrap();
        assert_eq!(name.session, None);
        assert_eq!(name.seq, None);

        // 尾部不是合法标记：降级为 None 而不是失败
        let name = RecordingSegmentName::parse("synced_1792392529_hello.apm").unwrap();
        assert_eq!(name.session, None);
        assert_eq!(name.seq, None);

        // 原始设备名仍然整体失败
        assert_eq!(RecordingSegmentName::parse("059F19DF.APM"), None);
    }

    #[test]
    fn file_entry_marker_requires_both_fields() {
        let entry: FileEntry =
            serde_json::from_str(r#"{"index":0,"name":"A1B2C3D4.APM","session":"A1B2C3D4","seq":3}"#)
                .unwrap();
        assert_eq!(entry.recording_marker(), Some(("A1B2C3D4", 3)));

        // 旧固件条目（无新字段）→ None；start_at 缺省也为 None
        let entry: FileEntry =
            serde_json::from_str(r#"{"index":0,"name":"059F19DF.APM","size":10,"is_dir":0}"#)
                .unwrap();
        assert_eq!(entry.recording_marker(), None);
        assert_eq!(entry.session, None);
        assert_eq!(entry.start_at, None);
    }

    #[test]
    fn recording_name_matches_firmware_rule() {
        // 完整条目 → 与固件上传名一致
        let entry: FileEntry = serde_json::from_str(
            r#"{"index":0,"name":"A1B2C3D4.APM","start_at":1792392529,"synced":true,"session":"A1B2C3D4","seq":1}"#,
        )
        .unwrap();
        assert_eq!(
            entry.recording_name().as_deref(),
            Some("synced_1792392529_A1B2C3D4_01.apm")
        );

        // 未同步时钟 → nosync 前缀；seq 补零 2 位
        let entry: FileEntry = serde_json::from_str(
            r#"{"index":1,"name":"059F19DF.APM","start_at":42,"synced":false,"session":"059F19DF","seq":12}"#,
        )
        .unwrap();
        assert_eq!(
            entry.recording_name().as_deref(),
            Some("nosync_42_059F19DF_12.apm")
        );

        // 旧 txt（有时间戳无会话标记）→ 旧格式名
        let entry: FileEntry = serde_json::from_str(
            r#"{"index":2,"name":"059F19DF.APM","start_at":1792392529,"synced":true}"#,
        )
        .unwrap();
        assert_eq!(
            entry.recording_name().as_deref(),
            Some("synced_1792392529.apm")
        );

        // 缺 synced / start_at → None（不给猜测的前缀）
        let entry: FileEntry = serde_json::from_str(
            r#"{"index":3,"name":"059F19DF.APM","start_at":42,"session":"059F19DF","seq":1}"#,
        )
        .unwrap();
        assert_eq!(entry.recording_name(), None);

        // AP 条目同样可派生
        let ap: AccessPointFileEntry = serde_json::from_str(
            r#"{"name":"A1B2C3D4.APM","size":1,"startAt":1792392529,"synced":true,"session":"A1B2C3D4","seq":3}"#,
        )
        .unwrap();
        assert_eq!(
            ap.recording_name().as_deref(),
            Some("synced_1792392529_A1B2C3D4_03.apm")
        );
    }

    #[test]
    fn session_id_helpers() {
        assert!(is_valid_session_id("A1B2C3D4"));
        assert!(is_valid_session_id("00000000"));
        assert!(!is_valid_session_id("A1B2C3D")); // 7 位
        assert!(!is_valid_session_id("A1B2C3D4X")); // 9 位
        assert!(!is_valid_session_id("A1B2C3D4 ")); // 含空格
        let id = new_session_id();
        assert!(is_valid_session_id(&id), "生成的编号应可通过校验: {id}");
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
