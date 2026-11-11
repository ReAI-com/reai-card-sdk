# reai-card-sdk 0.0.7 更新说明（一键同步事件模型 v2）

> **面向对象**：消费 SDK 的上位机/服务端开发同学。本文自包含，不需要读固件代码。
> **版本配套**：SDK **0.0.7**；完整事件能力需固件 **主控 ≥0.3.12 + WiFi ≥0.1.4**（版本对照见 §7 兼容性矩阵）。
> **一句话总结**：一键同步（`sync_upload`）的过程事件从 5 种粗粒度状态升级为**全程细粒度可见**——正在试哪个 WiFi、连上了哪个热点（拿到什么 IP）、每个文件何时开始/结束、失败的具体原因，全部以 `DeviceEvent::Sync` 事件实时推送。

---

## 1. 升级了什么（对比 0.0.6）

| 能力 | 0.0.6（旧行为） | 0.0.7（新行为） |
|---|---|---|
| WiFi 连接成功 | 只有状态跳变 `connecting`，热点名混在 message 里，IP 丢失 | `connecting` 事件带 **`ssid` + `ip`** 字段 |
| 正在试哪个 WiFi | **完全静默**（多凭据依次尝试最长 50s+ 盲等） | 新事件：`WIFI_AUTOCONNECT_TRY` → 正在试第 N/M 个 |
| 文件开始上传 | **完全静默**（大文件分钟级盲等） | 新事件：`UPLOAD_START` → 开始传第 N/M 个文件 |
| 上传失败原因 | 一律「上传失败」 | message 细分：`HTTP 500` / `网络连接失败` / `网络错误(-1)` / `上传前置失败` |
| 上传总结帧 | `UPLOAD_ALL` 帧被 SDK **静默丢弃** | 映射为 `Sync(closing)` 事件，带 `uploaded`/`failed`/`total` |
| 失败细节 | error 帧只有错误码 | error/closing 事件带 `message`（如 `autoconnect_timeout`）；查询响应带 `lastMessage` |
| 坏帧处理 | 含无效 UTF-8 的帧**整行丢弃** | 降级解析（U+FFFD 替换）尽量救回（§5） |

## 2. SyncEvent 字段总表

```rust
pub struct SyncEvent {
    pub state: Option<SyncState>,       // opening/connecting/uploading/closing/done/error/idle
    pub trigger: Option<SyncTrigger>,   // app/button/switch/charge
    pub current: Option<String>,        // 正在上传的卡上文件名（8.3 名）
    pub done: Option<u32>,              // 语义随事件：完成文件数 / 第 N 个尝试
    pub total: Option<u32>,             // 总文件数 / 总凭据数
    pub code: Option<u32>,              // 0=正常推进 1=出错
    pub error: Option<String>,          // 错误码（见 §4 错误码表）
    pub message: Option<String>,        // 失败原因细分 / ssid（TRY 与 AUTOCONNECT 事件）
    pub last_state: Option<SyncState>,  // 仅 idle 查询：上次结果
    pub last_error: Option<String>,     // 仅 idle 查询：上次错误码
    pub last_message: Option<String>,   // 仅 idle 查询：上次失败细节
    pub ssid: Option<String>,           // ★连接成功的热点名 / TRY 帧的 ssid
    pub ip: Option<String>,             // ★连接成功拿到的 IP
    pub index: Option<u32>,             // ★第 N 个（1 起算）：文件序号 / 凭据序号
    pub count: Option<u32>,             // ★总数：待传文件数 / 凭据数
    pub uploaded: Option<u32>,          // ★UPLOAD_ALL：成功数
    pub failed: Option<u32>,            // ★UPLOAD_ALL：失败数（>0 = 失败文件保留在卡上）
}
```

所有 `★` 字段均为 `Option`：旧固件不下发时为 `None`，**消费方按需读取，缺省不报错**。

## 3. 帧 → 事件映射规则

| BLE 帧 | → SyncEvent | 字段承载 |
|---|---|---|
| `SYNC_STATUS:{...}` | 原样解析 | state/trigger/current/done/total/code/error/message/ssid/ip/lastMessage |
| `UPLOAD_PROGRESS:{name,done,total,code,message}` | `state=Uploading` | current=name，message=失败原因细分 |
| `UPLOAD_START:{name,index,total}` ★新 | `state=Uploading` | current=name，done=index，total |
| `UPLOAD_ALL:{code,uploaded,failed,total,message}` ★新映射 | `state=Closing` | uploaded/failed/total/message；**成败看 `failed`**（code 恒 0） |
| `WIFI_AUTOCONNECT:{code:0,ssid,ip,lastUsed}` | `state=Connecting` | ssid/ip 字段 + message=ssid（兼容旧消费） |
| `WIFI_AUTOCONNECT:{code:1,message}` | `state=Connecting` | error=message（`no_credentials`/`all_failed`） |
| `WIFI_AUTOCONNECT_TRY:{ssid,index,count}` ★新 | `state=Connecting` | message=ssid，done=index，total=count |

## 4. 完整事件序列（真实时序示例）

一次成功的一键同步（`SYNC_START` 或绑定触发），上位机事件流按此顺序收到：

```jsonc
SYNC_STATUS   {"state":"opening","trigger":"app","code":0}
SYNC_STATUS   {"state":"connecting","trigger":"app","code":0}
WIFI_AUTOCONNECT_TRY {"ssid":"LcTaoZiRoom","index":1,"count":2}     // 正在试第 1 个已存 WiFi
SYNC_STATUS   {"state":"connecting","trigger":"app","code":0,
               "ssid":"LcTaoZiRoom","ip":"192.168.31.15"}           // ★连上了哪个热点、什么 IP
UPLOAD_START  {"name":"3C6CF5EE.APM","index":1,"total":2}           // 开始传第 1 个文件
SYNC_STATUS   {"state":"uploading","trigger":"app",
               "current":"3C6CF5EE.APM","done":1,"total":2,"code":0}
UPLOAD_START  {"name":"79C12C8E.APM","index":2,"total":2}
UPLOAD_PROGRESS {"name":"79C12C8E.APM","done":2,"total":2,
                 "code":1,"message":"HTTP 500"}                     // ★失败原因细分
SYNC_STATUS   {"state":"uploading","trigger":"app",
               "current":"79C12C8E.APM","done":2,"total":2,"code":1,"message":"HTTP 500"}
SYNC_STATUS   {"state":"closing","trigger":"app","code":0}
SYNC_STATUS   {"state":"done","trigger":"app","done":1,"total":2,"code":0}
UPLOAD_ALL    {"code":0,"uploaded":1,"failed":1,"total":2,
               "message":"部分文件上传失败（失败文件已保留）"}          // ★总结（failed>0 = 有文件保留在卡上）
```

失败路径的错误码：`wifi_open_failed` / `wifi_open_timeout` / `no_credentials` / `connect_failed` / `upload_url_missing` / `upload_failed`；error/closing 事件的 `message` 可带 `autoconnect_timeout` / `upload_timeout` 等细节。

## 5. 坏帧降级解析（0.0.7 行为变更）

固件 UART 丢字节（已知问题 dyj-hd-v2#11 回归期）最常砍在中文 message 的多字节序列中间，导致整帧非法 UTF-8。

- **0.0.6**：整帧非法 → SDK 跳过该行，事件消失（日志 `⚠️ 跳过无法解析的行`）。
- **0.0.7**：降级解析——非法字节替换为 `�`（U+FFFD），**JSON 结构（ASCII）通常完好，帧被救回**，事件照常派发（message 内可能含 �）。只有 JSON 结构本身被砸烂才维持跳过（日志 `⚠️ 帧含无效UTF-8，已用替换字符降级解析`，且跳过时仍有 Fault 事件上报）。

消费方处理建议：展示 message 时对 U+FFFD 容忍即可，无需特殊逻辑；不要因含 � 而丢弃事件。

## 6. 消费示例

### Rust（订阅事件）

```rust
let mut events = device.subscribe_events();
while let Some(event) = events.recv().await {
    if let DeviceEvent::Sync(sync) = event {
        match sync.state {
            Some(SyncState::Connecting) => {
                if let (Some(s), Some(ip)) = (&sync.ssid, &sync.ip) {
                    println!("已连上热点 {s}（{ip}）");
                } else if let Some(ssid) = &sync.message {
                    println!("正在试 WiFi：{ssid}（{}/{}）", sync.done, sync.total);
                }
            }
            Some(SyncState::Uploading) => {
                if let (Some(i), Some(t)) = (sync.done, sync.total) {
                    let name = sync.current.as_deref().unwrap_or("-");
                    let ok = sync.code == Some(0);
                    println!("文件 {name}: {} ({i}/{t}){}", if ok { "完成" } else { "失败" },
                             sync.message.as_deref().map(|m| format!(" {m}")).unwrap_or_default());
                }
            }
            Some(SyncState::Closing) if sync.failed.is_some() => {
                println!("上传结束：成功 {} 失败 {}", sync.uploaded, sync.failed);
            }
            Some(SyncState::Done) => println!("同步完成"),
            Some(SyncState::Error) => println!("同步失败: {:?}", sync.error),
            _ => {}
        }
    }
}
```

### HTTP / SSE 消费方（dyj_web 同款）

事件经 `/api/events`（SSE）推送，`SyncEvent` 序列化为 JSON，新字段自动透出：

```jsonc
event: hardware
data: {"command":"SYNC_STATUS","payload":{"state":"connecting","ssid":"LcTaoZiRoom","ip":"192.168.31.15","code":0,"trigger":"app"}}
```

前端渲染建议：`connecting` 且带 `ssid` → 显示「已连上 X」；`UPLOAD_START` → 「正在传 X (i/N)」；`uploading` 且 `code:1` → 显示 message。

## 7. 兼容性矩阵（哪台设备能看到哪些事件）

| 固件组合 | 能看到的事件 |
|---|---|
| 主控 ≥0.3.12 + WiFi ≥0.1.4（完整） | §4 全序列 |
| 主控 0.3.12 + WiFi 0.1.3 | 无 TRY/UPLOAD_START；UPLOAD_PROGRESS message 仍「上传失败」；connecting(ssid/ip)、UPLOAD_ALL 总结、error message 均可用 |
| WiFi 0.1.4 + 主控 ≤0.3.11 | TRY/UPLOAD_START 由旧主控原样转发，SDK 0.0.7 可见；SYNC_STATUS 无 ssid/ip |
| 全旧固件 + SDK 0.0.7 | 等同 0.0.6 行为，新字段为 None |
| **老批次卡发 WiFi 固件** | ⚠️ bin 必须 ≤ 0x140000（1,310,720B）——出厂分区表 OTA 槽位限制（详见主控仓 `docs/updates/v0.3.12-wifi0.1.4.md`） |

## 8. 升级清单（消费方）

- [ ] SDK 依赖升至 `reai-card-sdk = "0.0.7"`
- [ ] （可选）UI 增加「正在试第 N 个 WiFi」「正在传 X (i/N)」「已连上热点 X」三类展示
- [ ] （可选）上传失败展示 `message` 细分（`HTTP 500` / `网络连接失败` 等）
- [ ] （可选）使用 `uploaded`/`failed` 做结果统计展示
- [ ] 不需要改的：事件订阅方式、既有 state 处理逻辑、`WIFI_AUTOCONNECT` 的 message 消费（兼容保留）

## 9. 已知限制

- `UPLOAD_START` 事件 `code` 恒为 `None`（开始帧没有该字段），成败判断只适用于 `UPLOAD_PROGRESS`/`UPLOAD_ALL`。
- `WIFI_AUTOCONNECT_TRY` 的 ssid 含极端字符（引号/反斜杠）时固件做了最小转义，不会产生坏 JSON。
- 单个坏帧若 JSON 结构被砸烂（不只是中文被砍），仍会跳过——根治需固件侧 UART 接收重构（dyj-hd-v2#11，下一主控版本）。
- 同步进行中 ESP32 被上传独占，期间其它 WiFi/上传命令排队是既有行为，与 0.0.7 无关。
