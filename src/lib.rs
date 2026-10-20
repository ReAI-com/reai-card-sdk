//! Async Bluetooth Low Energy SDK for DYJ recording devices.
//!
//! `DyjDevice` serializes device requests because the wire protocol has no request ID.
//! Subscribe to [`DyjDevice::subscribe_events`] for hardware-originated notifications
//! (button gestures, side-switch moves, recording state changes, power notices) and to
//! [`DyjDevice::subscribe_audio`] for decoded realtime PCM. Button/switch behaviour is
//! configured on the device through [`DyjDevice::set_control_action`] and friends.

pub mod access_point;
pub mod audio;
pub mod controls;
mod device;
mod protocol;
pub mod recording;
pub mod transport;
pub mod types;

pub use access_point::AccessPointClient;
pub use audio::{AdpcmDecoder, CHANNELS, PcmChunk, SAMPLE_RATE_HZ, wav_bytes};
pub use controls::{
    ButtonEvent, ButtonGesture, ControlAction, ControlBindings, ControlInput, SwitchPosition,
    ToggleSwitchEvent,
};
pub use device::{DyjDevice, RealtimeRecordingSession};
pub use recording::{RecordingFileName, SavedRecording, WavFile, apm_to_wav, recording_file_name};
pub use transport::{BleTransport, NotificationStream, SharedTransport};
pub use types::*;

#[cfg(feature = "btleplug")]
mod btleplug_transport;
#[cfg(feature = "btleplug")]
pub use btleplug_transport::DeviceManager;

use thiserror::Error;

pub const NUS_SERVICE_UUID: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
pub const NUS_WRITE_UUID: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
pub const NUS_NOTIFY_UUID: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";

/// 固件 SoftAP 的固定 IP（ap_manager 未做 softAPConfig，即 Arduino 默认网段）。
pub const AP_DEFAULT_IP: &str = "192.168.4.1";
/// 固件 Web 服务器的固定端口（config.h `WEB_SERVER_PORT`）。
pub const AP_DEFAULT_PORT: u16 = 80;
/// 固件默认热点名（config.h `AP_DEFAULT_SSID`），供 UI 提示用户加入。
///
/// 热点密码由设备配置决定，SDK 不内置（HTTP 文件接口无鉴权，密码即访问凭证）；
/// 应用请从自己的配置/设备文档获取后提示用户。
pub const AP_DEFAULT_SSID: &str = "DYJ-HD-AP";

pub type Result<T> = std::result::Result<T, SdkError>;

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("BLE transport error: {0}")]
    Transport(String),
    #[error("no usable Bluetooth adapter was found")]
    NoAdapter,
    #[error("device `{0}` is no longer discoverable")]
    DeviceNotFound(String),
    #[error("required NUS {0} characteristic was not found")]
    MissingCharacteristic(&'static str),
    #[error("invalid command `{0}`")]
    InvalidCommand(String),
    #[error("invalid protocol frame: {0}")]
    InvalidFrame(String),
    #[error("device sent invalid UTF-8")]
    InvalidUtf8,
    #[error("JSON protocol error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("timed out waiting for `{command}` after {seconds}s")]
    Timeout { command: String, seconds: u64 },
    #[error("device disconnected")]
    Disconnected,
    #[error("device rejected `{command}` (code {code}): {message}")]
    Device {
        command: String,
        code: i32,
        message: String,
    },
    #[error("protocol worker stopped: {0}")]
    Protocol(String),
    #[error("file transfer length mismatch: expected {expected} bytes, received {actual}")]
    FileLengthMismatch { expected: u64, actual: u64 },
    #[error("realtime recording is active; stop the session before taking files")]
    RealtimeRecordingActive,
    #[error("WiFi module is powered down; call `open_wifi` before `{command}`")]
    WifiModulePoweredDown { command: String },
    #[error(
        "SD card is owned by the WiFi module while it is powered; call `close_wifi` before `{command}`"
    )]
    SdOwnedByWifiModule { command: String },
    #[error("system time predates the Unix epoch")]
    InvalidSystemTime,
}
