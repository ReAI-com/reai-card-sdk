//! DYJ 网页测试控制台：浏览器里驱动 SDK 全部 API，日志与设备事件经 SSE 实时推送。
//!
//! ```text
//! cargo run --example dyj_web -- --port 8787
//! # 打开 http://127.0.0.1:8787
//! ```
//!
//! 仅绑定 127.0.0.1，属开发测试工具，不得暴露到局域网（无鉴权）。

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Query, State, ws},
    http::StatusCode,
    response::{
        Html, IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::Stream;
use tokio::sync::{Mutex, RwLock, broadcast};

use reai_card_sdk::{
    AP_DEFAULT_IP, AP_DEFAULT_PORT, AP_DEFAULT_SSID, AccessPointClient, ControlAction,
    ControlBindings, ControlInput, DeviceEvent, DeviceManager, DfuOptions, DfuPackage, DfuStage,
    DyjDevice, PcmChunk, RealtimeRecordingSession, SAMPLE_RATE_HZ, SavedRecording,
};

#[path = "common/mod.rs"]
mod common;

const INDEX_HTML: &str = include_str!("dyj_web.html");
const DEFAULT_PORT: u16 = 8787;
const DEFAULT_SCAN_SECS: u64 = 5;

struct AppState {
    manager: DeviceManager,
    device: RwLock<Option<DyjDevice>>,
    /// 串行化 connect/disconnect，避免两个浏览器标签页同时重连。
    link_lock: Mutex<()>,
    /// 设备主动上报：(命令名, payload)。日志走 `common::log_sender` 的独立广播。
    events: broadcast::Sender<(String, serde_json::Value)>,
    downloads: PathBuf,
    /// 进行中的实时录音会话（同一时刻至多一个，会话保活期间音频持续流入）。
    recording: Mutex<Option<RealtimeRecordingSession>>,
    /// 无真机时的演示音频源：与设备音频走同一条 WS 通路，便于验证前端波形。
    demo_audio: broadcast::Sender<PcmChunk>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut port = DEFAULT_PORT;
    let mut verbosity = 0;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--port" => {
                port = iter
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_PORT)
            }
            "--verbose" => verbosity = 1,
            "--trace" => verbosity = 2,
            other => {
                eprintln!("未知参数 {other}（支持 --port <n> --verbose --trace）");
                std::process::exit(2);
            }
        }
    }
    common::init_tracing(verbosity);

    let manager = DeviceManager::new()
        .await
        .expect("蓝牙适配器不可用（macOS 需在系统设置里授权蓝牙）");
    let downloads = std::env::current_dir()
        .unwrap_or_default()
        .join("downloads");
    tokio::fs::create_dir_all(&downloads)
        .await
        .expect("无法创建 downloads 目录");
    let (events, _) = broadcast::channel(256);
    let (demo_audio, _) = broadcast::channel(512);
    let app_downloads = downloads.clone();
    let state = Arc::new(AppState {
        manager,
        device: RwLock::new(None),
        link_lock: Mutex::new(()),
        events,
        downloads,
        recording: Mutex::new(None),
        demo_audio,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/stream", get(stream))
        .route("/api/scan", get(scan))
        .route("/api/connect", post(connect))
        .route("/api/disconnect", post(disconnect))
        .route("/api/command", post(command))
        .route("/api/files", get(files))
        .route("/api/take", post(take))
        .route("/api/wifi/scan", get(wifi_scan))
        .route("/api/wifi/status", get(wifi_status))
        .route("/api/uploads", get(uploads))
        .route("/api/ap/status", get(ap_status))
        .route("/api/ap/files", get(ap_files))
        .route("/api/ap/download", post(ap_download))
        .route("/api/record/realtime/start", post(realtime_start))
        .route("/api/record/realtime/stop", post(realtime_stop))
        .route("/api/audio", get(audio_ws))
        .route("/api/audio/demo", post(audio_demo))
        .route("/api/controls", get(controls_list))
        .route("/api/controls/set", post(controls_set))
        .route("/api/controls/clear", post(controls_clear))
        .route("/api/controls/preset", post(controls_preset))
        .route("/api/dfu", post(dfu_update))
        .route("/api/ap/ota/check", get(ap_ota_check))
        .route("/api/ap/ota/upload", post(ap_ota_upload))
        // 固件上传：ESP32 .bin 约 1.3MB，nrfutil .zip 约 170KB；默认 2MB 限制不够
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|error| panic!("无法绑定 {addr}: {error}"));
    tracing::info!(target: "dyj.device", "🚀 dyj_web 已启动 http://{addr}（仅本机可访问）");
    tracing::info!(target: "dyj.file", "📁 下载目录 {}（BLE 取走与 AP 快传都保存在这里）", app_downloads.display());
    axum::serve(listener, app).await.expect("HTTP 服务异常退出");
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// 统一 API 错误响应：JSON `{"error": "..."}`。
struct ApiError(reai_card_sdk::SdkError);

impl From<reai_card_sdk::SdkError> for ApiError {
    fn from(error: reai_card_sdk::SdkError) -> Self {
        Self(error)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self(reai_card_sdk::SdkError::Io(error))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(reai_card_sdk::SdkError::Json(error))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": self.0.to_string()})),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

async fn scan(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<reai_card_sdk::DiscoveredDevice>>> {
    let secs: u64 = params
        .get("secs")
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SCAN_SECS);
    let devices = state.manager.scan(Duration::from_secs(secs)).await?;
    Ok(Json(devices))
}

async fn connect(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = body
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 id 字段"))?;

    let _link = state.link_lock.lock().await;
    // btleplug 连接需要适配器已见过该外设：先短扫描刷新，再按 id 匹配。
    let devices = state.manager.scan(Duration::from_secs(2)).await?;
    let found = devices
        .iter()
        .find(|device| device.id == id)
        .ok_or_else(|| ApiError(reai_card_sdk::SdkError::DeviceNotFound(id.to_owned())))?;
    let device = state.manager.connect(found).await?;

    let mut event_rx = device.subscribe_events();
    let event_tx = state.events.clone();
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let message = describe_event(event);
                    let _ = event_tx.send(message);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    *state.device.write().await = Some(device);
    tracing::info!(target: "dyj.device", "✅ 网页端已连接 id={id}");
    Ok(Json(serde_json::json!({"connected": true, "id": id})))
}

async fn disconnect(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let _link = state.link_lock.lock().await;
    // 录音会话先于设备断开收尾，避免设备没了之后 SDK 后台还在重试停录。
    if let Some(session) = state.recording.lock().await.take() {
        let _ = session.stop().await;
    }
    if let Some(device) = state.device.write().await.take() {
        device.disconnect().await?;
    }
    Ok(Json(serde_json::json!({"connected": false})))
}

async fn command(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let name = body
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 command 字段"))?;
    let payload = body
        .get("payload")
        .filter(|value| !value.is_null())
        .cloned();
    let response = device.raw_command(name, payload).await?;
    Ok(Json(response))
}

async fn files(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<Vec<reai_card_sdk::FileEntry>>> {
    let device = current_device(&state).await?;
    let folder = params.get("folder").map(String::as_str);
    Ok(Json(device.list_files(folder).await?))
}

async fn take(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let name = body
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 name 字段"))?;
    let saved = device.take_file_to_dir(name, &state.downloads).await?;
    saved_json(saved, want_wav(&body)).await
}

async fn wifi_scan(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<reai_card_sdk::WifiNetwork>>> {
    let device = current_device(&state).await?;
    Ok(Json(device.scan_wifi().await?))
}

/// 走 SDK 封装：模块断电时由 `STATE_INFO.wifiOpen` 兜底，不会打 10s 超时。
async fn wifi_status(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<reai_card_sdk::WifiStatus>> {
    let device = current_device(&state).await?;
    Ok(Json(device.wifi_status().await?))
}

async fn uploads(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<reai_card_sdk::FileEntry>>> {
    let device = current_device(&state).await?;
    Ok(Json(device.list_uploads().await?))
}

/// 快传状态一键闭环：模块是否上电 → 电脑是否已连热点（实际探测 ESP32 的
/// /api/device/info，4s 超时）→ 已连则顺带拉 AP 文件列表供下载。
/// 不再查询 AP_STATUS（v0.3.x 的 nRF 不透传该命令，发了只会白等超时）。
async fn ap_status(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let mut out = serde_json::Map::new();
    out.insert(
        "endpoint".into(),
        format!("http://{}:{}/", AP_DEFAULT_IP, AP_DEFAULT_PORT).into(),
    );
    out.insert("ssid".into(), AP_DEFAULT_SSID.into());
    // 热点密码不随 SDK 公开发布：测试时通过环境变量注入，缺省提示去看设备说明
    let ap_password = std::env::var("REAI_CARD_AP_PASSWORD").ok();
    out.insert("password".into(), ap_password.clone().into());

    if !device.state().await?.wifi_open {
        out.insert("powered".into(), false.into());
        out.insert("reachable".into(), false.into());
        out.insert(
            "hint".into(),
            "WiFi 模块未上电：先点 WiFi「开」，AP 会随上电自动启动（约 2s 后就绪）".into(),
        );
        return Ok(Json(out.into()));
    }
    out.insert("powered".into(), true.into());

    // access_point_client 内部会等 WIFI_AP_START 宽限期；模块已上电但事件没来时按自启推断
    let client = device.access_point_client().await?;
    let probe = tokio::time::timeout(Duration::from_secs(4), client.device_info()).await;
    match probe {
        Ok(Ok(info)) => {
            out.insert("reachable".into(), true.into());
            out.insert(
                "deviceInfo".into(),
                serde_json::to_value(info).unwrap_or_default(),
            );
            match client.list_files(1, 50).await {
                Ok(page) => {
                    out.insert(
                        "files".into(),
                        serde_json::to_value(page).unwrap_or_default(),
                    );
                    out.insert(
                        "hint".into(),
                        "已连接热点，快传可用：下方为 AP 文件列表，点「下载」即可".into(),
                    );
                }
                Err(error) => {
                    out.insert(
                        "hint".into(),
                        format!("热点可达，但拉取文件列表失败：{error}").into(),
                    );
                }
            }
        }
        _ => {
            out.insert("reachable".into(), false.into());
            out.insert(
                "hint".into(),
                format!(
                    "热点已就绪，但电脑尚未连接：请加入 WiFi「{AP_DEFAULT_SSID}」（密码{}）后再点一次",
                    ap_password
                        .as_deref()
                        .map(|p| format!(" {p}"))
                        .unwrap_or_else(|| "见设备说明".to_owned())
                )
                .into(),
            );
        }
    }
    Ok(Json(out.into()))
}

/// AP 侧文件列表（走 HTTP，电脑需已加入设备热点）。
async fn ap_files(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult<Json<reai_card_sdk::AccessPointFilePage>> {
    let device = current_device(&state).await?;
    let client = device.access_point_client().await?;
    let page: u32 = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    let page_size: u32 = params
        .get("pageSize")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    Ok(Json(client.list_files(page, page_size).await?))
}

async fn ap_download(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let path = body
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 path 字段"))?;
    let client: AccessPointClient = device.access_point_client().await?;
    let saved = client.download_to_dir(path, &state.downloads).await?;
    saved_json(saved, want_wav(&body)).await
}

/// `wav` 字段缺省为 true：落盘后顺手转成可播放的 WAV。
fn want_wav(body: &serde_json::Value) -> bool {
    body.get("wav")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// BLE 取走与 AP 下载共用的结果：落盘路径、录音时间、可选的 WAV 转换结果。
async fn saved_json(saved: SavedRecording, wav: bool) -> ApiResult<Json<serde_json::Value>> {
    let wav = if wav {
        Some(saved.to_wav().await?)
    } else {
        None
    };
    Ok(Json(serde_json::json!({
        "savedTo": saved.path.display().to_string(),
        "bytesWritten": saved.bytes_written,
        "originalName": saved.original_name,
        "deviceName": saved.device_name,
        "recordedAt": saved.recorded_at.map(|at| at.to_rfc3339()),
        "synced": saved.stamp.map(|stamp| stamp.synced),
        "wavPath": wav.as_ref().map(|file| file.path.display().to_string()),
        "durationSecs": wav.as_ref().map(|file| file.duration_secs()),
    })))
}

/// 打开实时录音：SDK 内部依次发 RECORD_REALTIME_ENABLE(true) + RECORD_START，
/// 音频随后经 /api/audio 的 WebSocket 以 16bit 小端 PCM 推给浏览器。
async fn realtime_start(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let mut recording = state.recording.lock().await;
    if recording.is_some() {
        return Ok(Json(serde_json::json!({
            "started": false,
            "reason": "实时录音已在进行中",
        })));
    }
    *recording = Some(device.start_realtime_recording().await?);
    Ok(Json(serde_json::json!({"started": true})))
}

async fn realtime_stop(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let session = state.recording.lock().await.take();
    match session {
        Some(session) => {
            session.stop().await?;
            Ok(Json(serde_json::json!({"stopped": true})))
        }
        None => Ok(Json(serde_json::json!({
            "stopped": false,
            "reason": "当前没有实时录音",
        }))),
    }
}

/// 实时音频 WebSocket：首条文本消息是采样格式声明，之后每帧二进制都是 i16LE PCM。
/// 服务端积压导致的丢块以 `{"type":"lost"}` 文本消息告知，前端据此标记断流。
async fn audio_ws(ws: ws::WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| audio_socket(socket, state))
}

async fn audio_socket(mut socket: ws::WebSocket, state: Arc<AppState>) {
    let config = serde_json::json!({
        "type": "config",
        "sampleRate": SAMPLE_RATE_HZ,
        "channels": 1,
        "bits": 16,
    });
    if socket
        .send(ws::Message::Text(config.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    // 设备未连接时设备源保持 pending，演示源仍可用。
    let mut device_rx = current_device(&state)
        .await
        .map(|device| device.subscribe_audio())
        .ok();
    let mut demo_rx = state.demo_audio.subscribe();
    loop {
        let message = tokio::select! {
            recv = next_chunk(device_rx.as_mut()) => to_ws_message(recv),
            recv = demo_rx.recv() => to_ws_message(recv),
        };
        match message {
            Some(message) => {
                if socket.send(message).await.is_err() {
                    return;
                }
            }
            None => return,
        }
    }
}

async fn next_chunk(
    rx: Option<&mut broadcast::Receiver<PcmChunk>>,
) -> Result<PcmChunk, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn to_ws_message(recv: Result<PcmChunk, broadcast::error::RecvError>) -> Option<ws::Message> {
    match recv {
        Ok(chunk) => {
            let mut bytes = Vec::with_capacity(chunk.samples.len() * 2);
            for sample in &chunk.samples {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            Some(ws::Message::Binary(bytes.into()))
        }
        Err(broadcast::error::RecvError::Lagged(count)) => Some(ws::Message::Text(
            serde_json::json!({"type": "lost", "chunks": count})
                .to_string()
                .into(),
        )),
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

/// 演示音频源：无需真机即可验证整条 WS + 波形链路。
/// 生成变频哨声 + 淡入淡出包络，按 20ms 一块推进，节奏与固件实时流一致。
async fn audio_demo(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let secs: f64 = params
        .get("secs")
        .and_then(|value| value.parse().ok())
        .unwrap_or(6.0_f64)
        .clamp(1.0, 30.0);
    let tx = state.demo_audio.clone();
    tokio::spawn(async move {
        const CHUNK: usize = 320; // 20ms @16kHz
        let total = (secs * SAMPLE_RATE_HZ as f64 / CHUNK as f64) as usize;
        for n in 0..total {
            let base = n as f64 * CHUNK as f64 / SAMPLE_RATE_HZ as f64;
            let mut samples = Vec::with_capacity(CHUNK);
            for i in 0..CHUNK {
                let t = base + i as f64 / SAMPLE_RATE_HZ as f64;
                let freq = 320.0 + 280.0 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
                let envelope = (t / 0.4).min(1.0).min(((secs - t) / 0.4).min(1.0)).max(0.0);
                let value = (2.0 * std::f64::consts::PI * freq * t).sin() * 11_000.0 * envelope;
                samples.push(value as i16);
            }
            if tx
                .send(PcmChunk {
                    samples,
                    sample_rate: SAMPLE_RATE_HZ,
                    channels: 1,
                    discontinuous: false,
                })
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    Json(serde_json::json!({"demo": true, "seconds": secs}))
}

/// SSE：日志与设备事件合并成一条流，断线由浏览器 EventSource 自动重连。
async fn stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let logs = common::log_sender().subscribe();
    let events = state.events.subscribe();

    let log_stream = unfold_broadcast(logs, |line| {
        SseEvent::default()
            .event("log")
            .data(serde_json::json!({"level": line.level, "line": line.line}).to_string())
    });
    let event_stream = unfold_broadcast(events, |(command, payload)| {
        SseEvent::default()
            .event("device")
            .data(serde_json::json!({"command": command, "payload": payload}).to_string())
    });

    Sse::new(futures::stream::select(log_stream, event_stream)).keep_alive(KeepAlive::default())
}

/// 把 broadcast 接收端转成 SSE 流；消费过慢只报告落后条数，不断流。
fn unfold_broadcast<T, F>(
    rx: broadcast::Receiver<T>,
    to_event: F,
) -> impl Stream<Item = Result<SseEvent, std::convert::Infallible>>
where
    T: Clone + Send + 'static,
    F: Fn(T) -> SseEvent + Send + 'static + Clone,
{
    futures::stream::unfold((rx, to_event), |(mut rx, to_event)| async move {
        // Lagged 后返回一条提示；unfold 下一次 poll 会再次进入本闭包，等效于循环重试。
        match rx.recv().await {
            Ok(message) => Some((Ok(to_event(message)), (rx, to_event))),
            Err(broadcast::error::RecvError::Lagged(count)) => {
                let warning = SseEvent::default().event("log").data(
                    serde_json::json!({
                        "level": "WARN",
                        "line": format!("⚠️ [系统] 推送过慢，落后 {count} 条"),
                    })
                    .to_string(),
                );
                Some((Ok(warning), (rx, to_event)))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
}

async fn current_device(state: &AppState) -> ApiResult<DyjDevice> {
    state
        .device
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError(reai_card_sdk::SdkError::Disconnected))
}

fn invalid_body(message: &str) -> ApiError {
    ApiError(reai_card_sdk::SdkError::InvalidFrame(message.to_owned()))
}

// ========== 固件升级 ==========

/// 从 multipart 里取名为 `field` 的文件（文件名, 内容），其余文本字段原样收集。
async fn read_upload(
    mut multipart: Multipart,
    field: &str,
) -> ApiResult<(String, Vec<u8>, HashMap<String, String>)> {
    let mut file = None;
    let mut fields = HashMap::new();
    while let Some(part) = multipart
        .next_field()
        .await
        .map_err(|error| invalid_body(&format!("multipart 解析失败: {error}")))?
    {
        let name = part.name().unwrap_or_default().to_owned();
        if name == field {
            let file_name = part.file_name().unwrap_or("upload.bin").to_owned();
            let bytes = part
                .bytes()
                .await
                .map_err(|error| invalid_body(&format!("读取 {field} 失败: {error}")))?;
            file = Some((file_name, bytes.to_vec()));
        } else {
            let text = part
                .text()
                .await
                .map_err(|error| invalid_body(&format!("读取字段 {name} 失败: {error}")))?;
            fields.insert(name, text);
        }
    }
    let (file_name, bytes) = file.ok_or_else(|| invalid_body(&format!("缺少文件字段 {field}")))?;
    Ok((file_name, bytes, fields))
}

/// 主控 DFU 全流程：multipart `package`（nrfutil .zip）+ 可选 `packet_size`。
/// 同步执行（约 1–2 分钟），进度每 5% 走一条日志；完成后设备以新固件重启，旧连接作废。
async fn dfu_update(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let (file_name, bytes, fields) = read_upload(multipart, "package").await?;
    let package = DfuPackage::from_zip_bytes(&bytes)?;
    let mut options = DfuOptions::default();
    if let Some(size) = fields
        .get("packet_size")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|size| *size > 0)
    {
        options.packet_size = size;
    }
    let device = current_device(&state).await?;
    tracing::info!(target: "dyj.dfu", "⬆️ 网页发起主控升级 {file_name}：init {} 字节，固件 {} 字节，packet {}", package.init_packet.len(), package.firmware.len(), options.packet_size);

    // 录音会话在设备复位前收尾，避免后台还在尝试停录
    if let Some(session) = state.recording.lock().await.take() {
        let _ = session.stop().await;
    }
    let mut last: Option<(DfuStage, u8)> = None;
    let report = state
        .manager
        .update_main_firmware(&device, &package, &options, move |progress| {
            let bucket = progress.percent() / 5;
            if last != Some((progress.stage, bucket)) {
                let label = match progress.stage {
                    DfuStage::InitPacket => "init packet",
                    DfuStage::Firmware => "固件",
                };
                tracing::info!(target: "dyj.dfu", "⬆️ {label} {:>3}%（{}/{} 字节）", progress.percent(), progress.sent, progress.total);
                last = Some((progress.stage, bucket));
            }
        })
        .await;
    // 无论成败，设备都已复位离开应用固件，旧连接作废
    state.device.write().await.take();
    let report = report?;
    Ok(Json(serde_json::json!({
        "fileName": file_name,
        "initPacketBytes": report.init_packet_bytes,
        "firmwareBytes": report.firmware_bytes,
        "objects": report.objects,
        "retries": report.retries,
        "resumedFrom": report.resumed_from,
        "elapsedSecs": report.elapsed.as_secs_f64(),
        "hint": "bootloader 正在校验并重启进新固件，请稍后重新扫描连接；BASE_INFO 的 version 应为新版本",
    })))
}

/// 热点模式读 WiFi 模块当前固件版本。
async fn ap_ota_check(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let client = device.access_point_client().await?;
    Ok(Json(serde_json::to_value(client.ota_status().await?)?))
}

/// 热点模式本地升级 WiFi 模块：multipart `firmware`（.bin）。需电脑已连设备热点。
async fn ap_ota_upload(
    State(state): State<Arc<AppState>>,
    multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let (file_name, bytes, _) = read_upload(multipart, "firmware").await?;
    let device = current_device(&state).await?;
    let client = device.access_point_client().await?;
    let before = client.ota_status().await?;
    tracing::info!(target: "dyj.ap", "⬆️ 网页发起 WiFi 模块本地升级 {file_name}（{} 字节），当前 {} (code {})", bytes.len(), before.current_version, before.current_version_code);
    let result = client.upload_firmware(bytes, &file_name).await?;
    Ok(Json(serde_json::json!({
        "fileName": file_name,
        "before": before,
        "result": result,
        "hint": "ESP32 将在 3 秒后重启，热点随之重启；稍后再点「快传状态」确认新版本（WIFI_AP_START 事件会再来一次）",
    })))
}

// ========== 按键 / 侧边开关绑定 ==========

fn parse_control_input(body: &serde_json::Value) -> ApiResult<ControlInput> {
    let text = body
        .get("input")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 input 字段"))?;
    ControlInput::from_config_key(text).ok_or_else(|| {
        invalid_body(&format!(
            "未知输入 {text}，应为 {}",
            ControlInput::ALL
                .iter()
                .map(|input| input.config_key())
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    })
}

/// 读六个输入的绑定表（六次 CONFIG_GET，未配置的项显示为 none）。
async fn controls_list(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    Ok(Json(serde_json::to_value(
        device.control_bindings().await?,
    )?))
}

/// `{input, action}`：把某个输入绑定到动作，固件侧立刻生效并持久化。
async fn controls_set(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let input = parse_control_input(&body)?;
    let action = body
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(ControlAction::parse)
        .ok_or_else(|| invalid_body("缺少 action 字段"))?;
    device.set_control_action(input, action.clone()).await?;
    Ok(Json(serde_json::json!({
        "input": input.config_key(),
        "action": action.wire_name(),
    })))
}

/// `{input}`：解除绑定（删配置键）。
async fn controls_clear(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let input = parse_control_input(&body)?;
    device.clear_control_action(input).await?;
    Ok(Json(
        serde_json::json!({ "input": input.config_key(), "action": "none" }),
    ))
}

/// `{name}`：整表写入预设。`recorder` = 开关开/关录音、单击打点、双击切换；`factory` = 全部解绑。
async fn controls_preset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let device = current_device(&state).await?;
    let name = body
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_body("缺少 name 字段"))?;
    match name {
        "recorder" => {
            device
                .apply_control_bindings(&ControlBindings::recorder())
                .await?
        }
        "factory" => {
            for input in ControlInput::ALL {
                device.clear_control_action(input).await?;
            }
        }
        other => return Err(invalid_body(&format!("未知预设 {other}"))),
    }
    Ok(Json(serde_json::to_value(
        device.control_bindings().await?,
    )?))
}

/// `DeviceEvent` → (命令名, payload)，前端据此渲染事件卡片。
fn describe_event(event: DeviceEvent) -> (String, serde_json::Value) {
    fn value(item: impl serde::Serialize) -> serde_json::Value {
        serde_json::to_value(item).unwrap_or_default()
    }
    match event {
        DeviceEvent::Button(event) => ("BTN_CLICK".into(), value(event)),
        DeviceEvent::ToggleSwitch(event) => ("TOGGLE_SWITCH".into(), value(event)),
        DeviceEvent::RecordStatus(status) => ("RECORD_STATUS".into(), value(status)),
        DeviceEvent::WifiNetwork(network) => ("WIFI_LIST".into(), value(network)),
        DeviceEvent::WifiOff(response) => ("WIFI_OFF".into(), value(response)),
        DeviceEvent::UploadComplete(response) => ("UPLOAD_COMPLETE".into(), value(response)),
        DeviceEvent::WifiOtaProgress(progress) => ("OTA_WIFI_PROCESS".into(), value(progress)),
        DeviceEvent::AccessPointReady(response) => ("WIFI_AP_START".into(), value(response)),
        DeviceEvent::Power(power) => ("POWER".into(), value(power)),
        DeviceEvent::Unhandled { command, payload } if command.is_empty() => {
            ("RAW".into(), payload)
        }
        DeviceEvent::Unhandled { command, payload } => (command, payload),
    }
}
