use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Mutex, broadcast, mpsc, watch},
    time::{sleep, timeout},
};

use crate::{
    Result, SdkError,
    audio::{AdpcmDecoder, CHANNELS, PcmChunk, SAMPLE_RATE_HZ},
    controls::{ButtonEvent, ControlAction, ControlBindings, ControlInput, ToggleSwitchEvent},
    protocol::{Frame, FrameDecoder, TextFrame, encode_command},
    recording,
    recording::SavedRecording,
    transport::SharedTransport,
    types::{
        AccessPointInfo, BatteryInfo, CommandResponse, DeviceEvent, DeviceInfo, DeviceState,
        FileEntry, LedState, ModuleCheck, OtaProgress, PowerEvent, RecordMark, RecordStatus,
        StorageInfo, TakenFile, WifiNetwork, WifiOnlineOtaOutcome, WifiOtaCheck, WifiStatus,
    },
};

const NORMAL_TIMEOUT: Duration = Duration::from_secs(10);
/// `OTA_WIFI_CHECK` 让 ESP32 现场发一次 HTTPS GET（TLS 握手 1–3s）；旧固件连上路由后
/// 主循环还可能被其他阻塞网络操作占住，10s 不够。
const OTA_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// 固件 `CONFIG_GET` / `CONFIG_DEL` 对不存在的键统一回 `code 3 "not found"`。
const CONFIG_CODE_NOT_FOUND: i64 = 3;
// 固件兜底回滚恰好在 30s 时才回 `code:3`（handler_wifi.c WIFI_OPEN_TIMEOUT_MS），
// 且其注释明确要求上位机超时必须大于 30s，否则上位机先超时、拿不到结构化错误。
const WIFI_OPEN_TIMEOUT: Duration = Duration::from_secs(35);
// ESP32 的 WIFI_CONNECT 同步阻塞最坏 ~11.4s（1.4s 前置延时 + 10s 等待），
// 10s 普通超时会在连接慢/失败时伪超时。
const WIFI_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
// ESP32 的 UPLOAD_FILE 是同步的：整个文件经 HTTP 上传完成后才回响应。
// 该值只是上限，正常完成时立即返回。
const UPLOAD_FILE_TIMEOUT: Duration = Duration::from_secs(600);
const FILE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 文件传输 INFO 进度日志的间隔（字节）。
const FILE_PROGRESS_STEP: u64 = 64 * 1024;
const WRITE_CHUNK_SIZE: usize = 20;
const WRITE_CHUNK_DELAY: Duration = Duration::from_millis(10);
/// 实时音频广播容量：20ms 一块时约 10 秒的缓冲，订阅方短暂卡顿不至于丢块。
const AUDIO_CHANNEL_CAPACITY: usize = 512;
/// `access_point_client` 等待 WIFI_AP_START 的宽限：刚 WIFI_OPEN 时事件约 2s 内到达；
/// 超过它仍没来，多半是本次连接建立前模块就已供电（事件早已发过），按 AP 已自启处理。
const AP_READY_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug)]
enum Inbound {
    Text(TextFrame),
    Binary(Vec<u8>),
    Fault(String),
    Disconnected,
}

struct Core {
    transport: SharedTransport,
    incoming: Mutex<mpsc::UnboundedReceiver<Inbound>>,
    command_gate: Mutex<()>,
    events: broadcast::Sender<DeviceEvent>,
    /// 解码后的实时 PCM 广播。设备只在实时录音时发出二进制流，无订阅者时块被丢弃。
    audio: broadcast::Sender<PcmChunk>,
    /// 快传入口就绪信号（固件 `WIFI_AP_START` 事件驱动，WIFI_OPEN/CLOSE/WIFI_OFF 复位）。
    ap_ready: watch::Receiver<bool>,
    /// 就绪信号发送端：worker 按帧驱动；`open_wifi` 发现模块已上电时也要据此直接置位。
    ap_ready_tx: Arc<watch::Sender<bool>>,
    /// FILE_UPLOAD 传输期间为 true：二进制通知属于文件数据，不得当作音频广播。
    /// 与通知 worker 共享（worker 先于 Core 构造，因此用 Arc 包裹）。
    file_transfer_active: Arc<AtomicBool>,
    /// SDK 侧实时录音会话进行中：禁止 take_file 与实时流在同一 wire 上混流。
    realtime_active: AtomicBool,
}

/// 离开作用域时自动清除布尔标志。
struct FlagGuard<'a>(&'a AtomicBool);

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A connected DYJ device. Calls that issue protocol commands are deliberately serialized.
#[derive(Clone)]
pub struct DyjDevice {
    core: Arc<Core>,
}

impl DyjDevice {
    /// Creates a device around an already connected NUS transport and immediately synchronizes time.
    pub async fn from_transport(transport: SharedTransport) -> Result<Self> {
        let stream = transport.notifications().await?;
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let (audio, _) = broadcast::channel(AUDIO_CHANNEL_CAPACITY);
        let (ap_ready_tx, ap_ready_rx) = watch::channel(false);
        let ap_ready_tx = Arc::new(ap_ready_tx);
        let file_transfer_active = Arc::new(AtomicBool::new(false));
        spawn_notification_worker(
            stream,
            incoming_tx,
            events.clone(),
            audio.clone(),
            file_transfer_active.clone(),
            ap_ready_tx.clone(),
        );

        let device = Self {
            core: Arc::new(Core {
                transport,
                incoming: Mutex::new(incoming_rx),
                command_gate: Mutex::new(()),
                events,
                audio,
                ap_ready: ap_ready_rx,
                ap_ready_tx,
                file_transfer_active,
                realtime_active: AtomicBool::new(false),
            }),
        };
        device.sync_time_now().await?;
        tracing::info!(target: "dyj.device", "✅ 时间同步完成，连接就绪");
        Ok(device)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<DeviceEvent> {
        self.core.events.subscribe()
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.core.transport.disconnect().await
    }

    /// Sends a validated protocol command and returns its JSON response.
    pub async fn raw_command(&self, command: &str, payload: Option<Value>) -> Result<Value> {
        self.request_value(command, payload, NORMAL_TIMEOUT).await
    }

    pub async fn device_info(&self) -> Result<DeviceInfo> {
        self.request_typed("BASE_INFO", None, NORMAL_TIMEOUT).await
    }
    pub async fn state(&self) -> Result<DeviceState> {
        self.request_typed("STATE_INFO", None, NORMAL_TIMEOUT).await
    }
    pub async fn battery(&self) -> Result<BatteryInfo> {
        self.request_typed("BATTERY", None, NORMAL_TIMEOUT).await
    }
    /// SD 卡容量查询（nRF 侧读卡，要求 WiFi 模块未上电，见 [`Self::close_wifi`]）。
    pub async fn storage(&self) -> Result<StorageInfo> {
        self.require_sd_on_nrf("STORAGE").await?;
        self.request_typed("STORAGE", None, NORMAL_TIMEOUT).await
    }
    pub async fn record_status(&self) -> Result<RecordStatus> {
        self.request_typed("RECORD_STATUS", None, NORMAL_TIMEOUT)
            .await
    }
    /// Queries the WiFi module status.
    ///
    /// `WIFI_STATUS` travels through the nRF's UART passthrough and is only answerable while the
    /// ESP32 is powered (i.e. after a successful `WIFI_OPEN`). When `STATE_INFO` reports
    /// `wifiOpen == false` the module cannot reply, so the powered-down state is returned
    /// directly instead of burning the 10 s timeout on an unanswered frame.
    pub async fn wifi_status(&self) -> Result<WifiStatus> {
        let state = self.state().await?;
        if !state.wifi_open {
            tracing::info!(target: "dyj.wifi", "📶 WiFi模块未开启，跳过WIFI_STATUS查询");
            return Ok(WifiStatus {
                status: "off".to_owned(),
                ..Default::default()
            });
        }
        self.request_typed("WIFI_STATUS", None, NORMAL_TIMEOUT)
            .await
    }
    /// ESP32 透传命令的上电预检：模块断电时 UART 无人应答，只会白等到超时。
    /// `STATE_INFO.wifiOpen` 即模块电源状态（WIFI_OPEN/WIFI_CLOSE 由 nRF 本地处理）。
    async fn require_wifi_module(&self, command: &str) -> Result<()> {
        if !self.state().await?.wifi_open {
            tracing::info!(target: "dyj.wifi", "📶 WiFi模块未上电，跳过{command}（先调用 open_wifi）");
            return Err(SdkError::WifiModulePoweredDown {
                command: command.to_owned(),
            });
        }
        Ok(())
    }

    /// nRF 侧 SD 操作的对偶预检：`WIFI_OPEN` 后 SD 卡挂载权移交 ESP32，
    /// nRF 的 STORAGE / FILE_* / 录音写卡都会回 `sd not mounted`。模块上电期间
    /// 文件请走 AP 快传（[`Self::access_point_client`]），要回到 BLE 侧先 `close_wifi`。
    async fn require_sd_on_nrf(&self, command: &str) -> Result<()> {
        if self.state().await?.wifi_open {
            tracing::info!(target: "dyj.device", "💾 SD卡已挂载到WiFi模块，跳过{command}（先调用 close_wifi）");
            return Err(SdkError::SdOwnedByWifiModule {
                command: command.to_owned(),
            });
        }
        Ok(())
    }

    pub async fn wifi_ota_status(&self) -> Result<OtaProgress> {
        self.require_wifi_module("OTA_WIFI_STATUS").await?;
        self.request_typed("OTA_WIFI_STATUS", None, NORMAL_TIMEOUT)
            .await
    }

    pub async fn sync_time_now(&self) -> Result<CommandResponse> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SdkError::InvalidSystemTime)?
            .as_secs();
        self.sync_time(timestamp as i64).await
    }

    pub async fn sync_time(&self, timestamp: i64) -> Result<CommandResponse> {
        self.request_success(
            "SYNC_TIME",
            Some(json!({"timestamp": timestamp})),
            NORMAL_TIMEOUT,
        )
        .await
    }

    /// 开始录音（nRF 写 SD 卡，要求 WiFi 模块未上电，否则固件回 `SD卡未挂载`）。
    pub async fn start_recording(&self) -> Result<CommandResponse> {
        self.require_sd_on_nrf("RECORD_START").await?;
        self.request_success("RECORD_START", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn stop_recording(&self) -> Result<CommandResponse> {
        self.request_success("RECORD_STOP", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn toggle_recording(&self) -> Result<CommandResponse> {
        self.request_success("RECORD", None, NORMAL_TIMEOUT).await
    }
    pub async fn set_realtime_enabled(&self, enabled: bool) -> Result<CommandResponse> {
        self.request_success(
            "RECORD_REALTIME_ENABLE",
            Some(json!({"enabled": enabled})),
            NORMAL_TIMEOUT,
        )
        .await
    }
    /// 录音打点（写同名 `.mrk`，同样要求 SD 在 nRF 侧）。未在录音时固件返回错误。
    pub async fn record_mark(&self) -> Result<RecordMark> {
        self.require_sd_on_nrf("RECORD_MARK").await?;
        self.request_typed("RECORD_MARK", None, NORMAL_TIMEOUT)
            .await
    }

    /// 订阅解码后的实时 PCM 流（16kHz 单声道 i16）。
    ///
    /// 无论录音由本 SDK 还是按键/开关在设备侧触发，只要实时传输开着，二进制音频块
    /// 都会持续到达并被解码广播。积压超过容量时 `recv()` 返回 `Lagged`，表示中间有
    /// 块被丢弃、解码流出现断裂。
    pub fn subscribe_audio(&self) -> broadcast::Receiver<PcmChunk> {
        self.core.audio.subscribe()
    }

    /// 打开实时传输并开始录音，返回会话句柄。
    ///
    /// 音频通过 [`Self::subscribe_audio`]（或会话上的 [`RealtimeRecordingSession::subscribe`]）
    /// 获取；录音期间 SD 卡仍同步写入 `.apm` 文件。会话结束（[`RealtimeRecordingSession::stop`]
    /// 或 drop）会停止录音并关闭实时传输。
    pub async fn start_realtime_recording(&self) -> Result<RealtimeRecordingSession> {
        if self.core.realtime_active.load(Ordering::SeqCst) {
            return Err(SdkError::InvalidCommand(
                "realtime recording session already active".into(),
            ));
        }
        self.set_realtime_enabled(true).await?;
        if let Err(error) = self.start_recording().await {
            // 回滚实时开关，避免留下“开着流但没在录音”的状态
            let _ = self.set_realtime_enabled(false).await;
            return Err(error);
        }
        self.core.realtime_active.store(true, Ordering::SeqCst);
        tracing::info!(target: "dyj.audio", "🎙️ 实时录音已开始（16kHz 单声道，ADPCM≈8KB/s）");
        Ok(RealtimeRecordingSession {
            device: self.clone(),
        })
    }

    /// 停止录音并关闭实时传输。会话 drop 时也会尽力执行同样的收尾。
    async fn stop_realtime_recording(&self) -> Result<()> {
        self.core.realtime_active.store(false, Ordering::SeqCst);
        let result = self.stop_recording().await;
        // RECORD_STOP 因“未在录音”失败（设备侧已停）时同样要把实时开关关掉。
        let _ = self.set_realtime_enabled(false).await;
        tracing::info!(target: "dyj.audio", "🎙️ 实时录音已停止");
        result.map(|_| ())
    }

    /// BLE 侧文件列表（nRF 读 SD，要求 WiFi 模块未上电；模块上电期间请用 AP 快传列表）。
    pub async fn list_files(&self, folder: Option<&str>) -> Result<Vec<FileEntry>> {
        self.require_sd_on_nrf("FILE_LIST").await?;
        let payload = folder.map(|folder| json!({"folder": folder}));
        self.collect_indexed("FILE_LIST", payload).await
    }
    pub async fn delete_file(&self, filename: &str) -> Result<CommandResponse> {
        self.require_sd_on_nrf("FILE_DELETE").await?;
        self.request_success(
            "FILE_DELETE",
            Some(json!({"filename": filename})),
            NORMAL_TIMEOUT,
        )
        .await
    }
    pub async fn clear_files(&self) -> Result<Vec<FileEntry>> {
        self.require_sd_on_nrf("FILE_CLEAR").await?;
        self.collect_indexed("FILE_CLEAR", None).await
    }

    /// Transfers a file into `writer`. The firmware deletes the source file (and possible matching
    /// `.TXT`) only after a successful transfer, so this operation intentionally has "take" semantics.
    ///
    /// 实时录音进行中时拒绝执行：二进制通道没有头部字段，音频流和文件数据无法区分，
    /// 混流会把音频块写进目标文件。WiFi 模块上电期间同样拒绝（SD 在 ESP32 侧），
    /// 大文件本就应走 AP 快传。
    pub async fn take_file_to<W: AsyncWrite + Unpin>(
        &self,
        name: &str,
        writer: &mut W,
    ) -> Result<TakenFile> {
        if self.core.realtime_active.load(Ordering::SeqCst) {
            tracing::warn!(target: "dyj.file", "⚠️ 实时录音进行中，拒绝取文件 name={name}");
            return Err(SdkError::RealtimeRecordingActive);
        }
        self.require_sd_on_nrf("FILE_UPLOAD").await?;
        tracing::info!(target: "dyj.file", "📄 请求取走文件 name={name}");
        let _command = self.core.command_gate.lock().await;
        self.core.file_transfer_active.store(true, Ordering::SeqCst);
        let _transfer_guard = FlagGuard(&self.core.file_transfer_active);
        let mut incoming = self.core.incoming.lock().await;
        discard_stale_inbound(&mut incoming, "FILE_UPLOAD")?;
        self.write_command("FILE_UPLOAD", Some(json!({"name": name})))
            .await?;

        let mut expected_size = None;
        let mut result_name = name.to_owned();
        let mut written = 0_u64;
        let mut next_milestone = FILE_PROGRESS_STEP;
        loop {
            match receive_next(&mut incoming, FILE_IDLE_TIMEOUT, "FILE_UPLOAD").await? {
                Inbound::Binary(bytes) if expected_size.is_some() => {
                    writer.write_all(&bytes).await?;
                    written += bytes.len() as u64;
                    tracing::debug!(target: "dyj.file", "📄 收到文件块 {}字节 累计={written}", bytes.len());
                    if written >= next_milestone {
                        next_milestone += FILE_PROGRESS_STEP;
                        let total = expected_size.unwrap_or(0);
                        let percent = (written * 100).checked_div(total).unwrap_or(0);
                        tracing::info!(target: "dyj.file", "📄 传输进度 name={result_name} 已传={written}/{total}字节 ({percent}%)");
                    }
                }
                Inbound::Text(frame) if frame.command == "FILE_UPLOAD" => {
                    ensure_success(&frame.command, &frame.payload)?;
                    match frame.payload.get("state").and_then(Value::as_str) {
                        Some("start") => {
                            expected_size = Some(required_u64(&frame.payload, "size")?);
                            if let Some(received_name) =
                                frame.payload.get("name").and_then(Value::as_str)
                            {
                                result_name = received_name.to_owned();
                            }
                            tracing::info!(target: "dyj.file", "📄 传输开始 name={result_name} size={}", expected_size.unwrap_or(0));
                        }
                        Some("end") => {
                            let expected = expected_size.ok_or_else(|| {
                                SdkError::InvalidFrame("FILE_UPLOAD ended without start".into())
                            })?;
                            if written != expected {
                                tracing::error!(target: "dyj.file", "❌ 传输校验失败 name={result_name} 期望={expected} 实际={written}");
                                return Err(SdkError::FileLengthMismatch {
                                    expected,
                                    actual: written,
                                });
                            }
                            writer.flush().await?;
                            tracing::info!(target: "dyj.file", "✅ 传输完成 name={result_name} 共{written}字节（设备源文件已删除）");
                            return Ok(TakenFile {
                                name: result_name,
                                bytes_written: written,
                            });
                        }
                        _ => {
                            return Err(SdkError::InvalidFrame("invalid FILE_UPLOAD state".into()));
                        }
                    }
                }
                Inbound::Binary(bytes) => {
                    // 无头二进制只可能来自设备侧自触发的后台实时录音（按键/开关触发
                    // 且实时开关持久化开启）。文件块不带任何标记，无法与音频流区分，
                    // 这里只能丢弃并警告——真正可靠的互斥由 realtime_active 守卫保证。
                    tracing::warn!(
                        target: "dyj.file",
                        "⚠️ 传输开始前收到 {}字节二进制数据（疑似后台实时音频流），已丢弃",
                        bytes.len()
                    );
                }
                _ => {}
            }
        }
    }

    /// BLE 取走到目录：先写 `<dir>/<原名>.part`，成功后按固件时间戳名改成
    /// `YYYYMMDD-HHMMSS_<原编号>.apm`（见 [`crate::recording_file_name`]），失败清掉半截文件。
    /// 取走语义不变（设备删源文件）；大文件请走 AP 快传的 `download_to_dir`。
    pub async fn take_file_to_dir(
        &self,
        name: &str,
        dir: impl AsRef<Path>,
    ) -> Result<SavedRecording> {
        let dir = dir.as_ref();
        tokio::fs::create_dir_all(dir).await?;
        let original = recording::base_name(name).to_owned();
        let part = recording::part_path(dir, &original);
        let mut file = tokio::fs::File::create(&part).await?;
        let result = match self.take_file_to(name, &mut file).await {
            Ok(result) => result,
            Err(error) => {
                drop(file);
                recording::discard_part(&part).await;
                return Err(error);
            }
        };
        drop(file);
        recording::finalize(
            dir,
            &part,
            &original,
            Some(&result.name),
            result.bytes_written,
        )
        .await
    }

    /// 给 WiFi 模块上电（幂等）。
    ///
    /// BLE 断链重连后 ESP32 往往仍在供电（`STATE_INFO.wifiOpen == true`），此时重复发
    /// `WIFI_OPEN` 没有意义；直接视为成功，并把快传入口标记为就绪——AP 随上电自启，
    /// 本次连接不会再收到 `WIFI_AP_START`。真正上电时仍等待固件应答（≤35s）。
    pub async fn open_wifi(&self) -> Result<CommandResponse> {
        if self.state().await?.wifi_open {
            tracing::info!(target: "dyj.wifi", "📶 WiFi模块已在上电状态，跳过WIFI_OPEN（AP 视为就绪）");
            self.core.ap_ready_tx.send_replace(true);
            return Ok(CommandResponse {
                code: Some(0),
                message: Some("WiFi module already powered".to_owned()),
                extra: Default::default(),
            });
        }
        self.request_success("WIFI_OPEN", None, WIFI_OPEN_TIMEOUT)
            .await
    }
    pub async fn close_wifi(&self) -> Result<CommandResponse> {
        self.request_success("WIFI_CLOSE", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn connect_wifi(&self, ssid: &str, password: &str) -> Result<CommandResponse> {
        self.require_wifi_module("WIFI_CONNECT").await?;
        self.request_success(
            "WIFI_CONNECT",
            Some(json!({"ssid": ssid, "password": password})),
            WIFI_CONNECT_TIMEOUT,
        )
        .await
    }
    pub async fn disconnect_wifi(&self) -> Result<CommandResponse> {
        self.require_wifi_module("WIFI_DISCONNECT").await?;
        self.request_success("WIFI_DISCONNECT", None, NORMAL_TIMEOUT)
            .await
    }

    /// 手动启动 AP（可选自定义 ssid/password）。
    ///
    /// ⚠️ 需要带 `AP_` 透传的固件：v0.3.x 的 nRF 会把 `AP_*` 静默丢弃，
    /// 调用将一直等到超时。v0.3.x 上 AP 随 `WIFI_OPEN` 自动启动，无需调用
    /// 本命令；就绪判断用 [`Self::wait_for_ap_ready`]。
    pub async fn start_access_point(
        &self,
        ssid: Option<&str>,
        password: Option<&str>,
    ) -> Result<AccessPointInfo> {
        self.require_wifi_module("AP_START").await?;
        let mut payload = serde_json::Map::new();
        if let Some(ssid) = ssid {
            payload.insert("ssid".into(), Value::String(ssid.into()));
        }
        if let Some(password) = password {
            payload.insert("password".into(), Value::String(password.into()));
        }
        self.request_typed(
            "AP_START",
            (!payload.is_empty()).then_some(Value::Object(payload)),
            NORMAL_TIMEOUT,
        )
        .await
    }

    /// 手动停止 AP。⚠️ 需要 `AP_` 透传（同 [`Self::start_access_point`]）。
    pub async fn stop_access_point(&self) -> Result<CommandResponse> {
        self.require_wifi_module("AP_STOP").await?;
        self.request_success("AP_STOP", None, NORMAL_TIMEOUT).await
    }

    /// 查询 AP 状态。⚠️ 需要 `AP_` 透传（同 [`Self::start_access_point`]）；
    /// v0.3.x 上会等到超时。就绪判断请用 [`Self::wait_for_ap_ready`]，
    /// 快传客户端请用 [`Self::access_point_client`]。
    pub async fn access_point_status(&self) -> Result<AccessPointInfo> {
        self.require_wifi_module("AP_STATUS").await?;
        self.request_typed("AP_STATUS", None, NORMAL_TIMEOUT).await
    }

    /// 当前快传入口是否就绪（收到过 `WIFI_AP_START` 且未经历新的上电/关电）。
    pub fn access_point_ready(&self) -> bool {
        *self.core.ap_ready.borrow()
    }

    /// 等待快传入口就绪（`WIFI_AP_START` 事件）。
    ///
    /// 固件在 `WIFI_OPEN` 上电后自动启动 AP + HTTP 服务器，两端就绪时上报
    /// `WIFI_AP_START`。`WIFI_OPEN` 的响应早于该事件，因此调完 `open_wifi`
    /// 后想立刻用快传，应先经此方法等待。
    pub async fn wait_for_ap_ready(&self, wait: Duration) -> Result<()> {
        if self.ap_ready_within(wait).await? {
            return Ok(());
        }
        tracing::warn!(target: "dyj.ap", "⏱️ 快传入口就绪等待超时（{}s）", wait.as_secs());
        Err(SdkError::Timeout {
            command: "WIFI_AP_START".to_owned(),
            seconds: wait.as_secs(),
        })
    }

    /// 在 `wait` 内等到就绪返回 true，超时返回 false（不打日志，供内部推断用）。
    async fn ap_ready_within(&self, wait: Duration) -> Result<bool> {
        let mut rx = self.core.ap_ready.clone();
        let wait_ready = async {
            loop {
                if *rx.borrow_and_update() {
                    return Ok(());
                }
                // 发送端在 worker/Core 里，与接收端同生命周期，不会 Closed。
                rx.changed().await.map_err(|_| SdkError::Disconnected)?;
            }
        };
        match timeout(wait, wait_ready).await {
            Ok(result) => result.map(|()| true),
            Err(_) => Ok(false),
        }
    }

    /// 构建快传 HTTP 客户端（文件列表 / 下载 / 删除，需电脑已加入设备热点）。
    ///
    /// 固件（v0.3.x）上电即自动启动 AP，端点固定为 `192.168.4.1:80`
    /// （`DYJ-HD-AP`，见 [`crate::AP_DEFAULT_SSID`]），就绪时上报
    /// `WIFI_AP_START` 事件。该固件的 nRF 不透传 `AP_*` 命令，因此这里
    /// 不查询 `AP_STATUS`，等待就绪事件后直接使用固定端点；自定义了
    /// AP 参数的未来固件请改用 [`Self::access_point_status`] 取真实值。
    ///
    /// 就绪事件只在本次连接期间上电时才能收到：若连接建立时模块已在供电
    /// （BLE 断链重连），事件早已发过，这里在短暂宽限后按“AP 随上电自启”推断就绪。
    pub async fn access_point_client(&self) -> Result<crate::AccessPointClient> {
        self.require_wifi_module("AP_STATUS").await?;
        if !self.ap_ready_within(AP_READY_GRACE).await? {
            tracing::info!(target: "dyj.ap", "📶 模块已上电但未见 WIFI_AP_START，按 AP 已自启处理");
            self.core.ap_ready_tx.send_replace(true);
        }
        crate::AccessPointClient::new(crate::AP_DEFAULT_IP, crate::AP_DEFAULT_PORT)
    }

    pub async fn scan_wifi(&self) -> Result<Vec<WifiNetwork>> {
        self.require_wifi_module("WIFI_SCAN").await?;
        let _command = self.core.command_gate.lock().await;
        let mut incoming = self.core.incoming.lock().await;
        discard_stale_inbound(&mut incoming, "WIFI_SCAN")?;
        self.write_command("WIFI_SCAN", None).await?;
        let mut started = false;
        let mut networks = Vec::new();
        loop {
            match receive_next(&mut incoming, NORMAL_TIMEOUT, "WIFI_SCAN").await? {
                Inbound::Text(frame) if frame.command == "WIFI_SCAN" => {
                    ensure_success(&frame.command, &frame.payload)?;
                    started = true;
                }
                Inbound::Text(frame) if started && frame.command == "WIFI_LIST" => {
                    let network: WifiNetwork = serde_json::from_value(frame.payload)?;
                    if network.index == -1 {
                        tracing::info!(target: "dyj.wifi", "📶 WiFi扫描完成 共{}个网络", networks.len());
                        return Ok(networks);
                    }
                    tracing::debug!(target: "dyj.wifi", "📶 网络 {} ssid={:?} rssi={:?}", network.index, network.ssid, network.rssi);
                    networks.push(network);
                }
                _ => {}
            }
        }
    }

    pub async fn list_uploads(&self) -> Result<Vec<FileEntry>> {
        self.require_wifi_module("UPLOAD_LIST").await?;
        self.collect_indexed("UPLOAD_LIST", None).await
    }
    /// Uploads a file over the ESP32 STA link. The firmware responds only after the whole
    /// HTTP upload finishes, so this call can legitimately run for minutes on large files.
    pub async fn upload_file(&self, name: &str) -> Result<CommandResponse> {
        self.upload_file_via_sta_to(name, None).await
    }
    /// Same as [`Self::upload_file_via_sta`], with an explicit target base URL
    /// (WiFi 0.1.0+；旧固件会忽略 `url` 并回落 token 门控的旧链路).
    pub async fn upload_file_via_sta_to(
        &self,
        name: &str,
        url: Option<&str>,
    ) -> Result<CommandResponse> {
        self.require_wifi_module("UPLOAD_FILE").await?;
        let mut payload = json!({"name": name});
        if let Some(url) = url {
            payload["url"] = json!(url);
        }
        self.request_success("UPLOAD_FILE", Some(payload), UPLOAD_FILE_TIMEOUT)
            .await
    }
    /// Convenience alias kept from earlier releases.
    pub async fn upload_file_via_sta(&self, name: &str) -> Result<CommandResponse> {
        self.upload_file_via_sta_to(name, None).await
    }
    /// 逐个上传 `UPLOAD_LIST` 里的全部文件到 `url`，返回 (文件名, 结果) 列表；
    /// 单个失败不中断其余文件。
    pub async fn upload_all_via_sta(
        &self,
        url: Option<&str>,
    ) -> Result<Vec<(String, Result<CommandResponse>)>> {
        let entries = self.list_uploads().await?;
        let mut results = Vec::new();
        for entry in entries {
            let Some(name) = entry.name else { continue };
            let outcome = self.upload_file_via_sta_to(&name, url).await;
            results.push((name, outcome));
        }
        Ok(results)
    }
    /// 持久化默认上传基址（`upload_url`，需主控 ≥0.3.7 + WiFi ≥0.1.0；值 ≤63 字节）。
    pub async fn set_wifi_upload_url(&self, url: &str) -> Result<CommandResponse> {
        self.set_config("upload_url", url).await
    }
    /// 读取持久化的默认上传基址（未配置返回 None）。
    pub async fn wifi_upload_url(&self) -> Result<Option<String>> {
        self.config_value("upload_url").await
    }
    pub async fn delete_upload(&self, name: &str) -> Result<CommandResponse> {
        self.require_wifi_module("UPLOAD_DELETE").await?;
        self.request_success("UPLOAD_DELETE", Some(json!({"name": name})), NORMAL_TIMEOUT)
            .await
    }
    pub async fn clear_uploads(&self) -> Result<CommandResponse> {
        self.require_wifi_module("UPLOAD_CLEAR").await?;
        self.request_success("UPLOAD_CLEAR", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn activate(
        &self,
        device_id: &str,
        device_name: &str,
        code: &str,
    ) -> Result<CommandResponse> {
        self.require_wifi_module("AUTH_ACTIVE").await?;
        self.request_success(
            "AUTH_ACTIVE",
            Some(json!({"deviceId": device_id, "deviceName": device_name, "code": code})),
            NORMAL_TIMEOUT,
        )
        .await
    }
    pub async fn clear_activation(&self) -> Result<CommandResponse> {
        self.require_wifi_module("AUTH_CLEAR").await?;
        self.request_success("AUTH_CLEAR", None, NORMAL_TIMEOUT)
            .await
    }
    // ========== WiFi 模块 OTA ==========
    //
    // 需要 STA 已连上路由且能访问 `api_url` + `ota_path`
    // （默认 `/ota/info?mod=wifi`，返回 `{version, versionCode, url}`）。没有该服务时
    // ESP32 回 `code 1 "OTA检查更新失败"`；STA 未连接回 `code 1 "WiFi未连接…"`。

    /// 检查 WiFi 模块是否有新固件。结果缓存在 ESP32 本次上电周期内，
    /// [`Self::start_wifi_ota`] / [`Self::update_wifi_firmware`] 依赖它。
    pub async fn check_wifi_ota(&self) -> Result<WifiOtaCheck> {
        self.require_wifi_module("OTA_WIFI_CHECK").await?;
        self.request_typed("OTA_WIFI_CHECK", None, OTA_CHECK_TIMEOUT)
            .await
    }

    /// 开始 WiFi 模块 OTA（仅发起；进度经 `DeviceEvent::WifiOtaProgress` 推送）。
    /// 同一上电周期内未先 `check_wifi_ota` 或无可用更新时固件拒绝（`code 1`）。
    pub async fn start_wifi_ota(&self) -> Result<CommandResponse> {
        self.require_wifi_module("OTA_WIFI_UPDATE").await?;
        self.request_success("OTA_WIFI_UPDATE", None, NORMAL_TIMEOUT)
            .await
    }

    /// 在线升级 WiFi 模块固件的一步封装：先 `check_wifi_ota`，有更新再
    /// [`Self::update_wifi_firmware`]（固件要求两者在同一上电周期内先后执行）。
    /// 无更新返回 [`WifiOnlineOtaOutcome::UpToDate`]，否则返回终态进度。
    pub async fn check_and_update_wifi_firmware(
        &self,
        wait: Duration,
    ) -> Result<WifiOnlineOtaOutcome> {
        let check = self.check_wifi_ota().await?;
        if !check.update_available {
            tracing::info!(target: "dyj.ota", "✅ WiFi 模块已是最新，未发起升级");
            return Ok(WifiOnlineOtaOutcome::UpToDate);
        }
        Ok(WifiOnlineOtaOutcome::Finished(
            self.update_wifi_firmware(wait).await?,
        ))
    }

    /// 发起 WiFi 模块 OTA 并跟进到终态（`Success` / `Failed`），返回最后一条进度。
    ///
    /// 先订阅事件再发命令，不会漏掉首条进度；固件每 5% 推一条，整个过程通常
    /// 数十秒到数分钟，`wait` 是总时长上限。`Success` 后 ESP32 会自行重启，
    /// 热点随之重启——之后要用快传请重新等待 [`Self::wait_for_ap_ready`]。
    /// 调用前需在同一上电周期内 [`Self::check_wifi_ota`] 得到 `update_available == true`。
    pub async fn update_wifi_firmware(&self, wait: Duration) -> Result<OtaProgress> {
        let mut events = self.subscribe_events();
        self.start_wifi_ota().await?;
        tracing::info!(target: "dyj.ota", "⬇️ WiFi 模块 OTA 已发起，跟进进度（上限 {}s）", wait.as_secs());
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let event = match timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                events.recv(),
            )
            .await
            {
                Ok(Ok(event)) => event,
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    tracing::warn!(target: "dyj.ota", "⚠️ OTA 进度事件积压丢弃 {skipped} 条");
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => return Err(SdkError::Disconnected),
                Err(_) => {
                    tracing::error!(target: "dyj.ota", "⏱️ OTA 未在 {}s 内结束", wait.as_secs());
                    return Err(SdkError::Timeout {
                        command: "OTA_WIFI_PROCESS".to_owned(),
                        seconds: wait.as_secs(),
                    });
                }
            };
            if let DeviceEvent::WifiOtaProgress(progress) = event {
                tracing::info!(target: "dyj.ota", "⬇️ OTA {} {}%（{}/{} 字节）", progress.status, progress.percentage, progress.progress, progress.total);
                if progress.status.is_terminal() {
                    return Ok(progress);
                }
            }
        }
    }

    // ========== 设备 KV 配置 ==========
    //
    // 键以 `wifi_` 开头或以 `_wifi` 结尾时 nRF 不落本地 KV，而是透传给 ESP32
    // （需模块已上电）；其余键存 nRF 的 16 槽 KV。两侧"键不存在"的表现不同：
    // nRF 回 `code 3 "not found"`，ESP32 回 `value: ""`——[`Self::config_value`] 统一成 `None`。

    /// 写入设备 KV 配置。键 ≤ [`CONFIG_KEY_MAX_BYTES`]、值 ≤ [`CONFIG_VALUE_MAX_BYTES`]
    /// （nRF 解析缓冲；超长会被固件误报为 `missing key/value`，因此在本地先拒绝）。
    pub async fn set_config(&self, key: &str, value: &str) -> Result<CommandResponse> {
        validate_config_key(key)?;
        validate_config_value(value)?;
        self.require_wifi_module_for_config_key("CONFIG_SET", key)
            .await?;
        self.request_success(
            "CONFIG_SET",
            Some(json!({"key": key, "value": value})),
            NORMAL_TIMEOUT,
        )
        .await
    }

    pub async fn set_sta_api_base_url(&self, url: &str) -> Result<CommandResponse> {
        validate_url_scheme(url, &["http", "https"])?;
        self.set_config("api_url", url).await
    }

    pub async fn sta_api_base_url(&self) -> Result<Option<String>> {
        self.config_value("api_url").await
    }

    /// ESP32 拉取 OTA 版本信息的相对路径（拼在 `api_url` 之后），
    /// 例如 `/ota/info?mod=wifi`（未配置时的固件默认值）。
    pub async fn set_wifi_ota_info_path(&self, path: &str) -> Result<CommandResponse> {
        validate_relative_path(path, "WiFi OTA info path")?;
        self.set_config("ota_path", path).await
    }

    pub async fn wifi_ota_info_path(&self) -> Result<Option<String>> {
        self.config_value("ota_path").await
    }

    /// 原样返回 `CONFIG_GET` 响应；nRF 侧键不存在时固件回错误（`code 3`）。
    /// 只想拿值请用 [`Self::config_value`]。
    pub async fn get_config(&self, key: &str) -> Result<Value> {
        validate_config_key(key)?;
        self.require_wifi_module_for_config_key("CONFIG_GET", key)
            .await?;
        self.request_value("CONFIG_GET", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await
    }

    /// 读取设备 KV 配置；键不存在返回 `Ok(None)` 而不是错误。
    /// ESP32 路由的键把固件回的空字符串也视为 `None`（ESP32 对缺键就是这么回的）。
    pub async fn config_value(&self, key: &str) -> Result<Option<String>> {
        validate_config_key(key)?;
        self.require_wifi_module_for_config_key("CONFIG_GET", key)
            .await?;
        let payload = self
            .request_payload("CONFIG_GET", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await?;
        if response_code(&payload) == Some(CONFIG_CODE_NOT_FOUND) {
            return Ok(None);
        }
        ensure_success("CONFIG_GET", &payload)?;
        let value = payload
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SdkError::InvalidFrame(format!("CONFIG_GET `{key}` omitted string value"))
            })?;
        if value.is_empty() && config_key_routes_to_wifi_module(key) {
            return Ok(None);
        }
        Ok(Some(value.to_owned()))
    }

    /// 透传给 ESP32 的配置键要求模块已上电，否则 UART 无人应答只能等超时。
    async fn require_wifi_module_for_config_key(&self, command: &str, key: &str) -> Result<()> {
        if config_key_routes_to_wifi_module(key) {
            self.require_wifi_module(command).await?;
        }
        Ok(())
    }

    /// 删除设备 KV 配置；键不存在时固件回错误（nRF `code 3 not found`，ESP32 `code 1 配置删除失败`）。
    /// 想要"不存在也算成功"请用 [`Self::clear_config`]。
    pub async fn delete_config(&self, key: &str) -> Result<CommandResponse> {
        validate_config_key(key)?;
        self.require_wifi_module_for_config_key("CONFIG_DEL", key)
            .await?;
        self.request_success("CONFIG_DEL", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await
    }

    /// 幂等删除：键不存在返回 `Ok(false)`，删掉了返回 `Ok(true)`。
    ///
    /// nRF 侧按 `code 3` 识别"不存在"；ESP32 对不存在的键回 `code 1 "配置删除失败"`，
    /// 与真实失败无法区分，因此路由键先读一次再决定删不删。
    pub async fn clear_config(&self, key: &str) -> Result<bool> {
        validate_config_key(key)?;
        self.require_wifi_module_for_config_key("CONFIG_DEL", key)
            .await?;
        if config_key_routes_to_wifi_module(key) {
            if self.config_value(key).await?.is_none() {
                tracing::debug!(target: "dyj.device", "🧹 {key} 本就未配置");
                return Ok(false);
            }
            self.request_success("CONFIG_DEL", Some(json!({"key": key})), NORMAL_TIMEOUT)
                .await?;
            return Ok(true);
        }
        let payload = self
            .request_payload("CONFIG_DEL", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await?;
        if response_code(&payload) == Some(CONFIG_CODE_NOT_FOUND) {
            tracing::debug!(target: "dyj.device", "🧹 {key} 本就未配置");
            return Ok(false);
        }
        ensure_success("CONFIG_DEL", &payload)?;
        Ok(true)
    }

    // ========== 按键 / 侧边开关绑定 ==========
    //
    // 绑定存在设备 KV 里（16 槽，与其他设置共用），重启后保留；固件在输入触发时
    // 自己执行绑定的动作并上报事件，上位机只需订阅 `DeviceEvent::Button` /
    // `DeviceEvent::ToggleSwitch` / `DeviceEvent::RecordStatus`（reason=button/switch）。

    /// 读取某个输入当前绑定的动作；设备上没配置过的输入返回 [`ControlAction::None`]
    /// （与固件运行时的默认行为一致）。
    pub async fn control_action(&self, input: impl Into<ControlInput>) -> Result<ControlAction> {
        let input = input.into();
        Ok(self
            .config_value(input.config_key())
            .await?
            .map(|value| ControlAction::parse(&value))
            .unwrap_or_default())
    }

    /// 把输入绑定到动作。[`ControlAction::None`] 也会显式写入 `none`。
    pub async fn set_control_action(
        &self,
        input: impl Into<ControlInput>,
        action: ControlAction,
    ) -> Result<()> {
        let input = input.into();
        tracing::info!(target: "dyj.controls", "🎛️ 绑定 {input} → {action}");
        self.set_config(input.config_key(), action.wire_name())
            .await
            .map(|_| ())
    }

    /// 删除输入的绑定（回到未配置状态）；本就没有绑定时也视为成功。
    pub async fn clear_control_action(&self, input: impl Into<ControlInput>) -> Result<()> {
        let input = input.into();
        if self.clear_config(input.config_key()).await? {
            tracing::info!(target: "dyj.controls", "🎛️ 已解除绑定 {input}");
        } else {
            tracing::debug!(target: "dyj.controls", "🎛️ {input} 本就未绑定");
        }
        Ok(())
    }

    /// 一次读回六个输入的绑定表（六次 `CONFIG_GET`）。
    pub async fn control_bindings(&self) -> Result<ControlBindings> {
        let mut bindings = ControlBindings::default();
        for input in ControlInput::ALL {
            let action = self.control_action(input).await?;
            bindings.set(input, action);
        }
        tracing::info!(target: "dyj.controls", "🎛️ 当前绑定表 {bindings:?}");
        Ok(bindings)
    }

    /// 整表写入（六次 `CONFIG_SET`）。中途失败时已写入的项保留，不回滚。
    pub async fn apply_control_bindings(&self, bindings: &ControlBindings) -> Result<()> {
        for (input, action) in bindings.entries() {
            self.set_control_action(input, action.clone()).await?;
        }
        Ok(())
    }

    pub async fn set_led(&self, state: LedState) -> Result<CommandResponse> {
        self.request_success(
            "LED",
            Some(json!({"state": state.wire_name()})),
            NORMAL_TIMEOUT,
        )
        .await
    }
    /// 在 OLED 上显示自定义内容（需主控固件 ≥0.3.7）。文本走设备内置 5×8 字体
    /// （仅可打印 ASCII）；点阵/中文/图形由调用方渲染成 72×40 位图传入。
    pub async fn display_show(&self, request: &DisplayShowRequest) -> Result<CommandResponse> {
        request.validate()?;
        let mut payload = serde_json::Map::new();
        if !request.lines.is_empty() {
            payload.insert("lines".into(), json!(request.lines));
        }
        if let Some(bitmap) = &request.bitmap {
            use base64::Engine as _;
            payload.insert(
                "bitmap".into(),
                json!(base64::engine::general_purpose::STANDARD.encode(bitmap)),
            );
        }
        payload.insert("persistent".into(), json!(request.persistent));
        payload.insert("timeoutMs".into(), json!(request.timeout_ms));
        self.request_success("DISPLAY_SHOW", Some(Value::Object(payload)), NORMAL_TIMEOUT)
            .await
    }
    /// 清除屏幕内容并复位显示超时（需主控固件 ≥0.3.7）。
    pub async fn display_clear(&self) -> Result<CommandResponse> {
        self.request_success("DISPLAY_CLEAR", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn test_modules(&self) -> Result<Vec<ModuleCheck>> {
        // Firmware sends exactly one response for each of its 11 registered modules and no sentinel.
        self.collect_count("TEST_CHECK", None, 11).await
    }
    pub async fn restart(&self) -> Result<CommandResponse> {
        self.request_success("RESTART", None, NORMAL_TIMEOUT).await
    }
    pub async fn shutdown(&self) -> Result<CommandResponse> {
        self.request_success("SHUTDOWN", None, NORMAL_TIMEOUT).await
    }
    /// Requests that the device enter Nordic DFU mode. Firmware transfer is intentionally out of scope.
    pub async fn enter_dfu_mode(&self) -> Result<CommandResponse> {
        self.request_success("OTA", None, NORMAL_TIMEOUT).await
    }

    async fn request_typed<T: DeserializeOwned>(
        &self,
        command: &str,
        payload: Option<Value>,
        wait: Duration,
    ) -> Result<T> {
        serde_json::from_value(self.request_value(command, payload, wait).await?)
            .map_err(Into::into)
    }

    async fn request_success(
        &self,
        command: &str,
        payload: Option<Value>,
        wait: Duration,
    ) -> Result<CommandResponse> {
        let value = self.request_value(command, payload, wait).await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    async fn request_value(
        &self,
        command: &str,
        payload: Option<Value>,
        wait: Duration,
    ) -> Result<Value> {
        let command = command.to_ascii_uppercase();
        let started = std::time::Instant::now();
        let payload = self.request_payload(&command, payload, wait).await?;
        ensure_success(&command, &payload)?;
        tracing::info!(target: "dyj.proto", "📥 收到响应 cmd={command} code=0 耗时={}ms", started.elapsed().as_millis());
        Ok(payload)
    }

    /// 发命令并取回同名响应的原始载荷，**不**检查 `code`——调用方要自己处理
    /// 预期中的非零码（如 CONFIG 的 `not found`）。
    async fn request_payload(
        &self,
        command: &str,
        payload: Option<Value>,
        wait: Duration,
    ) -> Result<Value> {
        let command = command.to_ascii_uppercase();
        let _command = self.core.command_gate.lock().await;
        let mut incoming = self.core.incoming.lock().await;
        discard_stale_inbound(&mut incoming, &command)?;
        self.write_command(&command, payload).await?;
        loop {
            if let Inbound::Text(frame) = receive_next(&mut incoming, wait, &command).await?
                && frame.command == command
                && !is_unsolicited_push(&frame)
            {
                tracing::debug!(target: "dyj.proto", "📥 响应内容 cmd={} body={}", frame.command, frame.payload);
                return Ok(frame.payload);
            }
        }
    }

    async fn collect_indexed<T: DeserializeOwned>(
        &self,
        command: &str,
        payload: Option<Value>,
    ) -> Result<Vec<T>> {
        let command = command.to_ascii_uppercase();
        let _command = self.core.command_gate.lock().await;
        let mut incoming = self.core.incoming.lock().await;
        discard_stale_inbound(&mut incoming, &command)?;
        self.write_command(&command, payload).await?;
        let mut results = Vec::new();
        loop {
            if let Inbound::Text(frame) =
                receive_next(&mut incoming, NORMAL_TIMEOUT, &command).await?
            {
                if frame.command != command {
                    continue;
                }
                ensure_success(&frame.command, &frame.payload)?;
                if frame.payload.get("index").and_then(Value::as_i64) == Some(-1) {
                    tracing::info!(target: "dyj.device", "📋 列表收集完成 cmd={command} 共{}条", results.len());
                    return Ok(results);
                }
                tracing::debug!(target: "dyj.device", "📋 列表条目 cmd={command} item={}", frame.payload);
                results.push(serde_json::from_value(frame.payload)?);
            }
        }
    }

    async fn collect_count<T: DeserializeOwned>(
        &self,
        command: &str,
        payload: Option<Value>,
        expected_count: usize,
    ) -> Result<Vec<T>> {
        let command = command.to_ascii_uppercase();
        let _command = self.core.command_gate.lock().await;
        let mut incoming = self.core.incoming.lock().await;
        discard_stale_inbound(&mut incoming, &command)?;
        self.write_command(&command, payload).await?;
        let mut results = Vec::with_capacity(expected_count);
        while results.len() < expected_count {
            if let Inbound::Text(frame) =
                receive_next(&mut incoming, NORMAL_TIMEOUT, &command).await?
                && frame.command == command
            {
                ensure_success(&frame.command, &frame.payload)?;
                results.push(serde_json::from_value(frame.payload)?);
            }
        }
        Ok(results)
    }

    async fn write_command(&self, command: &str, payload: Option<Value>) -> Result<()> {
        let bytes = encode_command(command, payload.as_ref())?;
        let payload_text = payload.as_ref().map(redacted_payload).unwrap_or_default();
        tracing::info!(target: "dyj.proto", "📤 发送命令 cmd={command}{}", if payload_text.is_empty() { String::new() } else { format!(" payload={payload_text}") });
        let chunks = bytes.chunks(WRITE_CHUNK_SIZE).count();
        for (index, chunk) in bytes.chunks(WRITE_CHUNK_SIZE).enumerate() {
            tracing::debug!(target: "dyj.proto", "📤 写入分片 {}/{len} {len2}字节", index + 1, len = chunks, len2 = chunk.len());
            self.core.transport.write(chunk).await?;
            if index + 1 < chunks {
                sleep(WRITE_CHUNK_DELAY).await;
            }
        }
        Ok(())
    }
}

/// 一次实时录音会话，由 [`DyjDevice::start_realtime_recording`] 创建。
///
/// Clone 出的副本共享同一个底层设备；任一副本 drop 都不会重复收尾（用原子标志去重）。
#[derive(Clone)]
pub struct RealtimeRecordingSession {
    device: DyjDevice,
}

impl RealtimeRecordingSession {
    /// 订阅解码后的 PCM 流，等价于 [`DyjDevice::subscribe_audio`]。
    pub fn subscribe(&self) -> broadcast::Receiver<PcmChunk> {
        self.device.subscribe_audio()
    }

    /// 停止录音并关闭实时传输。
    pub async fn stop(self) -> Result<()> {
        self.device.stop_realtime_recording().await
    }
}

impl Drop for RealtimeRecordingSession {
    fn drop(&mut self) {
        // swap 兼任去重：显式 stop() 已把标志清零，此处直接返回。
        if !self
            .device
            .core
            .realtime_active
            .swap(false, Ordering::SeqCst)
        {
            return;
        }
        // 会话未 stop 就被丢弃：后台尽力停录并关闭实时开关（运行时已关闭时静默放弃）。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let device = self.device.clone();
            tracing::warn!(target: "dyj.audio", "🎙️ 实时会话未显式停止即被丢弃，后台收尾");
            handle.spawn(async move {
                let _ = device.stop_recording().await;
                let _ = device.set_realtime_enabled(false).await;
            });
        }
    }
}

fn spawn_notification_worker(
    mut stream: crate::NotificationStream,
    incoming: mpsc::UnboundedSender<Inbound>,
    events: broadcast::Sender<DeviceEvent>,
    audio: broadcast::Sender<PcmChunk>,
    file_transfer_active: Arc<AtomicBool>,
    ap_ready: Arc<watch::Sender<bool>>,
) {
    tokio::spawn(async move {
        let mut decoder = FrameDecoder::default();
        // ADPCM 解码状态跨整段录音连续，必须放在 worker 里而不是每个订阅者各自解码；
        // 每段录音开始时随固件编码器一起复位（见 RealtimeAudioDecoder）。
        let mut audio_decoder = RealtimeAudioDecoder::default();
        while let Some(next) = stream.next().await {
            match next {
                Ok(bytes) if bytes.first() == Some(&0) => {
                    let payload = &bytes[1..];
                    if file_transfer_active.load(Ordering::SeqCst) {
                        tracing::debug!(target: "dyj.file", "📄 收到文件二进制块 {}字节", payload.len());
                        let _ = incoming.send(Inbound::Binary(payload.to_vec()));
                    } else {
                        let mut samples = Vec::with_capacity(payload.len() * 2);
                        audio_decoder.decode_chunk(payload, &mut samples);
                        tracing::debug!(target: "dyj.audio", "🎙️ 音频块 {}字节 → {}样本", payload.len(), samples.len());
                        let _ = audio.send(PcmChunk {
                            samples,
                            sample_rate: SAMPLE_RATE_HZ,
                            channels: CHANNELS,
                            discontinuous: false,
                        });
                    }
                }
                Ok(bytes) => {
                    tracing::debug!(target: "dyj.proto", "📥 收到通知 {}字节", bytes.len());
                    let (frames, fault) = decoder.push(&bytes);
                    for frame in frames {
                        let frame = match frame {
                            Frame::Text(frame) => frame,
                            Frame::Raw(value) => {
                                // 裸 JSON 没有命令名，不会有请求在等它，只走事件广播。
                                tracing::info!(target: "dyj.event", "🔋 电源通知 payload={value}");
                                let _ = events.send(event_from_raw(value));
                                continue;
                            }
                        };
                        // 快传入口就绪信号由单帧驱动：AP 随上电自动启动，AP_START 事件
                        // 置位；WIFI_OPEN/CLOSE（新上电周期/关电）与 WIFI_OFF 事件复位。
                        match frame.command.as_str() {
                            "WIFI_AP_START" => {
                                ap_ready.send_replace(true);
                            }
                            "WIFI_OPEN" | "WIFI_CLOSE" | "WIFI_OFF" => {
                                ap_ready.send_replace(false);
                            }
                            _ => {}
                        }
                        audio_decoder.observe_frame(&frame);
                        if let Some(event) = event_from_frame(&frame) {
                            tracing::info!(target: "dyj.event", "🎛️ 硬件事件 cmd={} payload={}", frame.command, frame.payload);
                            let _ = events.send(event);
                        }
                        let _ = incoming.send(Inbound::Text(frame));
                    }
                    if let Some(message) = fault {
                        tracing::warn!(target: "dyj.proto", "⚠️ 跳过无法解析的行: {message}");
                        let _ = incoming.send(Inbound::Fault(message));
                    }
                }
                Err(error) => {
                    tracing::error!(target: "dyj.proto", "❌ 通知流错误，连接不可用: {error}");
                    // 流已死必须立刻以 Disconnected 终结所有等待方，
                    // 否则 take_file 等操作会一直挂到各自的超时（最长 30s）才失败。
                    let _ = incoming.send(Inbound::Disconnected);
                    break;
                }
            }
        }
        tracing::warn!(target: "dyj.proto", "🔌 通知流已结束，设备连接断开");
        let _ = incoming.send(Inbound::Disconnected);
    });
}

/// 实时音频解码器 + 录音段边界跟踪。
///
/// 固件只在每段录音开始（`RECORD_START`，按键/开关/自动分段触发的开始同理）时复位
/// ADPCM 编码器，段内状态连续。解码器若整条连接只建一次，同一连接内第二段录音的开头
/// 会带着上一段残留的 predictor/step 解码，出现一段杂音；所以要在同一边界复位解码器。
///
/// 只在「开始」边界复位一次，段内绝不复位（否则会在连续音频中间制造可听断点）：
/// - 明确的开始信号——`RECORD_START` 成功应答（`code == 0`）或带 `reason` 的
///   `RECORD_STATUS` 推送且 `isRecording == true`——在「未录音」时复位；若已在录音但本段
///   还没解码过音频（同一次开始的应答与推送先后到达）复位也无损，照样复位；已在录音且
///   本段已有音频时视为重复信号，不复位。
/// - 不带 `reason` 的 `RECORD_STATUS` 查询应答只说明「此刻在录」，不是开始边界：仅当
///   上次停止/复位以来还没解码过音频时才复位（此时复位无损）；已有音频在流（例如连上时
///   设备已在录音，漏看了开始边界）则只记状态、不复位。
/// - `RECORD_STOP` 成功应答与 `isRecording == false` 的状态帧（推送或应答）记为停止。
/// - `RECORD`（切换）的应答分不清开始还是停止，不作为边界，依赖随后的状态推送。
#[derive(Default)]
struct RealtimeAudioDecoder {
    decoder: AdpcmDecoder,
    /// worker 视角下设备是否在录音；连接时未知，按未录音处理。
    recording: bool,
    /// 自上次停止或复位以来是否解码过音频。
    decoded_in_segment: bool,
}

impl RealtimeAudioDecoder {
    fn decode_chunk(&mut self, adpcm: &[u8], out: &mut Vec<i16>) {
        self.decoder.decode_chunk(adpcm, out);
        self.decoded_in_segment = true;
    }

    fn observe_frame(&mut self, frame: &TextFrame) {
        let succeeded = || frame.payload.get("code").and_then(Value::as_i64) == Some(0);
        match frame.command.as_str() {
            "RECORD_START" if succeeded() => self.on_start_edge(),
            "RECORD_STOP" if succeeded() => self.on_stop(),
            "RECORD_STATUS" => {
                // 失败应答（只有 code/message）解析不出状态，不改变边界判断。
                let Ok(status) = serde_json::from_value::<RecordStatus>(frame.payload.clone())
                else {
                    return;
                };
                match (status.is_recording, status.reason.is_some()) {
                    (false, _) => self.on_stop(),
                    (true, true) => self.on_start_edge(),
                    (true, false) => {
                        if !self.decoded_in_segment {
                            self.reset("录音状态查询应答（本段尚无音频）");
                        }
                        self.recording = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_start_edge(&mut self) {
        if !self.recording || !self.decoded_in_segment {
            self.reset("录音开始");
        }
        self.recording = true;
    }

    fn on_stop(&mut self) {
        self.recording = false;
        self.decoded_in_segment = false;
    }

    fn reset(&mut self, why: &str) {
        tracing::debug!(target: "dyj.audio", "🎙️ 复位 ADPCM 解码器：{why}");
        self.decoder = AdpcmDecoder::new();
        self.decoded_in_segment = false;
    }
}

fn event_from_frame(frame: &TextFrame) -> Option<DeviceEvent> {
    // 认得命令名却解析不了载荷时退化成 Unhandled，而不是悄悄丢掉。
    let unhandled = || DeviceEvent::Unhandled {
        command: frame.command.clone(),
        payload: frame.payload.clone(),
    };
    match frame.command.as_str() {
        "BTN_CLICK" => Some(
            ButtonEvent::from_payload(&frame.payload)
                .map(DeviceEvent::Button)
                .unwrap_or_else(unhandled),
        ),
        "TOGGLE_SWITCH" => Some(
            ToggleSwitchEvent::from_payload(&frame.payload)
                .map(DeviceEvent::ToggleSwitch)
                .unwrap_or_else(unhandled),
        ),
        // RECORD_STATUS 也是查询响应：失败响应（只有 code/message）解析不出状态，
        // 不是事件，直接忽略。
        "RECORD_STATUS" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::RecordStatus),
        "WIFI_LIST" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::WifiNetwork),
        "WIFI_OFF" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::WifiOff),
        "UPLOAD_COMPLETE" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::UploadComplete),
        "OTA_WIFI_PROCESS" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::WifiOtaProgress),
        "WIFI_AP_START" => serde_json::from_value(frame.payload.clone())
            .ok()
            .map(DeviceEvent::AccessPointReady),
        // 0.3.7 起电源通知按标准事件框架 `POWER:{json}` 发送；旧固件的裸 JSON 形态
        // 见 event_from_raw，两者映射到同一个事件，应用无需关心固件版本。
        "POWER" => Some(
            serde_json::from_value::<PowerEvent>(frame.payload.clone())
                .map(DeviceEvent::Power)
                .unwrap_or_else(|_| unhandled()),
        ),
        _ => None,
    }
}

/// 无命令封装的裸 JSON：现固件的电源管理直接写 BLE（`{"type":...}`），不走协议封装。
fn event_from_raw(value: Value) -> DeviceEvent {
    match serde_json::from_value::<PowerEvent>(value.clone()) {
        Ok(power) => DeviceEvent::Power(power),
        Err(_) => DeviceEvent::Unhandled {
            command: String::new(),
            payload: value,
        },
    }
}

fn response_code(value: &Value) -> Option<i64> {
    value.get("code").and_then(Value::as_i64)
}

/// 日志里的命令载荷：路由器密码（`WIFI_CONNECT`）、激活码（`AUTH_ACTIVE`）打码。
fn redacted_payload(payload: &Value) -> String {
    match payload {
        Value::Object(map) if map.contains_key("password") || map.contains_key("code") => {
            let mut masked = map.clone();
            for secret in ["password", "code"] {
                if let Some(value) = masked.get_mut(secret)
                    && value.is_string()
                {
                    *value = Value::String("***".into());
                }
            }
            Value::Object(masked).to_string()
        }
        other => other.to_string(),
    }
}

/// 写命令前排空应答队列里的积压项。
///
/// worker 把每个文本帧都送进应答队列（事件另走广播），空闲时设备主动推送的帧
/// （如带 `reason` 的 `RECORD_STATUS`）和上一个请求超时后才到的迟到应答会一直积压，
/// 被下一次同名请求当成自己的应答取走，之后每次错位一拍。持有 `incoming` 锁时
/// 积压项必然早于本次请求，不可能是它的应答，直接丢弃；只有连接已断要立刻失败。
fn discard_stale_inbound(
    incoming: &mut mpsc::UnboundedReceiver<Inbound>,
    command: &str,
) -> Result<()> {
    loop {
        match incoming.try_recv() {
            Ok(Inbound::Disconnected) | Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(SdkError::Disconnected);
            }
            Ok(Inbound::Text(frame)) => {
                tracing::debug!(target: "dyj.proto", "🧹 丢弃积压帧 cmd={command} stale={} body={}", frame.command, frame.payload);
            }
            Ok(Inbound::Binary(bytes)) => {
                tracing::debug!(target: "dyj.proto", "🧹 丢弃积压二进制块 cmd={command} {}字节", bytes.len());
            }
            Ok(Inbound::Fault(message)) => {
                tracing::debug!(target: "dyj.proto", "🧹 丢弃积压解析错误 cmd={command} {message}");
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
        }
    }
}

/// 与查询应答同名的主动推送：`RECORD_STATUS` 推送带 `reason`，查询应答不带。
/// 请求等待期间设备可能恰好推送一条，不能当作本次查询的应答。
fn is_unsolicited_push(frame: &TextFrame) -> bool {
    frame.command == "RECORD_STATUS" && frame.payload.get("reason").is_some()
}

async fn receive_next(
    incoming: &mut mpsc::UnboundedReceiver<Inbound>,
    wait: Duration,
    command: &str,
) -> Result<Inbound> {
    match timeout(wait, incoming.recv()).await {
        Ok(Some(Inbound::Fault(message))) => Err(SdkError::Protocol(message)),
        Ok(Some(Inbound::Disconnected)) | Ok(None) => Err(SdkError::Disconnected),
        Ok(Some(message)) => Ok(message),
        Err(_) => {
            tracing::error!(target: "dyj.device", "⏱️ 命令超时 cmd={command} 等待={}s", wait.as_secs());
            Err(SdkError::Timeout {
                command: command.to_owned(),
                seconds: wait.as_secs(),
            })
        }
    }
}

fn ensure_success(command: &str, value: &Value) -> Result<()> {
    if let Some(code) = value.get("code").and_then(Value::as_i64)
        && code != 0
    {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown device error");
        tracing::error!(target: "dyj.device", "❌ 设备返回错误 cmd={command} code={code} message={message}");
        return Err(SdkError::Device {
            command: command.to_owned(),
            code: code as i32,
            message: message.to_owned(),
        });
    }
    Ok(())
}

fn required_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| SdkError::InvalidFrame(format!("missing or invalid `{key}`")))
}

/// 设备 KV 配置键的最大字节数（nRF `handler_sys.c` 解析缓冲 `key[32]`）。
pub const CONFIG_KEY_MAX_BYTES: usize = 31;
/// 设备 KV 配置值的最大字节数（nRF `handler_sys.c` 解析缓冲 `value[64]`）。
pub const CONFIG_VALUE_MAX_BYTES: usize = 63;

/// 该配置键是否由 nRF 透传给 ESP32。与固件 `strip_wifi_key` 规则一致：
/// `wifi_` 前缀 / `_wifi` 后缀，外加 WiFi 0.1.0 起的短键
/// `upload_url` / `api_url` / `ota_path`（NVS 键名 15 字符硬限所致）。
pub fn config_key_routes_to_wifi_module(key: &str) -> bool {
    matches!(key, "upload_url" | "api_url" | "ota_path")
        || key.starts_with("wifi_")
        || key.ends_with("_wifi")
}

/// OLED 可见区点阵字节数：72×40 = 5 页 × 72 列，纵向字节、bit0 为页顶。
pub const DISPLAY_BITMAP_BYTES: usize = 360;
/// OLED 文本模式最大行数（可见区 40px 高 / 8px 行高）。
pub const DISPLAY_MAX_LINES: usize = 5;
/// OLED 每行可见字符数（5×8 字体 + 1px 间距，72px 宽）。
pub const DISPLAY_LINE_CHARS: usize = 12;
/// 显示超时上限（毫秒），与固件 `handler_display.c` 一致。
pub const DISPLAY_TIMEOUT_MAX_MS: u32 = 3_600_000;

/// `display_show` 的请求内容：`bitmap`（72×40 单色点阵）与 `lines`（ASCII 文本）
/// 至少给一个，同时存在时固件按 bitmap 显示。
///
/// 点阵由调用方渲染（网页 canvas、字体栅格化、二维码等任意来源），
/// 布局与固件文本路径一致：按列字节，每字节 bit0 为该 8 行页的顶行。
#[derive(Debug, Clone, Default)]
pub struct DisplayShowRequest {
    pub lines: Vec<String>,
    pub bitmap: Option<Vec<u8>>,
    pub persistent: bool,
    pub timeout_ms: u32,
}

impl DisplayShowRequest {
    /// 文本模式：每行 ≤ [`DISPLAY_LINE_CHARS`] 个可打印 ASCII 字符，最多 [`DISPLAY_MAX_LINES`] 行。
    pub fn text(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            lines: lines.into_iter().map(Into::into).collect(),
            bitmap: None,
            persistent: false,
            timeout_ms: 5000,
        }
    }

    /// 点阵模式：`bytes` 必须恰好 [`DISPLAY_BITMAP_BYTES`] 字节。
    pub fn bitmap(bytes: Vec<u8>) -> Self {
        Self {
            lines: Vec::new(),
            bitmap: Some(bytes),
            persistent: false,
            timeout_ms: 5000,
        }
    }

    /// 常显（不自动清屏，直到系统界面覆盖或 `display_clear`）。
    pub fn persistent(mut self, on: bool) -> Self {
        self.persistent = on;
        self
    }

    /// 非常显时的自动清屏时长。
    pub fn timeout_ms(mut self, ms: u32) -> Self {
        self.timeout_ms = ms;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.lines.is_empty() && self.bitmap.is_none() {
            return Err(SdkError::InvalidCommand(
                "display_show requires `lines` or `bitmap`".into(),
            ));
        }
        if self.lines.len() > DISPLAY_MAX_LINES {
            return Err(SdkError::InvalidCommand(format!(
                "display_show accepts at most {DISPLAY_MAX_LINES} lines, got {}",
                self.lines.len()
            )));
        }
        for line in &self.lines {
            if line.chars().count() > DISPLAY_LINE_CHARS {
                return Err(SdkError::InvalidCommand(format!(
                    "display line `{line}` exceeds the visible {DISPLAY_LINE_CHARS} characters"
                )));
            }
            if !line.chars().all(|c| (' '..='~').contains(&c)) {
                return Err(SdkError::InvalidCommand(
                    "display lines accept printable ASCII only; render other glyphs into `bitmap`"
                        .into(),
                ));
            }
        }
        if let Some(bitmap) = &self.bitmap
            && bitmap.len() != DISPLAY_BITMAP_BYTES
        {
            return Err(SdkError::InvalidCommand(format!(
                "display bitmap must be exactly {DISPLAY_BITMAP_BYTES} bytes (72x40), got {}",
                bitmap.len()
            )));
        }
        if !(1..=DISPLAY_TIMEOUT_MAX_MS).contains(&self.timeout_ms) {
            return Err(SdkError::InvalidCommand(format!(
                "display timeout_ms must be within 1..={DISPLAY_TIMEOUT_MAX_MS}"
            )));
        }
        Ok(())
    }
}

fn validate_config_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(SdkError::InvalidCommand(
            "config key must not be empty".into(),
        ));
    }
    if key.len() > CONFIG_KEY_MAX_BYTES {
        return Err(SdkError::InvalidCommand(format!(
            "config key `{key}` is {} bytes; the device accepts at most {CONFIG_KEY_MAX_BYTES}",
            key.len()
        )));
    }
    Ok(())
}

fn validate_config_value(value: &str) -> Result<()> {
    if value.len() > CONFIG_VALUE_MAX_BYTES {
        return Err(SdkError::InvalidCommand(format!(
            "config value is {} bytes; the device accepts at most {CONFIG_VALUE_MAX_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &str, what: &str) -> Result<()> {
    if !path.starts_with('/') {
        return Err(SdkError::InvalidCommand(format!(
            "{what} must begin with `/`"
        )));
    }
    Ok(())
}

fn validate_url_scheme(value: &str, allowed_schemes: &[&str]) -> Result<()> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| SdkError::InvalidCommand(format!("invalid URL: {error}")))?;
    if allowed_schemes.contains(&url.scheme()) {
        Ok(())
    } else {
        Err(SdkError::InvalidCommand(format!(
            "URL scheme must be one of {}",
            allowed_schemes.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        task::{Context, Poll},
    };

    use async_trait::async_trait;
    use futures::stream;
    use tokio::{io::AsyncWrite, sync::Mutex};

    use super::*;
    use crate::transport::{BleTransport, NotificationStream};

    struct MockTransport {
        sent: tokio::sync::mpsc::UnboundedSender<Result<Vec<u8>>>,
        received: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Result<Vec<u8>>>>>,
        write_buffer: Mutex<Vec<u8>>,
        commands: Mutex<Vec<String>>,
        /// 完整的发送行（含载荷），供断言 wire 格式。
        lines: Mutex<Vec<String>>,
        /// 模拟固件 setting_kv：CONFIG_SET/GET/DEL 的后备存储。
        config: std::sync::Mutex<std::collections::HashMap<String, String>>,
        announced_file_size: AtomicU64,
        wifi_open: AtomicBool,
        silent: AtomicBool,
        /// 模拟固件录音状态：RECORD_START 置位、RECORD_STOP 复位，RECORD_STATUS 查询据此应答。
        recording: AtomicBool,
    }

    impl MockTransport {
        fn new() -> Self {
            let (sent, received) = tokio::sync::mpsc::unbounded_channel();
            Self {
                sent,
                received: Mutex::new(Some(received)),
                write_buffer: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
                lines: Mutex::new(Vec::new()),
                config: std::sync::Mutex::new(Default::default()),
                announced_file_size: AtomicU64::new(3),
                wifi_open: AtomicBool::new(false),
                silent: AtomicBool::new(false),
                recording: AtomicBool::new(false),
            }
        }

        /// 让后续命令无人应答（挂起请求用）。
        fn go_silent(&self) {
            self.silent.store(true, Ordering::Relaxed);
        }

        /// 向通知流注入一个错误，模拟底层 BLE 流中断。
        fn fail_stream(&self) {
            let _ = self
                .sent
                .send(Err(SdkError::Transport("mock stream failure".into())));
        }

        fn mismatched_file() -> Self {
            let transport = Self::new();
            transport.announced_file_size.store(4, Ordering::Relaxed);
            transport
        }

        fn reply(&self, frame: &[u8]) {
            let _ = self.sent.send(Ok(frame.to_vec()));
        }

        fn handle_command(&self, command: &str, payload: Option<&str>) {
            if self.silent.load(Ordering::Relaxed) {
                return;
            }
            let payload_json = payload
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            let key = payload_json
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            match command {
                "SYNC_TIME" => self.reply(b"SYNC_TIME:{\"code\":0}\r\n"),
                "STATE_INFO" => {
                    let open = self.wifi_open.load(Ordering::Relaxed);
                    self.reply(
                        format!(
                            "STATE_INFO:{{\"authState\":false,\"recordRealtimeEnabled\":false,\"wifiOpen\":{open},\"bootType\":\"normal\",\"lowPowerMode\":false}}\r\n"
                        )
                        .as_bytes(),
                    );
                }
                "WIFI_STATUS" => {
                    self.reply(b"WIFI_STATUS:{\"version\":\"0.1.0\",\"versionCode\":100,\"status\":\"connected\",\"enabled\":true,\"connected\":true,\"mode\":\"sta\",\"ssid\":\"home\",\"ip\":\"192.168.1.23\",\"rssi\":-52}\r\n");
                }
                "BASE_INFO" => self.reply(b"BASE_INFO:{\"version\":\"0.3.6\",\"versionCode\":306,\"deviceId\":\"DYJ1\",\"deviceName\":\"DYJ\",\"timestamp\":1}\r\n"),
                "FILE_LIST" => {
                    self.reply(b"FILE_LIST:{\"index\":0,\"name\":\"a.apm\",\"size\":3}\r\n");
                    self.reply(b"FILE_LIST:{\"index\":-1}\r\n");
                }
                "FILE_UPLOAD" => {
                    self.reply(format!("FILE_UPLOAD:{{\"state\":\"start\",\"name\":\"taken.apm\",\"size\":{}}}\r\n", self.announced_file_size.load(Ordering::Relaxed)).as_bytes());
                    self.reply(&[0, b'a', b'b']);
                    self.reply(&[0, b'c']);
                    self.reply(b"FILE_UPLOAD:{\"state\":\"end\"}\r\n");
                }
                "RECORD_START" => {
                    self.recording.store(true, Ordering::Relaxed);
                    self.reply(b"RECORD_START:{\"code\":0}\r\n");
                }
                "RECORD_STOP" => {
                    self.recording.store(false, Ordering::Relaxed);
                    self.reply(b"RECORD_STOP:{\"code\":0}\r\n");
                }
                // 查询应答不带 reason（与固件一致）。
                "RECORD_STATUS" => {
                    let recording = self.recording.load(Ordering::Relaxed);
                    self.reply(
                        format!("RECORD_STATUS:{{\"isRecording\":{recording},\"realtimeEnabled\":true}}\r\n")
                            .as_bytes(),
                    );
                }
                "RECORD_MARK" => self.reply(b"RECORD_MARK:{\"code\":0,\"filename\":\"059F19DF.APM\",\"timestamp\":53}\r\n"),
                "AP_START" => self.reply(b"AP_START:{\"code\":0,\"ip\":\"192.168.4.1\",\"webPort\":80}\r\n"),
                "AP_STATUS" => self.reply(b"AP_STATUS:{\"status\":\"active\",\"active\":true,\"ip\":\"192.168.4.1\",\"webServerRunning\":true,\"webPort\":80}\r\n"),
                "TEST_CHECK" => {
                    for index in 0..11 {
                        self.reply(format!("TEST_CHECK:{{\"key\":\"M{index}\",\"result\":true}}\r\n").as_bytes());
                    }
                }
                // 与固件 handler_sys.c 一致：缺键回 code 3 "not found"。
                "CONFIG_SET" => {
                    let value = payload_json
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    self.config.lock().unwrap().insert(key, value);
                    self.reply(b"CONFIG_SET:{\"code\":0,\"message\":\"ok\"}\r\n");
                }
                "CONFIG_GET" => match self.config.lock().unwrap().get(&key) {
                    Some(value) => self.reply(
                        format!("CONFIG_GET:{{\"key\":\"{key}\",\"value\":\"{value}\"}}\r\n")
                            .as_bytes(),
                    ),
                    // ESP32（wifi_ 路由键）对缺键回空字符串，nRF 回 code 3。
                    None if config_key_routes_to_wifi_module(&key) => self.reply(
                        format!("CONFIG_GET:{{\"key\":\"{key}\",\"value\":\"\"}}\r\n").as_bytes(),
                    ),
                    None => self.reply(b"CONFIG_GET:{\"code\":3,\"message\":\"not found\"}\r\n"),
                },
                "OTA_WIFI_CHECK" => self.reply(b"OTA_WIFI_CHECK:{\"updateAvailable\":true}\r\n"),
                "OTA_WIFI_STATUS" => self.reply(b"OTA_WIFI_STATUS:{\"status\":\"IDLE\",\"progress\":0,\"total\":0,\"percentage\":0}\r\n"),
                "OTA_WIFI_UPDATE" => {
                    self.reply(b"OTA_WIFI_UPDATE:{\"code\":0,\"message\":\"ota started\"}\r\n");
                    self.reply(b"OTA_WIFI_PROCESS:{\"status\":\"DOWNLOADING\",\"progress\":0,\"total\":100,\"percentage\":0}\r\n");
                    self.reply(b"OTA_WIFI_PROCESS:{\"status\":\"DOWNLOADING\",\"progress\":50,\"total\":100,\"percentage\":50}\r\n");
                    self.reply(b"OTA_WIFI_PROCESS:{\"status\":\"SUCCESS\",\"progress\":100,\"total\":100,\"percentage\":100}\r\n");
                }
                "CONFIG_DEL" => match self.config.lock().unwrap().remove(&key) {
                    Some(_) => self.reply(b"CONFIG_DEL:{\"code\":0,\"message\":\"ok\"}\r\n"),
                    // ESP32（wifi_ 路由键）对不存在的键回 code 1，nRF 回 code 3。
                    None if config_key_routes_to_wifi_module(&key) => {
                        self.reply(b"CONFIG_DEL:{\"code\":1,\"message\":\"delete failed\"}\r\n")
                    }
                    None => self.reply(b"CONFIG_DEL:{\"code\":3,\"message\":\"not found\"}\r\n"),
                },
                _ => self.reply(format!("{command}:{{\"code\":0}}\r\n").as_bytes()),
            }
        }
    }

    #[async_trait]
    impl BleTransport for MockTransport {
        async fn write(&self, bytes: &[u8]) -> Result<()> {
            let mut buffer = self.write_buffer.lock().await;
            buffer.extend_from_slice(bytes);
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                let text = std::str::from_utf8(&line).unwrap().trim();
                let (command, payload) = match text.split_once(':') {
                    Some((command, payload)) => (command.to_owned(), Some(payload)),
                    None => (text.to_owned(), None),
                };
                self.commands.lock().await.push(command.clone());
                self.lines.lock().await.push(text.to_owned());
                self.handle_command(&command, payload);
            }
            Ok(())
        }

        async fn notifications(&self) -> Result<NotificationStream> {
            let receiver = self
                .received
                .lock()
                .await
                .take()
                .expect("only one notification worker");
            Ok(Box::pin(stream::unfold(
                receiver,
                |mut receiver| async move { receiver.recv().await.map(|next| (next, receiver)) },
            )))
        }

        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    struct VecWriter(Vec<u8>);
    impl AsyncWrite for VecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn constructor_syncs_time_before_exposing_device() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let info = device.device_info().await.unwrap();
        assert_eq!(info.device_id, "DYJ1");
        assert_eq!(transport.commands.lock().await.as_slice()[0], "SYNC_TIME");
    }

    #[tokio::test]
    async fn collects_list_until_sentinel() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let files = device.list_files(None).await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name.as_deref(), Some("a.apm"));
    }

    #[tokio::test]
    async fn takes_tagged_binary_file_and_verifies_length() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let mut writer = VecWriter(Vec::new());
        let result = device.take_file_to("a.apm", &mut writer).await.unwrap();
        assert_eq!(result.name, "taken.apm");
        assert_eq!(result.bytes_written, 3);
        assert_eq!(writer.0, b"abc");
    }

    #[tokio::test]
    async fn take_file_to_dir_saves_under_the_directory() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "reai-card-sdk-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let saved = device.take_file_to_dir("a.apm", &dir).await.unwrap();
        // mock 的 start 帧 name 是 taken.apm（无时间戳）→ 落盘沿用请求的原名
        assert_eq!(saved.path, dir.join("a.apm"));
        assert_eq!(saved.device_name.as_deref(), Some("taken.apm"));
        assert!(saved.stamp.is_none());
        assert_eq!(std::fs::read(&saved.path).unwrap(), b"abc");
        assert!(!dir.join("a.apm.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_incomplete_file_transfer() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::mismatched_file()))
            .await
            .unwrap();
        let mut writer = VecWriter(Vec::new());
        let error = device.take_file_to("a.apm", &mut writer).await.unwrap_err();
        assert!(matches!(
            error,
            SdkError::FileLengthMismatch {
                expected: 4,
                actual: 3
            }
        ));
        assert_eq!(writer.0, b"abc");
    }

    #[tokio::test]
    async fn collects_all_firmware_module_checks_without_a_sentinel() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let checks = device.test_modules().await.unwrap();
        assert_eq!(checks.len(), 11);
        assert_eq!(checks[10].key, "M10");
    }

    #[tokio::test]
    async fn wifi_status_skips_the_wire_while_the_module_is_powered_down() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let status = device.wifi_status().await.unwrap();
        assert_eq!(status.status, "off");
        assert!(!status.enabled);
        let commands = transport.commands.lock().await;
        assert!(commands.contains(&"STATE_INFO".to_owned()));
        assert!(!commands.contains(&"WIFI_STATUS".to_owned()));
    }

    #[tokio::test]
    async fn wifi_status_queries_the_module_once_wifi_is_open() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let status = device.wifi_status().await.unwrap();
        assert_eq!(status.status, "connected");
        assert_eq!(status.ssid.as_deref(), Some("home"));
        assert!(
            transport
                .commands
                .lock()
                .await
                .contains(&"WIFI_STATUS".to_owned())
        );
    }

    #[tokio::test]
    async fn open_wifi_skips_the_wire_while_module_already_powered() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        assert!(!device.access_point_ready());
        let response = device.open_wifi().await.unwrap();
        assert_eq!(response.code, Some(0));
        // 断链重连场景：模块仍供电，AP 随上电自启，直接视为就绪
        assert!(device.access_point_ready());
        let commands = transport.commands.lock().await;
        assert!(commands.contains(&"STATE_INFO".to_owned()));
        assert!(!commands.contains(&"WIFI_OPEN".to_owned()));
    }

    #[tokio::test]
    async fn sd_operations_fail_fast_while_wifi_module_owns_the_card() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let error = device.storage().await.unwrap_err();
        assert!(
            matches!(error, SdkError::SdOwnedByWifiModule { ref command } if command == "STORAGE")
        );
        let error = device.list_files(None).await.unwrap_err();
        assert!(
            matches!(error, SdkError::SdOwnedByWifiModule { ref command } if command == "FILE_LIST")
        );
        let error = device
            .take_file_to("a.apm", &mut VecWriter(Vec::new()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, SdkError::SdOwnedByWifiModule { ref command } if command == "FILE_UPLOAD")
        );
        let commands = transport.commands.lock().await;
        for blocked in ["STORAGE", "FILE_LIST", "FILE_UPLOAD"] {
            assert!(
                !commands.contains(&blocked.to_owned()),
                "{blocked} 不应上线"
            );
        }
    }

    #[tokio::test]
    async fn esp32_commands_fail_fast_while_module_is_powered_down() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let error = device.start_access_point(None, None).await.unwrap_err();
        assert!(
            matches!(error, SdkError::WifiModulePoweredDown { ref command } if command == "AP_START")
        );
        let error = device.scan_wifi().await.unwrap_err();
        assert!(
            matches!(error, SdkError::WifiModulePoweredDown { ref command } if command == "WIFI_SCAN")
        );
        // 预检拦截后，透传命令不应上线
        let commands = transport.commands.lock().await;
        assert!(!commands.contains(&"AP_START".to_owned()));
        assert!(!commands.contains(&"WIFI_SCAN".to_owned()));
    }

    #[tokio::test]
    async fn access_point_client_uses_default_endpoint_after_ready_event() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 固件就绪事件：v0.3.x 上 AP_STATUS 根本不该上线，客户端用固定端点。
        let frame = "WIFI_AP_START:{\"code\":0,\"message\":\"AP已启动\"}\r\n";
        transport.reply(frame.as_bytes());
        let client = device.access_point_client().await.unwrap();
        assert_eq!(client.base_url().as_str(), "http://192.168.4.1/");
        let commands = transport.commands.lock().await;
        assert!(!commands.contains(&"AP_STATUS".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn access_point_client_infers_ap_up_when_module_already_powered() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 全程没有 WIFI_AP_START：模拟连接建立时模块早已供电、事件早已发过的重连场景
        let client = device.access_point_client().await.unwrap();
        assert_eq!(client.base_url().as_str(), "http://192.168.4.1/");
        assert!(device.access_point_ready());
    }

    #[tokio::test]
    async fn wait_for_ap_ready_times_out_without_the_event() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport).await.unwrap();
        let error = device
            .wait_for_ap_ready(Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            matches!(error, SdkError::Timeout { ref command, seconds: 0 } if command == "WIFI_AP_START")
        );
    }

    #[tokio::test]
    async fn controls_access_point_through_the_uart_bridge() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport).await.unwrap();
        let started = device.start_access_point(None, None).await.unwrap();
        assert_eq!(started.ip.as_deref(), Some("192.168.4.1"));
        let status = device.access_point_status().await.unwrap();
        assert_eq!(status.web_port, Some(80));
    }

    #[tokio::test]
    async fn routes_hardware_event_while_waiting_for_command() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        transport.reply(b"BTN_CLICK:{\"key\":\"single\"}\r\n");
        let _ = device.start_recording().await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Button(_)
        ));
    }

    #[tokio::test]
    async fn button_and_switch_events_are_typed() {
        use crate::controls::{ButtonGesture, SwitchPosition};

        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        transport.reply(b"BTN_CLICK:{\"key\":\"long_press\",\"action\":\"record_toggle\"}\r\n");
        transport.reply(b"TOGGLE_SWITCH:{\"state\":\"off\",\"action\":\"record_stop\"}\r\n");
        // 未来固件新增的手势：不丢，退化成 Unhandled 原样交给应用。
        transport.reply(b"BTN_CLICK:{\"key\":\"triple\",\"action\":\"none\"}\r\n");

        match events.recv().await.unwrap() {
            DeviceEvent::Button(event) => {
                assert_eq!(event.gesture, ButtonGesture::LongPress);
                assert_eq!(event.action, ControlAction::RecordToggle);
            }
            other => panic!("unexpected {other:?}"),
        }
        match events.recv().await.unwrap() {
            DeviceEvent::ToggleSwitch(event) => {
                assert_eq!(event.position, SwitchPosition::Off);
                assert_eq!(event.action, ControlAction::RecordStop);
            }
            other => panic!("unexpected {other:?}"),
        }
        match events.recv().await.unwrap() {
            DeviceEvent::Unhandled { command, payload } => {
                assert_eq!(command, "BTN_CLICK");
                assert_eq!(payload["key"], "triple");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_status_push_carries_reason_and_error() {
        use crate::types::RecordReason;

        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"switch\"}\r\n");
        transport.reply(b"RECORD_STATUS:{\"isRecording\":false,\"realtimeEnabled\":true,\"reason\":\"auto\",\"error\":7}\r\n");

        match events.recv().await.unwrap() {
            DeviceEvent::RecordStatus(status) => {
                assert!(status.is_recording);
                assert_eq!(status.reason, Some(RecordReason::Switch));
                assert_eq!(status.error, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        match events.recv().await.unwrap() {
            DeviceEvent::RecordStatus(status) => {
                assert!(!status.is_recording);
                assert_eq!(status.reason, Some(RecordReason::Auto));
                assert_eq!(status.error, Some(7));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn unframed_power_message_becomes_an_event_without_eating_the_next_response() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        // 固件电源管理直接写 BLE：无命令名、无换行。紧接着的 BASE_INFO 响应必须完好。
        transport.reply(b"{\"type\":\"shutdown_imminent\",\"seconds\":5}");
        let info = device.device_info().await.unwrap();
        assert_eq!(info.device_id, "DYJ1");
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Power(PowerEvent::ShutdownImminent { seconds: 5 })
        ));

        transport.reply(b"{\"type\":\"device_wakeup\"}");
        transport.reply(b"{\"type\":\"mystery\",\"x\":1}");
        let _ = device.device_info().await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Power(PowerEvent::DeviceWakeup)
        ));
        match events.recv().await.unwrap() {
            DeviceEvent::Unhandled { command, payload } => {
                assert!(command.is_empty());
                assert_eq!(payload["type"], "mystery");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn framed_power_notification_maps_to_the_same_event() {
        // 未来固件若按协议格式封装电源通知（POWER:{json}\r\n），应用拿到的事件不变。
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        transport.reply(b"POWER:{\"type\":\"shutdown_imminent\",\"seconds\":5}\r\n");
        transport.reply(b"POWER:{\"type\":\"mystery\"}\r\n");
        let _ = device.device_info().await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Power(PowerEvent::ShutdownImminent { seconds: 5 })
        ));
        match events.recv().await.unwrap() {
            DeviceEvent::Unhandled { command, payload } => {
                assert_eq!(command, "POWER");
                assert_eq!(payload["type"], "mystery");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn charging_event_parses_from_power_frame() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut events = device.subscribe_events();
        transport.reply(b"POWER:{\"type\":\"charging\",\"charging\":true,\"level\":80}\r\n");
        transport.reply(b"POWER:{\"type\":\"charging\",\"charging\":false}\r\n");
        let _ = device.device_info().await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Power(PowerEvent::Charging {
                charging: true,
                level: Some(80)
            })
        ));
        assert!(matches!(
            events.recv().await.unwrap(),
            DeviceEvent::Power(PowerEvent::Charging {
                charging: false,
                level: None
            })
        ));
    }

    #[tokio::test]
    async fn control_bindings_round_trip_through_device_config() {
        use crate::controls::{ButtonGesture, SwitchPosition};

        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();

        // 出厂：没有任何键，读到的全是 None，且不报错。
        assert_eq!(
            device.control_action(ButtonGesture::Single).await.unwrap(),
            ControlAction::None
        );
        assert_eq!(
            device.control_bindings().await.unwrap(),
            ControlBindings::default()
        );

        device
            .set_control_action(SwitchPosition::On, ControlAction::RecordStart)
            .await
            .unwrap();
        device
            .set_control_action(ButtonGesture::Double, ControlAction::Other("future".into()))
            .await
            .unwrap();
        {
            let config = transport.config.lock().unwrap();
            assert_eq!(
                config.get("switch_on").map(String::as_str),
                Some("record_start")
            );
            assert_eq!(config.get("btn_double").map(String::as_str), Some("future"));
        }
        assert_eq!(
            device.control_action(SwitchPosition::On).await.unwrap(),
            ControlAction::RecordStart
        );
        assert_eq!(
            device.control_action(ButtonGesture::Double).await.unwrap(),
            ControlAction::Other("future".into())
        );

        let recorder = ControlBindings::recorder();
        device.apply_control_bindings(&recorder).await.unwrap();
        assert_eq!(device.control_bindings().await.unwrap(), recorder);
        assert_eq!(
            transport
                .config
                .lock()
                .unwrap()
                .get("btn_long_press")
                .map(String::as_str),
            Some("none"),
            "None 也要显式落盘为 none"
        );

        // 解绑是幂等的：第二次删除同一键固件回 not found，SDK 不报错。
        device
            .clear_control_action(SwitchPosition::On)
            .await
            .unwrap();
        device
            .clear_control_action(SwitchPosition::On)
            .await
            .unwrap();
        assert!(!transport.config.lock().unwrap().contains_key("switch_on"));
        assert_eq!(
            device.control_action(SwitchPosition::On).await.unwrap(),
            ControlAction::None
        );
    }

    #[tokio::test]
    async fn record_mark_is_typed() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let mark = device.record_mark().await.unwrap();
        assert_eq!(mark.filename, "059F19DF.APM");
        assert_eq!(mark.offset_secs, 53);
    }

    #[tokio::test]
    async fn wifi_status_carries_live_module_version() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // wifi_status() 先查 STATE_INFO，模块未上电时合成 off 不上线；先上电再查
        transport.wifi_open.store(true, Ordering::Relaxed);
        let status = device.wifi_status().await.unwrap();
        assert_eq!(status.version.as_deref(), Some("0.1.0"));
        assert_eq!(status.version_code, Some(100));
        assert_eq!(status.status, "connected");
    }

    #[tokio::test]
    async fn config_key_and_value_lengths_are_rejected_locally() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let long_key = "k".repeat(CONFIG_KEY_MAX_BYTES + 1);
        let long_value = "v".repeat(CONFIG_VALUE_MAX_BYTES + 1);
        assert!(matches!(
            device.set_config(&long_key, "v").await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device.set_config("k", &long_value).await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device.set_config("", "v").await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device.config_value(&long_key).await,
            Err(SdkError::InvalidCommand(_))
        ));
        // 边界值放行
        device
            .set_config(
                &"k".repeat(CONFIG_KEY_MAX_BYTES),
                &"v".repeat(CONFIG_VALUE_MAX_BYTES),
            )
            .await
            .unwrap();
        let commands = transport.commands.lock().await;
        assert_eq!(commands.iter().filter(|c| *c == "CONFIG_SET").count(), 1);
        assert!(!commands.contains(&"CONFIG_GET".to_owned()));
    }

    #[tokio::test]
    async fn wifi_routed_config_keys_need_the_module_and_read_empty_as_none() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 模块未上电：透传键快速失败，不上线
        assert!(matches!(
            device.sta_api_base_url().await,
            Err(SdkError::WifiModulePoweredDown { .. })
        ));
        assert!(matches!(
            device.set_wifi_ota_info_path("/ota/info").await,
            Err(SdkError::WifiModulePoweredDown { .. })
        ));
        assert!(
            !transport
                .commands
                .lock()
                .await
                .contains(&"CONFIG_GET".to_owned())
        );

        transport.wifi_open.store(true, Ordering::Relaxed);
        // ESP32 对缺键回 value:""，SDK 归一为 None
        assert_eq!(device.sta_api_base_url().await.unwrap(), None);
        assert_eq!(device.wifi_ota_info_path().await.unwrap(), None);
        device
            .set_sta_api_base_url("https://api.example.com")
            .await
            .unwrap();
        device
            .set_wifi_ota_info_path("/ota/info?mod=wifi")
            .await
            .unwrap();
        assert_eq!(
            device.sta_api_base_url().await.unwrap().as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(
            device.wifi_ota_info_path().await.unwrap().as_deref(),
            Some("/ota/info?mod=wifi")
        );
        assert!(matches!(
            device.set_wifi_ota_info_path("ota/info").await,
            Err(SdkError::InvalidCommand(_))
        ));
        // nRF 侧键缺失仍是 None，且空字符串不会被误归一
        assert_eq!(device.config_value("btn_single").await.unwrap(), None);
        device.set_config("app_note", "").await.unwrap();
        assert_eq!(
            device.config_value("app_note").await.unwrap().as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn clear_config_is_idempotent_on_both_stores() {
        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // nRF 键：缺失 → false（固件 code 3）
        assert!(!device.clear_config("btn_single").await.unwrap());
        device
            .set_config("btn_single", "record_mark")
            .await
            .unwrap();
        assert!(device.clear_config("btn_single").await.unwrap());
        // ESP32 键：缺失时固件回 code 1，直接 delete_config 会报错，clear_config 先读再删
        assert!(matches!(
            device.delete_config("api_url").await,
            Err(SdkError::Device { code: 1, .. })
        ));
        assert!(!device.clear_config("api_url").await.unwrap());
        device
            .set_sta_api_base_url("https://dyj2.dev.reai.com/api")
            .await
            .unwrap();
        assert!(device.clear_config("api_url").await.unwrap());
        assert_eq!(device.sta_api_base_url().await.unwrap(), None);
    }

    #[tokio::test]
    async fn command_log_redacts_passwords() {
        assert_eq!(
            redacted_payload(&json!({"ssid":"home","password":"secret"})),
            r#"{"password":"***","ssid":"home"}"#
        );
        assert_eq!(
            redacted_payload(&json!({"deviceId":"D1","deviceName":"n","code":"ABCD"})),
            r#"{"code":"***","deviceId":"D1","deviceName":"n"}"#
        );
        // 数值型 code（响应里的错误码）不属于密钥，不打码
        assert_eq!(redacted_payload(&json!({"code":3})), r#"{"code":3}"#);
    }

    #[tokio::test]
    async fn wifi_ota_check_is_typed_and_update_follows_progress_to_the_end() {
        use crate::types::OtaStatus;

        let transport = Arc::new(MockTransport::new());
        transport.wifi_open.store(true, Ordering::Relaxed);
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        assert!(device.check_wifi_ota().await.unwrap().update_available);
        let status = device.wifi_ota_status().await.unwrap();
        assert_eq!(status.status, OtaStatus::Idle);

        let final_progress = device
            .update_wifi_firmware(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(final_progress.status, OtaStatus::Success);
        assert_eq!(final_progress.percentage, 100);
        assert!(final_progress.status.is_terminal());
    }

    #[tokio::test]
    async fn led_state_goes_out_as_wire_name() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        device.set_led(LedState::Toggle).await.unwrap();
        assert!(
            transport
                .lines
                .lock()
                .await
                .contains(&"LED:{\"state\":\"toggle\"}".to_owned())
        );
    }

    #[tokio::test]
    async fn display_show_text_frame_carries_all_fields() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        device
            .display_show(&DisplayShowRequest::text(["HELLO", "DYJ"]).persistent(true))
            .await
            .unwrap();
        let sent = transport
            .lines
            .lock()
            .await
            .iter()
            .find(|line| line.starts_with("DISPLAY_SHOW:"))
            .cloned()
            .unwrap();
        let payload: Value =
            serde_json::from_str(sent.strip_prefix("DISPLAY_SHOW:").unwrap()).unwrap();
        assert_eq!(payload["lines"], json!(["HELLO", "DYJ"]));
        assert_eq!(payload["persistent"], json!(true));
        assert_eq!(payload["timeoutMs"], json!(5000));
    }

    #[tokio::test]
    async fn display_show_bitmap_is_base64_encoded() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        device
            .display_show(
                &DisplayShowRequest::bitmap(vec![0u8; DISPLAY_BITMAP_BYTES]).timeout_ms(8000),
            )
            .await
            .unwrap();
        let sent = transport
            .lines
            .lock()
            .await
            .iter()
            .find(|line| line.starts_with("DISPLAY_SHOW:"))
            .cloned()
            .unwrap();
        let payload: Value =
            serde_json::from_str(sent.strip_prefix("DISPLAY_SHOW:").unwrap()).unwrap();
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(payload["bitmap"].as_str().unwrap())
                .unwrap()
                .len(),
            DISPLAY_BITMAP_BYTES
        );
        assert_eq!(payload["timeoutMs"], json!(8000));
        assert!(payload.get("lines").is_none());
    }

    #[tokio::test]
    async fn display_show_rejects_invalid_input_locally() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let before = transport.commands.lock().await.len();
        // 空、超行数、非 ASCII、超宽行、位图尺寸、超时范围
        assert!(matches!(
            device.display_show(&DisplayShowRequest::default()).await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device
                .display_show(&DisplayShowRequest::text(["a"; DISPLAY_MAX_LINES + 1]))
                .await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device
                .display_show(&DisplayShowRequest::text(["中文"]))
                .await,
            Err(SdkError::InvalidCommand(_))
        ));
        let too_long = "x".repeat(DISPLAY_LINE_CHARS + 1);
        assert!(matches!(
            device
                .display_show(&DisplayShowRequest::text([too_long]))
                .await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device
                .display_show(&DisplayShowRequest::bitmap(vec![0u8; 359]))
                .await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert!(matches!(
            device
                .display_show(&DisplayShowRequest::text(["ok"]).timeout_ms(0))
                .await,
            Err(SdkError::InvalidCommand(_))
        ));
        assert_eq!(transport.commands.lock().await.len(), before);
    }

    #[tokio::test]
    async fn upload_via_sta_to_carries_url_in_payload() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        transport.wifi_open.store(true, Ordering::Relaxed);
        device
            .upload_file_via_sta_to(
                "1A2B3C4D.apm",
                Some("https://dyj2.dev.reai.com/api/upload/DYJ001"),
            )
            .await
            .unwrap();
        let sent = transport
            .lines
            .lock()
            .await
            .iter()
            .find(|line| line.starts_with("UPLOAD_FILE:"))
            .cloned()
            .unwrap();
        assert_eq!(
            sent,
            "UPLOAD_FILE:{\"name\":\"1A2B3C4D.apm\",\"url\":\"https://dyj2.dev.reai.com/api/upload/DYJ001\"}"
        );
    }

    #[tokio::test]
    async fn upload_url_config_routes_to_wifi_module() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        transport.wifi_open.store(true, Ordering::Relaxed);
        device
            .set_wifi_upload_url("https://dyj2.dev.reai.com/api/upload/DYJ")
            .await
            .unwrap();
        let sent = transport
            .lines
            .lock()
            .await
            .iter()
            .find(|line| line.starts_with("CONFIG_SET:"))
            .cloned()
            .unwrap();
        assert!(sent.starts_with("CONFIG_SET:{\"key\":\"upload_url\""));
        for key in ["upload_url", "api_url", "ota_path", "wifi_ota_info_path"] {
            assert!(config_key_routes_to_wifi_module(key));
        }
        assert!(!config_key_routes_to_wifi_module("btn_single"));
    }

    #[tokio::test]
    async fn stream_error_fails_pending_requests_with_disconnected() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        transport.go_silent();
        let handle = tokio::spawn(async move { device.device_info().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        transport.fail_stream();
        // 修复前：worker 出错后不发 Disconnected，请求会挂满 10s 命令超时；
        // 用 2s 上限锁死“立刻失败”这个行为。
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("pending request must end promptly after stream failure")
            .unwrap();
        assert!(matches!(result, Err(SdkError::Disconnected)));
    }

    #[tokio::test]
    async fn realtime_session_streams_decoded_pcm() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let session = device.start_realtime_recording().await.unwrap();
        let mut audio = session.subscribe();

        // 两块 20ms 实时块（各 160 字节 ADPCM / 320 样本）先后注入。
        let pcm = crate::audio::test_support::sine(440.0, 640);
        let encoded = crate::audio::test_support::AdpcmEncoder::new().encode_buffer(&pcm);
        transport.reply(&[&[0_u8], &encoded[..160]].concat());
        transport.reply(&[&[0_u8], &encoded[160..]].concat());

        let first = audio.recv().await.unwrap();
        assert_eq!(first.sample_rate, 16_000);
        assert_eq!(first.channels, 1);
        assert!(!first.discontinuous);
        assert_eq!(first.samples.len(), 320);
        // 解码状态跨块连续：两块拼接结果必须与离线整段解码完全一致。
        let second = audio.recv().await.unwrap();
        let mut whole = first.samples;
        whole.extend(second.samples);
        let expected = crate::audio::AdpcmDecoder::new().decode(&encoded);
        assert_eq!(whole, expected);

        session.stop().await.unwrap();
        let commands = transport.commands.lock().await;
        assert_eq!(
            commands
                .iter()
                .filter(|command| *command == "RECORD_REALTIME_ENABLE")
                .count(),
            2,
            "start 开一次、stop 关一次"
        );
        assert!(commands.contains(&"RECORD_START".to_owned()));
        assert!(commands.contains(&"RECORD_STOP".to_owned()));
    }

    /// 两段不同信号各自从全新编码器编码（对应固件每次 RECORD_START 复位编码器）。
    fn two_segments() -> (Vec<u8>, Vec<u8>) {
        use crate::audio::test_support::{AdpcmEncoder, sine};
        let first = AdpcmEncoder::new().encode_buffer(&sine(440.0, 320));
        let second = AdpcmEncoder::new().encode_buffer(&sine(1000.0, 320));
        (first, second)
    }

    /// 前提自检：若解码器沿用上一段状态，第二段解码结果确实与全新解码不同，
    /// 否则下面的回归测试证明不了任何事。
    fn assert_carry_over_would_differ(first: &[u8], second: &[u8]) {
        let mut carried = crate::audio::AdpcmDecoder::new();
        carried.decode(first);
        assert_ne!(
            carried.decode(second),
            crate::audio::AdpcmDecoder::new().decode(second)
        );
    }

    #[tokio::test]
    async fn second_recording_on_same_connection_decodes_from_fresh_state() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut audio = device.subscribe_audio();
        let (first, second) = two_segments();
        assert_carry_over_would_differ(&first, &second);

        device.start_recording().await.unwrap();
        transport.reply(&[&[0_u8], &first[..]].concat());
        assert_eq!(
            audio.recv().await.unwrap().samples,
            crate::audio::AdpcmDecoder::new().decode(&first)
        );
        device.stop_recording().await.unwrap();

        // 同一连接内第二段录音：首块必须与全新解码器的结果逐样本一致。
        device.start_recording().await.unwrap();
        transport.reply(&[&[0_u8], &second[..]].concat());
        assert_eq!(
            audio.recv().await.unwrap().samples,
            crate::audio::AdpcmDecoder::new().decode(&second)
        );
    }

    #[tokio::test]
    async fn device_triggered_recording_resets_decoder_on_status_push() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut audio = device.subscribe_audio();
        let (first, second) = two_segments();
        assert_carry_over_would_differ(&first, &second);

        // 按键/开关在设备侧开停录音：只有带 reason 的 RECORD_STATUS 推送，没有 SDK 命令。
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"button\"}\r\n");
        transport.reply(&[&[0_u8], &first[..]].concat());
        audio.recv().await.unwrap();
        transport.reply(b"RECORD_STATUS:{\"isRecording\":false,\"realtimeEnabled\":true,\"reason\":\"button\"}\r\n");
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"switch\"}\r\n");
        transport.reply(&[&[0_u8], &second[..]].concat());
        assert_eq!(
            audio.recv().await.unwrap().samples,
            crate::audio::AdpcmDecoder::new().decode(&second)
        );
    }

    #[tokio::test]
    async fn repeated_recording_signals_mid_segment_do_not_reset_decoder() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut audio = device.subscribe_audio();
        let pcm = crate::audio::test_support::sine(440.0, 960);
        let encoded = crate::audio::test_support::AdpcmEncoder::new().encode_buffer(&pcm);

        device.start_recording().await.unwrap();
        transport.reply(&[&[0_u8], &encoded[..160]].concat());
        let mut whole = audio.recv().await.unwrap().samples;
        // 录音进行中：查询应答（无 reason）与迟到的开始推送都说「在录」，都不是新段的开始。
        let status = device.record_status().await.unwrap();
        assert!(status.is_recording);
        transport.reply(&[&[0_u8], &encoded[160..320]].concat());
        whole.extend(audio.recv().await.unwrap().samples);
        transport.reply(
            b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"app\"}\r\n",
        );
        transport.reply(&[&[0_u8], &encoded[320..]].concat());
        whole.extend(audio.recv().await.unwrap().samples);

        assert_eq!(whole, crate::audio::AdpcmDecoder::new().decode(&encoded));
    }

    #[tokio::test]
    async fn record_status_skips_push_queued_while_idle() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 空闲时设备推送一条带 reason 的状态（按键开始录音），没有请求在等它。
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"button\"}\r\n");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 修复前：这条推送被当成查询应答取走，真实应答留在队列里给下一次，之后每次错位一拍。
        for _ in 0..2 {
            let status = device.record_status().await.unwrap();
            assert_eq!(status.reason, None);
            assert!(!status.is_recording);
        }
    }

    #[tokio::test]
    async fn record_status_skips_push_arriving_while_waiting() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        transport.go_silent();
        let handle = tokio::spawn(async move { device.record_status().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // 等待应答期间先到一条推送，再到真实应答。
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":true,\"reason\":\"switch\"}\r\n");
        transport.reply(b"RECORD_STATUS:{\"isRecording\":true,\"realtimeEnabled\":false}\r\n");
        let status = handle.await.unwrap().unwrap();
        assert_eq!(status.reason, None);
        assert!(!status.realtime_enabled);
    }

    #[tokio::test]
    async fn stale_frame_queued_before_request_is_not_taken_as_reply() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 模拟上一次请求超时后才到的迟到应答。
        transport.reply(b"BASE_INFO:{\"version\":\"stale\",\"versionCode\":1,\"deviceId\":\"DYJ1\",\"deviceName\":\"DYJ\",\"timestamp\":1}\r\n");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(device.device_info().await.unwrap().version, "0.3.6");
    }

    #[tokio::test]
    async fn take_file_refuses_while_realtime_session_is_active() {
        let device = DyjDevice::from_transport(Arc::new(MockTransport::new()))
            .await
            .unwrap();
        let session = device.start_realtime_recording().await.unwrap();
        let mut writer = VecWriter(Vec::new());
        let error = device.take_file_to("a.apm", &mut writer).await.unwrap_err();
        assert!(matches!(error, SdkError::RealtimeRecordingActive));
        assert!(writer.0.is_empty());
        session.stop().await.unwrap();
        let mut writer = VecWriter(Vec::new());
        device.take_file_to("a.apm", &mut writer).await.unwrap();
        assert_eq!(writer.0, b"abc");
    }

    #[tokio::test]
    async fn audio_is_suppressed_while_a_file_transfer_is_active() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        let mut audio = device.subscribe_audio();
        device
            .take_file_to("a.apm", &mut VecWriter(Vec::new()))
            .await
            .unwrap();
        // 文件传输中的 0x00 二进制属于文件数据，不得混进音频广播。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(audio.try_recv().is_err());
    }

    #[tokio::test]
    async fn stray_binary_before_transfer_no_longer_fails() {
        let transport = Arc::new(MockTransport::new());
        let device = DyjDevice::from_transport(transport.clone()).await.unwrap();
        // 模拟设备侧按键触发录音、实时开关持久化开启时的后台音频流。
        transport.reply(&[0, 1, 2, 3, 4]);
        let mut writer = VecWriter(Vec::new());
        let taken = device.take_file_to("a.apm", &mut writer).await.unwrap();
        assert_eq!(taken.bytes_written, 3);
        assert_eq!(writer.0, b"abc");
    }
}
