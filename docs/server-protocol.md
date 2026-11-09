# 服务端协议契约（设备 / SDK 视角）

> 本文从 SDK 与固件消费者的角度，完整描述设备与后端服务之间**实际在用的协议**：
> OTA 版本信息、录音上传，以及四条取数路径（BLE 取走 / AP 下载 / STA 上传 / 一键同步）的差异。
> 服务端现状参考主项目 [docs/DEPLOY.md](../../docs/DEPLOY.md)（dyj2.dev.reai.com 测试服务端，nginx 静态 + Go 上传服务）。
> 上传接口的测试参考实现见 `tools/apiServer/docs/`（主项目，JWT 版旧接口，与本文 §3 的现网接口不同）。
>
> 配套版本：SDK reai-card-sdk 0.0.6 / 主控 0.3.10（versionCode 310）/ WiFi 0.1.3。

---

## 1. 服务端有两个，别混

| | 测试服务端 dyj2.dev.reai.com | 旧测试 apiServer（主项目 `tools/apiServer`） |
|---|---|---|
| 用途 | **现网设备实际对接**的收发服务 | 仅协议开发调试用 |
| 上传接口 | `POST /api/upload/<deviceId>/<文件名>`（或 `/api/upload/<文件名>` + `x-device-id` 头） | `POST /api/files/:deviceId/upload`（JWT 必带） |
| 认证 | 设备有 token 才带 `Authorization`（可选） | JWT Token 必带 |
| 语义 | 2xx 即持久化，设备删源 | 2xx 返回文件元数据 JSON |
| OTA | `/api/ota/info?mod=main\|wifi\|app` | `/api/ota/info?mod=main\|wifi` |

新转写/接收服务对接**只看本文契约**，不关心背后是 nginx 静态还是真后端。

---

## 2. OTA 版本信息接口（契约硬约束）

**请求**（设备端发起，无需鉴权）：

```text
GET {api_url}{ota_path}
# api_url 为设备 NVS 的 CONFIG 键（编译期默认 esp32c3wifi/config.h DEFAULT_API_BASE_URL）
# ota_path 默认 /ota/info?mod=wifi；SDK 可经 set_sta_api_base_url / set_wifi_ota_info_path 修改
# mod: main = nRF 主控，wifi = ESP32，app = Android App
```

**响应**（200，application/json）：

```json
{"version":"0.3.10","versionCode":310,"url":"https://dyj2.dev.reai.com/ota/fw_main/app_update_v310.zip"}
```

**设备端解析规则**（`esp32c3wifi/src/http_client.cpp`，硬编码行为）：

| 约束 | 说明 |
|---|---|
| `version` / `versionCode` / `url` 三字段**必须同时存在** | 缺任何一个，设备判定响应非法，直接放弃本次检查 |
| `url` 必须是**绝对地址** | 设备拿它直接下载，不做拼接；固件文件可与 API 不同域（放对象存储/CDN 即可） |
| 升级条件：`versionCode > 设备当前值` | 否则视为「已是最新」；版本号约定 minor×100+patch（`0.3.10 → 310`，wifi `0.1.3 → 103`） |
| `mod` 非法 | 服务端应返回 400，设备按失败处理 |

**固件文件格式**：

| mod | 格式 | 说明 |
|---|---|---|
| main | nRF DFU zip（`nrfutil pkg generate`，私钥签名） | 生成参数：`--hw-version 52 --sd-req 0x100 --application-version <versionCode>`；`sd-req` 必须是 `0x100`，写 `0xFF` 设备拒收 `SoftDevice version failure (ext 7)` |
| wifi | ESP32 **app-only bin**（不是 merged bin） | 设备走 ESP32 HTTPUpdate，失败自动回滚；同一上电周期必须先 `OTA_WIFI_CHECK` 再 `OTA_WIFI_UPDATE` |
| app | APK | 由 App 端下载安装，设备不经手 |

---

## 3. 录音上传协议（STA / 一键同步共用）

### 3.1 地址与请求

**请求**（设备 → 服务端，流式 POST）：

```text
POST {upload_url}/<上传文件名>
Header: Content-Type:   application/octet-stream   （固定）
Header: x-filename:     <上传文件名>                （同 URL 尾段）
Header: x-filesize:     <文件字节数>                （必带）
Header: x-format:       adpcm                      （固定，录音格式）
Header: x-device-id:    <设备ID>                    （必带：激活的设备 ID，未激活用 MAC 去冒号）
Header: authorization:  Bearer <token>             （可选：设备已激活才有 token）
Body:   文件二进制（HTTP Stream，边读 SD 边发，无大小上限）
```

- **URL 拼接**：`upload_url` + `/` + 上传文件名。`upload_url` 来自设备 NVS 的 CONFIG 键（SDK `set_wifi_upload_url`，上限 63 字节），请求级 `url` 参数（`UPLOAD_FILE`/`UPLOAD_ALL`）优先于 NVS。
- **推荐短形式**：`https://<域名>/api/upload`（**不带设备 ID**）。服务端按路径 `/upload/<deviceId>/<文件名>` 落盘；`/upload/<文件名>` 时设备目录取 `x-device-id` 头。两种路径都要支持——固件两种都不区分。
- **上传文件名**由固件生成（`upload_manager.cpp generateUploadFileName`）：`synced_<unix秒>_<会话ID>_<seq>.apm`；前缀 `synced_`/`nosync_` 取决于录音当时主控时钟是否已同步（`nosync_` 时间戳为 0）。**设备本地原始名与上传名解耦**，服务端以 `x-filename` / URL 尾段为准。

### 3.2 成功判定与删源约定（重要）

| 状态码 | 设备行为 |
|---|---|
| **2xx**（200–299） | **视为上传成功 → 立即删除 SD 卡上的源文件**（`upload_manager.cpp`：`httpCode >= 200 && httpCode < 300` 即 `deleteUploadedFiles()`） |
| 非 2xx / 连接失败 / 超时 | 保留源文件，下次 `UPLOAD_ALL` 或一键同步会重传 |

由此服务端必须保证：

1. **2xx 返回前文件已持久化**。设备收到 2xx 就删源，此后服务器不能丢这个文件——2xx 是「落盘完成」的承诺，不是「收到即成功」。
2. **重复上传 = 幂等覆盖**。无重试队列与去重机制：网络失败源文件还在，下一轮会再次上传同名（同会话同 seq）文件；同名异内容（如录音会话被合并后重传第二版）服务端应**以后到的为准覆盖**。当前测试服务端按文件名落盘自然覆盖，契约上以此为准。
3. **无部分写恢复**。上传中断的文件不会带分片标记，服务端应将非 2xx 场景下的半截文件视为垃圾（不进入结果列表，或被后续完整重传覆盖）。
4. **无内容校验反馈**。固件不带 hash，`x-filesize` 是唯一长度参考；服务端可选校验长度不符返回 4xx（设备会保留源文件待重传）。

> ⚠️ 删源语义对**所有**上传路径生效（BLE 取走、AP 下载、STA 上传、一键同步），见 §5。

### 3.3 上传结果查询

测试服务端提供 `GET /files/<deviceId>` 列出已收文件（对账用）。上传文件按 `synced_<ts>_<会话>_<seq>.apm` 命名，可与会话/切片对应。**这不是契约必选接口**——设备上传后不回查；转写服务可自定结果查询接口，设备侧无依赖。

---

## 4. 设备标识（认证怎么做是服务方的业务决策）

- 固件每次上传必带 `x-device-id`（激活的设备 ID；未激活用 MAC 去冒号）——这是设备侧唯一强制的身份信息。
- `authorization: Bearer <token>` 为**可选头**：设备 NVS 里有 token 就带，没有就不带。**未激活设备也能上传**。
- 是否启用 code→token 激活流程、token 如何签发与校验、设备如何绑定到用户/门店——这是服务方的业务逻辑，由服务方自行决定；固件只负责「有 token 就带上」。SDK 提供对应命令通道（`activate` / `clear_activation`），但对接方可以完全不用。

---

## 5. 四条取数路径对比（转写服务只需要关心后两条的上行）

| 路径 | 通道 | 何时删源 | 转写服务可见性 |
|---|---|---|---|
| BLE 取走 | 手机 ↔ 设备 BLE，`take_file_to` 分块拉取 | **拉完即删**（固件行为，与对端是否成功落盘无关） | 服务端**看不到**该文件 |
| AP 快传下载 | 手机连设备热点，ESP32 `GET /api/files/download` | 同上，下载完即删 | 服务端看不到 |
| STA 上传 | 设备 Wi-Fi → 服务端 HTTP | **2xx 才删**（§3.2） | 服务端直接收到 |
| 一键同步 | 设备自主：开 Wi-Fi → 连已存网络 → `UPLOAD_ALL` → 关 Wi-Fi | 同 STA | 服务端直接收到 |

> BLE/AP 路径「下载完即删」意味着**传输对端丢弃数据 = 文件丢失**，无服务端兜底。转写服务对接场景应引导用户走 STA 上传或一键同步（`sync_upload` 绑定充电事件可实现「插电自动传云」）。

---

## 6. 换服务器的两种方式（存量设备 / 新固件）

| 方式 | 操作 | 适用 |
|---|---|---|
| 运行期切参数 | `CONFIG_SET api_url` / `upload_url`（SDK `set_sta_api_base_url` / `set_wifi_upload_url`，或 dyj_web 批量下发） | 存量设备，立即生效，无需 OTA，可随时改回 |
| 编译期默认值 | 改 `esp32c3wifi/config.h` 的 `DEFAULT_API_BASE_URL` → 重编 → 版本号 +1 | 新出厂 / 随 OTA 下发 |

`upload_url` 持久化需主控 ≥0.3.7 + WiFi ≥0.1.0；更早版本每次上传须显式带 `url`。值上限 63 字节（**带 27 位设备 ID 的长 URL 超限**，必须用短形式，见 §3.1）。

---

## 7. 明确未定 / 服务端需回答的部分

以下行为当前**只有测试服务端的实现参考，无固件级契约**，转写服务方需给出自己的约定（对应核对清单第 2、4 组问题）：

1. 长度/hash 校验：固件只带 `x-filesize`；是否校验、不符时返回码，由服务端定（建议 4xx，设备保留源重传）。
2. 同名异内容的合并语义（§3.2 第 2 条的「以后到为准」是测试服务端行为，正式服务可改版本化保留）。
3. 转写结果查询/下载/取消/超时的接口形态——设备侧无依赖，纯服务端自由度。
4. ASR 供应商、语言/模型配置、保存期限——服务端配置，与设备无关。
5. 认证/激活：code→token 流程是否采用、token 签发与校验、设备↔用户绑定规则——服务方业务逻辑（见 §4）。

设备端硬约束（不可协商）：§2 OTA 三字段与 versionCode 比较、§3.1 请求头五件套与 URL 拼接、§3.2 2xx 删源、§3.1 63 字节上限。
