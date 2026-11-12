# reai-card-sdk 0.0.8 更新说明（录音分片时长可配 + 蓝牙取最旧文件）

> **面向对象**：消费 SDK 的上位机/服务端开发同学。本文自包含，不需要读固件代码。
> **版本配套**：SDK **0.0.8**；新能力需固件 **主控 ≥0.3.13**（WiFi 模块无改动，仍 0.1.4；版本对照见 `protocol-support.md`）。
> **一句话总结**：录音分片从固定 10 分钟变成**可配**（如 1 分钟一片）；新增 `FILE_UPLOAD_OLDEST`——上位机调一次，设备把 SD 上**最旧的**（非正在录的）录音经 BLE 传回并删除，形成"短分片 → 录完即取"的流水线。

---

## 1. 升级了什么（对比 0.0.7）

| 能力 | 0.0.7（旧行为） | 0.0.8（新行为） |
|---|---|---|
| 录音分片时长 | 固定 600 秒（10 分钟），不可配 | `start_recording_with_duration(secs)`：`RECORD_START` 带 `{"duration":<秒>}`，固件下限 10 秒 |
| 分片时长生效范围 | — | 仅影响**本次开始**的录音；不带参数的 `start_recording`/`start_recording_with_session` 维持固件当前值（默认 600） |
| 取文件方式 | 只能按名字 `take_file_to(name)`（先 `list_files` 查名字） | 新增 `take_oldest_file_to(writer)` / `take_oldest_file_to_dir(dir)`：设备自选最旧文件，无需先查列表 |
| SD 空时取文件 | — | 设备回 fail（`无可上传文件`），源文件保留，可稍后重试 |

## 2. API 一览

```rust
// 分片时长（主控 ≥0.3.13；旧固件忽略该参数，行为与 start_recording 一致）
let resp = device.start_recording_with_duration(60).await?;   // 1 分钟一片
// resp.extra["duration"] = 60 —— 固件实际生效值（低于 10 会被钳到 10）

// 蓝牙取最旧文件（主控 ≥0.3.13）
let taken = device.take_oldest_file_to(&mut writer).await?;   // 流式写 writer
let saved = device.take_oldest_file_to_dir("downloads/").await?; // 或直接落盘（自动转 WAV 可选）
// taken.name 是设备 start 帧给出的改名结果：
//   synced_/nosync_<时间戳>[_会话_段序号].apm（与 take_file_to 同一套改名规则）
```

## 3. 传输语义（与 take_file_to 完全一致）

- 帧序：`{"state":"start","name":"...","size":N}` → 200 字节/25ms 二进制块 → `{"state":"end"}`；
  响应帧命令字为 **`FILE_UPLOAD_OLDEST`**（不是 `FILE_UPLOAD`）——SDK 的帧匹配跟随发起命令，消费方无感。
- **传完删除**：设备在 end 帧后才删源 `.APM` 与同名 `.TXT`；传输中断/失败不删，可重试。
- **选文件规则**：根目录 `.APM` 按 FAT mtime 最旧优先；**跳过正在录写的文件**（录音继续进行不受影响）。
- **节奏自控**：固件不做任何自动触发——何时取、取多频，完全由上位机决定。
  推荐：配 1 分钟分片 + 录音中周期性调 `take_oldest_file_to_dir`，取完即删不积压。
- 实时录音会话进行中 SDK 仍会拒绝（`RealtimeRecordingActive`），与 `take_file_to` 相同。

## 4. 兼容性矩阵

| SDK | 主控 | 分片 duration | FILE_UPLOAD_OLDEST |
|---|---|---|---|
| 0.0.8 | ≥0.3.13 | ✅ 生效 | ✅ 可用 |
| 0.0.8 | ≤0.3.12 | 忽略参数（默认 600s） | 命令字未注册：固件**不回帧**，SDK 走到 idle 超时报错（固件对未知命令静默丢弃，除非前缀命中透传规则） |
| ≤0.0.7 | ≥0.3.13 | —（无该方法） | 可用 `raw_command("FILE_UPLOAD_OLDEST", None)` 自行消费 |
| ≤0.0.7 | ≤0.3.12 | 旧行为 | 旧行为 |

## 5. dyj_web 测试台

`cargo run --example dyj_web -- --port 8787`（仅 127.0.0.1，开发用无鉴权）：

- **02 录音**区新增「分片秒」输入框——填了就随 `RECORD_START` 发 `duration`，留空走默认；
- **03 文件 BLE** 区新增「取最旧（0.3.13+）」按钮（`POST /api/take-oldest`），取回文件存 `downloads/`，「落盘后转 WAV」勾选同样生效。

## 6. 升级清单（消费方）

1. `Cargo.toml`：`reai-card-sdk = "0.0.8"`；
2. 需要短分片流水线的：录音改为 `start_recording_with_duration(60)`，并在录音中周期调 `take_oldest_file_to_dir`；
3. 不需要新能力的：无任何破坏性变更，升级后行为与 0.0.7 完全一致。
