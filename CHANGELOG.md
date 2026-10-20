# reai-card-sdk 更新日志

版本号独立于主控固件（nRF `v0.3.x` / WiFi `v0.0.x`）；SDK 与固件的协议对应关系见
[`docs/protocol-support.md`](docs/protocol-support.md)。

## 0.0.3（未发布）

按键 / 侧边开关事件全部类型化，并修复一个会丢帧的解码器问题。

**定位说明**：按键 / 开关在 SDK 里**只做事件传出**——固件在输入触发时先上报事件（带设备已执行的动作）再自己执行动作，业务层订阅事件做响应即可，不重放动作。随附的绑定 API 只是六个 `CONFIG_*` 键的类型化透传，**可选**：用不用、默认绑什么由业务层决定，SDK 不预设任何绑定（`ControlBindings::recorder()` 仅为示例预设）。

**⚠️ 不兼容变更**（0.0.x 阶段、driver-v2 尚未接入，一次性收口）：

- `DeviceEvent::Button(Value)` → `Button(ButtonEvent { gesture: ButtonGesture, action: ControlAction })`；`DeviceEvent::ToggleSwitch(Value)` → `ToggleSwitch(ToggleSwitchEvent { position: SwitchPosition, action })`。不认识的手势/位置不再丢弃，改为 `Unhandled { command, payload }`。
- `RecordStatus.reason: Option<String>` → `Option<RecordReason>`（`App/Button/Switch/Auto/Unknown/Other(String)`），新增 `error: Option<u32>`（录音出错推送携带）。
- `set_led(&str)` → `set_led(LedState)`（`On/Off/Toggle`，实现 `FromStr`）。
- `record_mark()` 返回 `RecordMark { filename, offset_secs }` 而非 `Value`。
- `check_wifi_ota()` 返回 `WifiOtaCheck { update_available }` 而非 `Value`；`OtaProgress.status: String` → `OtaStatus`（`Idle/Checking/UpdateAvailable/Downloading/Success/Failed/Other(String)`，`is_terminal()`）。
- `sta_upload_path()` / `sta_api_base_url()` / `sta_websocket_base_url()` 返回 `Option<String>`（`None` = 未配置，ESP32 用自身默认）。

新增：

- **按键 / 开关绑定 API**（`controls` 模块）：`control_action` / `set_control_action` / `clear_control_action` / `control_bindings` / `apply_control_bindings`，覆盖固件六个输入（`btn_single/btn_double/btn_long_press/btn_long_release/switch_on/switch_off`）与五个动作（`none/record_start/record_stop/record_toggle/record_mark`）。未配置读为 `ControlAction::None`，解绑幂等（固件 `not found` 视为成功），`ControlBindings::recorder()` 提供录音笔式预设。`ButtonGesture`/`SwitchPosition` 可直接传入（`Into<ControlInput>`）。
- **电源事件** `DeviceEvent::Power(PowerEvent)`：`ShutdownImminent { seconds }`（`SHUTDOWN` 响应后紧跟）、`DeviceWakeup`、`StartupComplete`。固件现状是**协议格式偏离**（裸 JSON，见下方修复项）；SDK 同时接受未来固件按协议格式封装的 `POWER:{json}\r\n` 文本帧，两种来源映射为同一事件。固件侧正确修法（`power_mgmt_event_handler` 改走 `app_protocol_send_event("POWER", …)`）已记入 `docs/protocol-support.md`，固件冻结仅记录。
- `config_value(key) -> Option<String>`：键不存在返回 `None` 而非错误；对 ESP32 路由键（`wifi_` 前缀 / `_wifi` 后缀，规则暴露为 `config_key_routes_to_wifi_module`）把固件回的空字符串也归一为 `None`（ESP32 对缺键回 `value:""`，nRF 回 `code 3`，两边不一致）。
- **配置快速失败**：`CONFIG_*` 对 ESP32 路由键先做模块上电预检（否则 UART 无人应答白等 10s）；键 > 31 字节 / 值 > 63 字节 / 空键在本地以 `InvalidCommand` 拒绝（超长会被 nRF 误报成 `missing key/value`），常量 `CONFIG_KEY_MAX_BYTES` / `CONFIG_VALUE_MAX_BYTES`。
- `set_wifi_ota_info_path` / `wifi_ota_info_path`：ESP32 四个配置键里最后一个未封装的（OTA 版本信息相对路径，默认 `/ota/info?mod=wifi`）。
- **WiFi OTA 编排** `update_wifi_firmware(wait)`：先订阅事件再发 `OTA_WIFI_UPDATE`，跟 `OTA_WIFI_PROCESS`（每 5% 一条）到 `Success`/`Failed` 返回最后一条进度，超时报 `Timeout{OTA_WIFI_PROCESS}`。需同一上电周期内先 `check_wifi_ota` 为 true；`Success` 后 ESP32 自重启、热点重启。真机验证与云上传同列延后（依赖 STA + `wifi_api_base_url` 服务）。
- `dyj_cli` 新增 `ota update [secs]`。
- `dyj_cli` 新增 `controls list|set|clear|preset recorder|reset`，事件打印改为可读文本；`dyj_web` 新增「08 按键 / 开关」分组（读绑定表 / 绑定 / 解绑 / 预设）与 `/api/controls*` 接口。

修复：

- **解码器识别无封装裸 JSON**：固件电源管理直接 `ble_uart_send` 写出 `{"type":"shutdown_imminent",...}`，既无命令名也无 `\r\n`；原按行解析会把它和下一帧粘成一行、两帧全丢（关机前 5 秒内若有命令响应就会超时）。现在缓冲区以 `{` 开头即按 JSON 边界切出裸对象（无需换行），余下字节照常按行解析；坏 JSON 丢到下一换行重新同步。新增解码器与设备级回归测试。

## 0.0.2（2026-10-20）

开源首发：源码迁至公开仓库 <https://github.com/ReAI-com/reai-card-sdk>（MIT），tag `v0.0.2`。AP 快传已真机验证通过（上电→连热点→列文件→下载→转 WAV）。

安全调整：**不再内置设备热点密码**（原 `AP_DEFAULT_PASSWORD` 常量移除）——HTTP 文件接口无鉴权，密码即访问凭证，不随开源 SDK 发布；应用自行从配置/设备文档获取。`dyj_web` 通过环境变量 `REAI_CARD_AP_PASSWORD` 注入显示。

真机联调修复（针对 v0.3.x 固件实测发现的问题）：

- **通知流出错立即终结挂起请求**：此前 worker 出错只发 Fault 就退出，`take_file` 等会挂满 30s 才失败；现在立刻以 `Disconnected` 返回，新增回归测试。
- **ESP32 透传命令上电预检**：`WIFI_SCAN/CONNECT/DISCONNECT`、`AP_*`、`UPLOAD_*`、`AUTH_*`、`OTA_WIFI_*` 先查 `STATE_INFO.wifiOpen`，模块断电直接返回 `WifiModulePoweredDown`（此前白等 10s 超时）。
- **AP 快传改为事件驱动 + 固定端点**：v0.3.x 固件上电自动启动 AP（固定 `DYJ-HD-AP` / `192.168.4.1:80`）且 nRF 不透传 `AP_*`；`access_point_client()` 不再查询 `AP_STATUS`，等待 `WIFI_AP_START` 事件（新增 `wait_for_ap_ready` / `access_point_ready`）后直接使用 `AP_DEFAULT_IP/PORT` 常量。`start_access_point` 等保留给带 `AP_` 透传的未来固件。HTTP 客户端建连超时 5s（电脑未连热点时快速失败而非挂到系统超时）；连接建立时模块已在供电、本次收不到就绪事件的重连场景，3s 宽限后按“AP 随上电自启”推断就绪。
- **AP 下载/删除 403 修复**：固件的目录遍历防护拒绝带前导 `/` 的 `path`（实测 `path=%2F059F19DF.APM` → 403 Invalid path）；`AccessPointClient::download_file_to/delete_file` 现在自动剥掉前导斜杠。
- **SD 卡归属预检**：`WIFI_OPEN` 后 SD 卡挂载权移交 ESP32，nRF 侧 `STORAGE`/`FILE_LIST`/`FILE_DELETE`/`FILE_CLEAR`/取走/`RECORD_START`/`RECORD_MARK` 都会 `sd not mounted`；SDK 现在先查 `STATE_INFO.wifiOpen`，模块上电时直接返回 `SdOwnedByWifiModule`（提示先 `close_wifi`，文件请走 AP 快传）。
- **`open_wifi` 幂等**：BLE 断链重连后 ESP32 常仍在供电，`wifiOpen` 已 true 时跳过 `WIFI_OPEN` 直接成功，并把快传入口标记为就绪（AP 随上电自启，本次连接不会再收到 `WIFI_AP_START`）。
- **录音时间命名**：固件按同名 `.txt` 的时间戳给录音起名（`synced_<unix>.apm` / `nosync_<unix>.apm`），BLE 取走的 start 帧 `name` 与 AP 下载的 `Content-Disposition` 都带它；新增 `RecordingStamp::parse` 解析，`AccessPointDownloadedFile.suggested_name` 透出建议名。dyj_web 落盘改为「本地时间_原编号.apm」（如 `20261019-160849_059F19DF.apm`），日志/结果带录音时间；时钟未同步的标 `nosync-<ts>_…`；无需额外下载 `.txt`。
- **落盘与转码进 SDK**：新增 `recording` 模块——`DyjDevice::take_file_to_dir` / `AccessPointClient::download_to_dir` 下载到指定目录（`.part` 原子改名、按录音时间命名、失败清理），返回 `SavedRecording`；`SavedRecording::to_wav()` / `apm_to_wav()` 把 IMA ADPCM 转成 16kHz WAV。dyj_web 改用这些接口并提供“落盘后转 WAV”开关；`chrono` 升为正式依赖。
- 明确结论：BLE 取走仅适合小文件（固件节奏 200B/25ms ≈ 8KB/s 上限），大文件一律走 AP 快传或云上传；云上传需 STA 模式 + 接收服务器，当前默认 AP 且无服务器，延后。

## 0.0.1（2026-10-19）

首个交付版本，供 ai-vibe-board / driver-v2 以 `recorder-card-v2` 硬件类型接入。

### 能力面

- **连接管理**：`DeviceManager` BLE 扫描/连接（NUS 服务校验），连接即自动 `SYNC_TIME`；命令串行化（协议无请求 ID）。
- **信息查询**（已真机验证）：`device_info` / `state` / `battery` / `storage` / `record_status` / `test_modules`。
- **录音控制**（已真机验证）：`start_recording` / `stop_recording` / `toggle_recording` / `record_mark`；录音写 SD 卡 `.apm`（16kHz/16bit/单声道，IMA ADPCM 4:1）。
- **实时音频**（已真机验证）：`start_realtime_recording()` 会话式收流 + `subscribe_audio()` 被动订阅；SDK 内置 IMA ADPCM→PCM 解码（16kHz 单声道 i16），丢块以 `Lagged` 上报；`wav_bytes()` 封 WAV；离线 `.apm` 可用 `AdpcmDecoder` 解码。
- **文件管理（BLE）**：`list_files`（已真机验证）、`delete_file`、`clear_files`、`take_file_to`（取走语义：设备删源文件；与实时录音互斥）。
- **硬件事件**：`subscribe_events()` 推送按键/拨杆/录音状态/WiFi 列表/OTA 进度等主动上报。
- **WiFi / 热点 / 云上传 / OTA / 配置**：API 已实现（`open_wifi`、`scan_wifi`、`connect_wifi`、`wifi_status`、`AccessPointClient` 快传、`upload_file_via_sta`、`wifi_ota_*`、`CONFIG_*`），协议层有 mock 测试覆盖，**尚未真机回归**。
- **测试工具**：`examples/dyj_cli`（全 API CLI，`--verbose/--trace`）与 `examples/dyj_web`（网页控制台：SSE 日志/事件推送、实时录音波形面板、演示信号源）。

### 已知限制

- 仅承诺 macOS/Linux；Windows 未验证（driver-v2 进 Windows 默认构建前必须先验证）。
- 二进制通道由文件下载与实时音频共用，无头部区分；SDK 以互斥门保证不混流，设备侧自触发录音期间取文件仍是固件层限制。
- `publish = false`：阶段一以 path / git + tag 依赖交付；发 crates.io 需补 LICENSE/CI。

### 依赖引入

0.0.1 托管于固件仓库（`dyj-hd-v2/sdk/rust`，tag `sdk-v0.0.1`）；0.0.2 起迁至本公开仓库，请使用：

```toml
reai-card-sdk = { git = "https://github.com/ReAI-com/reai-card-sdk.git", tag = "v0.0.2" }
```

验收命令：`make verify`（fmt/check/test/clippy 全绿）。
