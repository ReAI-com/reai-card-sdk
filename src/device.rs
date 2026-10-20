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
    protocol::{FrameDecoder, TextFrame, encode_command},
    recording,
    recording::SavedRecording,
    transport::SharedTransport,
    types::{
        AccessPointInfo, BatteryInfo, CommandResponse, DeviceEvent, DeviceInfo, DeviceState,
        FileEntry, ModuleCheck, OtaProgress, RecordStatus, StorageInfo, TakenFile, WifiNetwork,
        WifiStatus,
    },
};

const NORMAL_TIMEOUT: Duration = Duration::from_secs(10);
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
    /// 录音打点（写同名 `.mrk`，同样要求 SD 在 nRF 侧）。
    pub async fn record_mark(&self) -> Result<Value> {
        self.require_sd_on_nrf("RECORD_MARK").await?;
        self.request_value("RECORD_MARK", None, NORMAL_TIMEOUT)
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
        self.require_wifi_module("UPLOAD_FILE").await?;
        self.request_success(
            "UPLOAD_FILE",
            Some(json!({"name": name})),
            UPLOAD_FILE_TIMEOUT,
        )
        .await
    }
    /// Requests that the ESP32 upload a file over its STA connection to the configured server.
    pub async fn upload_file_via_sta(&self, name: &str) -> Result<CommandResponse> {
        self.upload_file(name).await
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
    pub async fn check_wifi_ota(&self) -> Result<Value> {
        self.require_wifi_module("OTA_WIFI_CHECK").await?;
        self.request_value("OTA_WIFI_CHECK", None, NORMAL_TIMEOUT)
            .await
    }
    pub async fn start_wifi_ota(&self) -> Result<CommandResponse> {
        self.require_wifi_module("OTA_WIFI_UPDATE").await?;
        self.request_success("OTA_WIFI_UPDATE", None, NORMAL_TIMEOUT)
            .await
    }

    pub async fn set_config(&self, key: &str, value: &str) -> Result<CommandResponse> {
        self.request_success(
            "CONFIG_SET",
            Some(json!({"key": key, "value": value})),
            NORMAL_TIMEOUT,
        )
        .await
    }

    /// Sets the ESP32's relative upload route, for example `/record`.
    pub async fn set_sta_upload_path(&self, path: &str) -> Result<CommandResponse> {
        if !path.starts_with('/') {
            return Err(SdkError::InvalidCommand(
                "STA upload path must begin with `/`".into(),
            ));
        }
        self.set_config("wifi_file_upload_path", path).await
    }

    pub async fn sta_upload_path(&self) -> Result<String> {
        self.wifi_config_value("wifi_file_upload_path").await
    }

    pub async fn set_sta_api_base_url(&self, url: &str) -> Result<CommandResponse> {
        validate_url_scheme(url, &["http", "https"])?;
        self.set_config("wifi_api_base_url", url).await
    }

    pub async fn sta_api_base_url(&self) -> Result<String> {
        self.wifi_config_value("wifi_api_base_url").await
    }

    pub async fn set_sta_websocket_base_url(&self, url: &str) -> Result<CommandResponse> {
        validate_url_scheme(url, &["ws", "wss"])?;
        self.set_config("wifi_ws_base_url", url).await
    }

    pub async fn sta_websocket_base_url(&self) -> Result<String> {
        self.wifi_config_value("wifi_ws_base_url").await
    }
    pub async fn get_config(&self, key: &str) -> Result<Value> {
        self.request_value("CONFIG_GET", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await
    }
    pub async fn delete_config(&self, key: &str) -> Result<CommandResponse> {
        self.request_success("CONFIG_DEL", Some(json!({"key": key})), NORMAL_TIMEOUT)
            .await
    }
    pub async fn set_led(&self, state: &str) -> Result<CommandResponse> {
        self.request_success("LED", Some(json!({"state": state})), NORMAL_TIMEOUT)
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
        let _command = self.core.command_gate.lock().await;
        let mut incoming = self.core.incoming.lock().await;
        self.write_command(&command, payload).await?;
        loop {
            if let Inbound::Text(frame) = receive_next(&mut incoming, wait, &command).await?
                && frame.command == command
            {
                ensure_success(&frame.command, &frame.payload)?;
                tracing::info!(target: "dyj.proto", "📥 收到响应 cmd={} code=0 耗时={}ms", frame.command, started.elapsed().as_millis());
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
        let payload_text = payload
            .as_ref()
            .map(|payload| payload.to_string())
            .unwrap_or_default();
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

    async fn wifi_config_value(&self, key: &str) -> Result<String> {
        self.get_config(key)
            .await?
            .get("value")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                SdkError::InvalidFrame(format!("CONFIG_GET `{key}` omitted string value"))
            })
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
        // ADPCM 解码状态跨整段录音连续，必须放在 worker 里而不是每个订阅者各自解码。
        let mut audio_decoder = AdpcmDecoder::new();
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

fn event_from_frame(frame: &TextFrame) -> Option<DeviceEvent> {
    match frame.command.as_str() {
        "BTN_CLICK" => Some(DeviceEvent::Button(frame.payload.clone())),
        "TOGGLE_SWITCH" => Some(DeviceEvent::ToggleSwitch(frame.payload.clone())),
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
        _ => None,
    }
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
        announced_file_size: AtomicU64,
        wifi_open: AtomicBool,
        silent: AtomicBool,
    }

    impl MockTransport {
        fn new() -> Self {
            let (sent, received) = tokio::sync::mpsc::unbounded_channel();
            Self {
                sent,
                received: Mutex::new(Some(received)),
                write_buffer: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
                announced_file_size: AtomicU64::new(3),
                wifi_open: AtomicBool::new(false),
                silent: AtomicBool::new(false),
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

        fn handle_command(&self, command: &str) {
            if self.silent.load(Ordering::Relaxed) {
                return;
            }
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
                    self.reply(b"WIFI_STATUS:{\"status\":\"connected\",\"enabled\":true,\"connected\":true,\"mode\":\"sta\",\"ssid\":\"home\",\"ip\":\"192.168.1.23\",\"rssi\":-52}\r\n");
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
                "RECORD_START" => self.reply(b"RECORD_START:{\"code\":0}\r\n"),
                "AP_START" => self.reply(b"AP_START:{\"code\":0,\"ip\":\"192.168.4.1\",\"webPort\":80}\r\n"),
                "AP_STATUS" => self.reply(b"AP_STATUS:{\"status\":\"active\",\"active\":true,\"ip\":\"192.168.4.1\",\"webServerRunning\":true,\"webPort\":80}\r\n"),
                "TEST_CHECK" => {
                    for index in 0..11 {
                        self.reply(format!("TEST_CHECK:{{\"key\":\"M{index}\",\"result\":true}}\r\n").as_bytes());
                    }
                }
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
                let command = text.split(':').next().unwrap().to_owned();
                self.commands.lock().await.push(command.clone());
                self.handle_command(&command);
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
