# reai-card-sdk

[![CI](https://github.com/ReAI-com/reai-card-sdk/actions/workflows/ci.yml/badge.svg)](https://github.com/ReAI-com/reai-card-sdk/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![version](https://img.shields.io/badge/version-0.0.2-informational)

ReAI 录音卡（DYJ-HD v2）的异步 Rust SDK：通过 BLE 控制设备、接收并解码**实时音频流**、经设备热点**快传录音文件**、把录音落盘并转成可播放的 WAV。

- 传输：BLE Nordic UART Service（NUS），协议 `CMD:JSON\r\n` + `0x00` 前缀二进制帧
- 音频：16 kHz / 16 bit / 单声道，固件以 IMA ADPCM（4:1）压缩，SDK 内置解码
- 运行时：Tokio；蓝牙后端 [btleplug](https://github.com/deviceplug/btleplug)
- 平台：macOS / Linux 已真机验证，Windows 未验证（btleplug 本身支持，欢迎反馈）

> 固件对应关系：主控 nRF52840 `v0.3.x` / WiFi 模块 ESP32-C3 `v0.0.9`。协议契约、超时表、固件行为细节见 [docs/protocol-support.md](docs/protocol-support.md)。

## 目录

- [安装](#安装)
- [五分钟上手](#五分钟上手)
- [设备模型（必读）](#设备模型必读)
- [实时音频](#实时音频)
- [按键与侧边开关](#按键与侧边开关)
- [录音文件：两条通道](#录音文件两条通道)
- [WiFi 与热点](#wifi-与热点)
- [错误速查](#错误速查)
- [测试工具](#测试工具)
- [已知限制与路线图](#已知限制与路线图)
- [安全说明](#安全说明)
- [开发与贡献](#开发与贡献)

## 安装

```toml
[dependencies]
reai-card-sdk = { git = "https://github.com/ReAI-com/reai-card-sdk.git", tag = "v0.0.2" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

要求 Rust 1.93+（edition 2024）。`main` 分支可能领先于最新 tag（未发布改动见 [CHANGELOG](CHANGELOG.md)），要用最新 API 可改成 `branch = "main"`。

- **macOS**：首次运行会弹蓝牙授权，需在「系统设置 → 隐私与安全性 → 蓝牙」里允许终端/应用。
- **Linux**：需要 BlueZ 开发头文件（`sudo apt-get install libdbus-1-dev pkg-config`）。
- **Windows**：未验证，进默认构建前请先用 `dyj_cli check` 跑一遍。

## 五分钟上手

```rust,no_run
use std::time::Duration;
use reai_card_sdk::DeviceManager;

#[tokio::main]
async fn main() -> reai_card_sdk::Result<()> {
    let manager = DeviceManager::new().await?;
    let devices = manager.scan(Duration::from_secs(5)).await?;
    let found = devices.first().expect("没有扫到录音卡");
    // 连接即校验 NUS 特征、启动通知任务并自动 SYNC_TIME
    let device = manager.connect(found).await?;

    println!("{:?}", device.device_info().await?);
    println!("{:?}", device.battery().await?);
    println!("{:?}", device.state().await?);   // wifiOpen / recordRealtimeEnabled / …
    Ok(())
}
```

想先不写代码看看设备能做什么：`make web` 打开网页控制台（见[测试工具](#测试工具)）。

## 设备模型（必读）

三条规则决定了 SDK 的大部分行为，理解它们能省掉绝大多数“为什么超时”的排查。

**1. 命令严格串行。** 协议没有请求 ID，SDK 在同一连接上一次只放行一条命令（含多帧列表、WiFi 扫描、文件传输）。并发调用会排队，不会交错。断连或通知流异常时，所有排队中的调用立刻以 `Disconnected` 返回。

**2. WiFi 模块的电源决定 SD 卡归谁。** 录音卡由 nRF52840（BLE、录音、SD 卡）和 ESP32-C3（WiFi、热点、HTTP 文件服务）两颗芯片组成，ESP32 只在 `open_wifi()` 后供电：

| 状态 | 可用 | 不可用 |
| --- | --- | --- |
| WiFi 关（`wifiOpen=false`） | 录音、打点、BLE 文件列表/取走、`storage()` | 所有走 ESP32 的命令（WiFi 扫描/连接、热点、上传、OTA）→ `WifiModulePoweredDown` |
| WiFi 开（`wifiOpen=true`） | 热点快传（HTTP 列表/下载/删除）、WiFi 命令 | SD 卡已交给 ESP32：nRF 侧录音/文件/存储查询 → `SdOwnedByWifiModule` |

SDK 对两类命令都做**上电预检**（先读 `STATE_INFO.wifiOpen`），不满足时立刻返回结构化错误而不是等 10 秒超时。错误信息会直接告诉你该先调 `open_wifi` 还是 `close_wifi`。

**3. 设备会主动说话。** 按键、拨杆、录音状态、热点就绪、关机倒计时等以事件推送，用 `subscribe_events()` 订阅，所有事件都是类型化的：

```rust,no_run
use reai_card_sdk::{ButtonGesture, DeviceEvent, DyjDevice, PowerEvent, RecordReason};

async fn watch(device: &DyjDevice) {
    let mut events = device.subscribe_events();
    while let Ok(event) = events.recv().await {
        match event {
            DeviceEvent::Button(e) if e.gesture == ButtonGesture::Double => {
                println!("双击，设备执行了 {}", e.action)
            }
            DeviceEvent::ToggleSwitch(e) => println!("开关拨到 {}，动作 {}", e.position, e.action),
            DeviceEvent::RecordStatus(s) if s.reason == Some(RecordReason::Switch) => {
                println!("开关让录音变成 {}", s.is_recording)
            }
            DeviceEvent::Power(PowerEvent::ShutdownImminent { seconds }) => {
                println!("设备 {seconds}s 后关机，连接即将断开")
            }
            DeviceEvent::AccessPointReady(_) => println!("热点就绪，可以连 DYJ-HD-AP 了"),
            other => println!("{other:?}"),
        }
    }
}
```

| 事件 | 何时到达 | 载荷 |
| --- | --- | --- |
| `Button(ButtonEvent)` | 按键单击 / 双击 / 长按 / 长按松开 | `gesture`、设备随之执行的 `action` |
| `ToggleSwitch(ToggleSwitchEvent)` | 侧边开关拨到 on / off | `position`、`action` |
| `RecordStatus(RecordStatus)` | 录音开始 / 停止 / 出错 | `is_recording`、`reason`（app / button / switch / auto）、出错时的 `error` 码 |
| `Power(PowerEvent)` | `SHUTDOWN` 之后、低电关机前 | `ShutdownImminent { seconds }`；另有 `DeviceWakeup` / `StartupComplete` |
| `AccessPointReady` | ESP32 上电后 AP + HTTP 服务起好 | 见 [WiFi 与热点](#wifi-与热点) |
| `WifiNetwork` / `WifiOff` / `UploadComplete` / `WifiOtaProgress` | WiFi 扫描结果、模块请求断电、上传完成、OTA 进度 | 对应结构体 |
| `Unhandled { command, payload }` | 认得命令但载荷解析不了（如未来固件的新手势），或 `type` 未知的裸 JSON（此时 `command` 为空） | 原始 JSON |

`record_status()` 主动查询的响应也会经事件通道广播一份，但没有 `reason`——想区分“谁改了录音状态”就看 `reason.is_some()`。

## 实时音频

固件把 16 kHz 单声道 PCM 以 IMA ADPCM 压缩后经 BLE 持续推送（约 8 KB/s）。SDK 在通知任务里统一解码，广播为 `PcmChunk`（`Vec<i16>`，`sample_rate = 16000`）。

```rust,no_run
use reai_card_sdk::DyjDevice;

async fn realtime(device: &DyjDevice) -> reai_card_sdk::Result<()> {
    // 自动开实时传输 + 开始录音；stop()/drop 时自动收尾
    let session = device.start_realtime_recording().await?;
    let mut audio = session.subscribe();
    for _ in 0..250 {                        // 约 5 秒
        let chunk = audio.recv().await.expect("音频通道");
        feed_to_your_pipeline(&chunk.samples); // 16 kHz mono i16
    }
    session.stop().await
}
# fn feed_to_your_pipeline(_: &[i16]) {}
```

- 录音也可以由设备按键触发（实时开关在设备上持久化）。此时无需会话，`device.subscribe_audio()` 被动收流即可。
- 消费过慢时 `recv()` 返回 `RecvError::Lagged(n)`，表示丢了 `n` 块，解码流会有短暂杂音后自行收敛。
- 实时录音进行中 `take_file_to*` 会被拒绝（`RealtimeRecordingActive`）：BLE 二进制通道没有头部，音频和文件数据无法区分。
- 实时录音同时也写 SD 卡 `.apm`，之后仍可整段取回。

## 按键与侧边开关

录音卡有一个按键和一个侧边拨杆开关。它们做什么**由设备自己决定**：六个输入（按键单击 / 双击 / 长按 / 长按松开、开关 on / off）各对应一个存在设备 KV 里的动作，输入触发时固件先上报事件、再在设备侧执行该动作。

**SDK 在这里的职责只有一件：把事件类型化传出去。** `DeviceEvent::Button { gesture, action }` / `ToggleSwitch { position, action }` 里的 `action` 是设备**已经执行**（或因绑定为 `None` 而未执行）的动作，随后若录音状态变化还会跟一条 `RecordStatus { reason: Button | Switch }`。业务层据这些事件做自己的响应（UI、上报、联动其他设备）即可，**不要再替设备重放动作**。

```rust,no_run
use reai_card_sdk::{ControlAction, DeviceEvent, DyjDevice, SwitchPosition};

async fn watch_controls(device: &DyjDevice) {
    let mut events = device.subscribe_events();
    while let Ok(event) = events.recv().await {
        match event {
            DeviceEvent::Button(e) => println!("按键 {}，设备执行了 {}", e.gesture, e.action),
            DeviceEvent::ToggleSwitch(e) if e.position == SwitchPosition::On
                && e.action == ControlAction::RecordStart =>
            {
                println!("用户拨开关开始录音了，准备接收音频")
            }
            _ => {}
        }
    }
}
```

无论绑定是谁、何时写进设备的，只要实时传输开着（`set_realtime_enabled(true)`，设备上持久化），设备侧触发的录音会让 `subscribe_audio()` 的 PCM 块自动到达，上位机不需要发任何命令。

### 可选：改写设备上的绑定表

绑定表本身是六个 `CONFIG_*` 键（`btn_single / btn_double / btn_long_press / btn_long_release / switch_on / switch_off`），SDK 提供了它们的类型化透传。**这是可选能力**：用不用、何时用（例如放进你的设备设置页）、默认值是什么，都由业务层决定；SDK 不预设任何绑定。

```rust,no_run
use reai_card_sdk::{ButtonGesture, ControlAction, ControlBindings, DyjDevice, SwitchPosition};

async fn configure(device: &DyjDevice) -> reai_card_sdk::Result<()> {
    // 单项写入
    device.set_control_action(SwitchPosition::On, ControlAction::RecordStart).await?;
    device.set_control_action(ButtonGesture::Single, ControlAction::RecordMark).await?;
    // 读回整表（设备上没配置过的输入是 None）
    let bindings = device.control_bindings().await?;
    println!("{bindings:?}");
    // 整表写入；`recorder()` 只是一个示例预设（开关控制录音、单击打点、双击切换），不是默认值
    device.apply_control_bindings(&ControlBindings::recorder()).await?;
    // 解绑：回到出厂的"什么都不做"
    device.clear_control_action(ButtonGesture::Double).await?;
    Ok(())
}
```

可用动作：`None`（不动作，出厂默认）、`RecordStart`、`RecordStop`、`RecordToggle`、`RecordMark`；`ControlAction::Other(String)` 用来无损携带未来固件新增的值。绑定存在设备 KV 里，重启后保留。

## 录音文件：两条通道

录音在设备上是裸 IMA ADPCM 的 `.apm` 文件，每个文件有同名 `.txt` 记录时间戳。**取回有两条通道，按文件大小选：**

| 通道 | 适合 | 速度 | 前提 |
| --- | --- | --- | --- |
| BLE 取走 `take_file_to_dir` | 小文件（KB 级）、调试 | ≈3–8 KB/s（固件节奏限制） | WiFi 关 |
| 热点快传 `download_to_dir` | 正式取回、MB 级录音 | 取决于 WiFi 链路 | WiFi 开 + 电脑加入设备热点 |

> ⚠️ BLE 取走是**取走语义**：传输成功后固件会删除设备上的源文件（含同名 `.txt`）。热点下载是只读的，删除需显式调用 `delete_file`。

两条通道落盘行为一致：先写 `<目录>/<原名>.part`，成功后按录音时间改名，失败自动清理半截文件。

```rust,no_run
use reai_card_sdk::DyjDevice;

async fn fetch(device: &DyjDevice) -> reai_card_sdk::Result<()> {
    // 热点快传（见下一节的 WiFi 准备步骤）
    let client = device.access_point_client().await?;
    let page = client.list_files(1, 50).await?;
    for file in &page.files {
        let saved = client.download_to_dir(&file.name, "./downloads").await?;
        // 例：./downloads/20261019-160849_059F19DF.apm
        println!("{} 录音时间 {:?}", saved.path.display(), saved.recorded_at);
        let wav = saved.to_wav().await?;       // 同目录生成 .wav，可直接播放
        println!("{} {:.1}s", wav.path.display(), wav.duration_secs());
    }
    Ok(())
}
```

落盘命名规则（`recording_file_name`）：

| 固件给的名字 | 落盘文件名 | 含义 |
| --- | --- | --- |
| `synced_1792392529.apm` | `20261019-160849_059F19DF.apm` | 本地时间 + 原始编号（编号用于和设备列表对应） |
| `nosync_42.apm` | `nosync-42_059F19DF.apm` | 录音时设备时钟**未同步**，数字不是真实日期 |
| `059F19DF.APM` | `059F19DF.apm` | 设备上没有 `.txt`，无时间信息 |

设备开机后要等 SDK 连上发送 `SYNC_TIME` 时钟才准确——连接之前的录音会是 `nosync`。离线转换任意 `.apm`：`apm_to_wav(apm, wav)`，或 `cargo run --example dyj_cli -- decode in.apm out.wav`。

## WiFi 与热点

固件是**默认热点模式**：`open_wifi()` 给 ESP32 上电后，它自动启动热点 `DYJ-HD-AP`（`192.168.4.1:80`）和 HTTP 文件服务，就绪时推送 `WIFI_AP_START` 事件。

```rust,no_run
use std::time::Duration;
use reai_card_sdk::{AP_DEFAULT_SSID, DyjDevice};

async fn prepare_ap(device: &DyjDevice) -> reai_card_sdk::Result<()> {
    device.open_wifi().await?;                                   // 幂等：已上电则直接返回
    device.wait_for_ap_ready(Duration::from_secs(10)).await?;    // 等 WIFI_AP_START
    println!("请把电脑/手机加入 WiFi「{AP_DEFAULT_SSID}」");        // 密码见设备说明
    // 之后 device.access_point_client() 即可列表/下载
    Ok(())
}
```

- `open_wifi` 是幂等的：BLE 断链重连后 ESP32 往往仍在供电，SDK 检测到 `wifiOpen=true` 会跳过 `WIFI_OPEN` 并视热点为就绪。
- `access_point_client()` 不查询设备，直接使用固件固定端点（`AP_DEFAULT_IP:AP_DEFAULT_PORT`）；若连接建立时模块早已供电、收不到就绪事件，3 秒宽限后按“热点随上电自启”推断就绪。
- 电脑没加入热点时 HTTP 请求会在 5 秒内失败（建连超时），不会挂死。
- 连路由器：`connect_wifi(ssid, password)`（最长约 12 秒）、`wifi_status()`（模块断电时直接返回 `status:"off"`）、`scan_wifi()`、`disconnect_wifi()`。热点激活时固件会跳过自动连路由器，两者互斥。
- `start_access_point` / `stop_access_point` / `access_point_status` 需要带 `AP_` 透传的未来固件，v0.3.x 上调用会等到超时；当前固件用不到它们。

## 错误速查

| `SdkError` | 含义 | 下一步 |
| --- | --- | --- |
| `WifiModulePoweredDown { command }` | 该命令要 ESP32 应答，但模块未上电 | 先 `open_wifi()` |
| `SdOwnedByWifiModule { command }` | 该命令要 nRF 读写 SD 卡，但卡已交给 ESP32 | 先 `close_wifi()`，或改走热点快传 |
| `RealtimeRecordingActive` | 实时录音中不能 BLE 取文件 | 先 `session.stop()` |
| `Timeout { command, seconds }` | 设备未在期限内应答（`WIFI_AP_START` 也用它） | 检查链路/固件状态 |
| `Disconnected` | 连接断开或通知流异常，排队中的请求一并返回 | 重新扫描连接 |
| `Device { command, code, message }` | 固件返回了结构化错误 | 看 `message` |
| `FileLengthMismatch { expected, actual }` | BLE 取走收到的字节数与声明不符 | 文件无效，重试 |
| `Http(_)` | 热点 HTTP 请求失败（403 多为路径带了前导 `/`，SDK 已自动处理） | 确认已加入热点 |

## 测试工具

两个示例程序覆盖全部公开 API，日志为中文 + emoji 分级（`RUST_LOG=debug/trace` 可调细）：

```bash
make cli ARGS=scan                      # 扫描
make cli ARGS=check                     # 只读全流程验收（info/state/battery/storage/modules/files）
make cli ARGS="realtime 30 rt.wav"      # 实时录音收流 30 秒并存 WAV
make cli ARGS="decode in.apm out.wav"   # 离线解码（不需要设备）
make cli ARGS="controls list"           # 读按键/开关绑定表
make cli ARGS="controls preset recorder" # 一键写入录音笔预设
make cli ARGS=events                    # 挂机打印事件（按按键、拨开关看上报）
make web                                # 网页控制台 http://127.0.0.1:8787
```

网页控制台包含：连接管理、状态卡片、全部命令按钮、**实时录音波形面板**（通道常开，数据驱动）、BLE 文件表格、**热点快传一键检查**（上电 → 探测是否已连热点 → 列出 AP 文件并逐行下载）、下载后自动转 WAV、**按键 / 开关绑定表**（读 / 绑定 / 解绑 / 预设）、SSE 实时日志与设备事件。仅绑定 `127.0.0.1`、无鉴权，请勿暴露到局域网。

## 已知限制与路线图

- **Windows 未验证**：btleplug 支持 Windows，但本 SDK 未在 Windows 上跑过真机。
- **云上传（STA 模式上传到服务器）暂未启用**：`upload_file_via_sta` 等 API 已实现，但需要 STA 模式 + 接收服务器，当前固件默认热点模式，后续版本再启用。
- **AP_\* 透传**：当前固件 v0.3.x 不透传 `AP_START/AP_STOP/AP_STATUS`，相关 API 保留给未来固件。
- **BLE 取走大文件**：固件以 200 B/25 ms 发送，MB 级文件请走热点快传。
- **crates.io**：暂以 git tag 方式分发，发布到 crates.io 待定。

## 安全说明

- 设备热点使用固定密码，而 HTTP 文件接口**没有鉴权**——密码就是录音文件的唯一访问控制。因此 **SDK 不内置热点密码**，应用应从自己的配置或设备文档获取；网页控制台可用环境变量 `REAI_CARD_AP_PASSWORD` 注入显示。
- 热点只在 `open_wifi()` 后存在，用完请 `close_wifi()`。

## 开发与贡献

```bash
make verify      # fmt --check + check + test + clippy -D warnings（CI 同款）
make test
```

- 改动涉及协议行为时请同步更新 [docs/protocol-support.md](docs/protocol-support.md)，并在 [CHANGELOG.md](CHANGELOG.md) 记录。
- 单测用 mock 传输覆盖协议层；真机相关结论请在 PR 里注明固件版本。
- Issue / PR 欢迎提交到本仓库。

## 许可证

[MIT](LICENSE) © 2026 ReAI
