# DYJ Rust SDK protocol support

This document is the Rust SDK's implementation contract. It is derived from the current nRF52840 firmware command parser and handlers in `nrf52840dk/src/protocol/`, not solely from legacy Web tooling.

## BLE transport

| Item | Value |
| --- | --- |
| Service | `6e400001-b5a3-f393-e0a9-e50e24dcca9e` |
| Central writes to | `6e400002-b5a3-f393-e0a9-e50e24dcca9e` |
| Central subscribes to | `6e400003-b5a3-f393-e0a9-e50e24dcca9e` |
| Text frame | `COMMAND[:JSON]\r\n` (SDK also accepts LF-only firmware frames) |
| Binary file frame | one leading `0x00`, followed by file bytes |
| Binary realtime-audio frame | same `0x00` prefix; the wire cannot distinguish it from a file frame — context does (see below) |

The firmware can reassemble fragmented commands. The SDK nevertheless writes at most 20 bytes per BLE write with a 10 ms inter-chunk delay, which is safe before ATT MTU negotiation. Notifications are decoded by one background task.

## Connection and concurrency

`DeviceManager::connect` connects, discovers services, validates the exact NUS write/notify pair, enables notify, starts the notification task and calls `SYNC_TIME` with the current Unix seconds.

The wire protocol does not contain a request ID. The SDK consequently permits only one in-flight command, including multi-frame lists, WiFi scans and file transfer. It does not retry a device operation automatically. A disconnect — or any failure of the notification stream itself — completes active and queued operations with `Disconnected` immediately.

| Operation | Timeout | Reason |
| --- | --- | --- |
| Ordinary command / list frame | 10 seconds | |
| `WIFI_OPEN` | 35 seconds | The nRF firmware rolls back and answers `code:3 wifi_module_timeout` at exactly 30 s; the host timeout must stay strictly above that or the host gives up first and never sees the structured error. |
| `WIFI_CONNECT` | 20 seconds | The ESP32 handles it synchronously and blocks up to ~11.4 s (1.4 s of setup delays + a 10 s association wait). |
| `UPLOAD_FILE` | 600 seconds | The ESP32 responds only after the whole file has been uploaded over HTTP; the ceiling only bounds the failure case. |
| `take_file_to` idle interval | 30 seconds | The firmware streams a chunk every 25 ms. |

## Supported API surface

| Group | Rust methods | Firmware command(s) |
| --- | --- | --- |
| Device | `device_info`, `state`, `sync_time`, `storage`, `battery` | `BASE_INFO`, `STATE_INFO`, `SYNC_TIME`, `STORAGE`, `BATTERY` |
| Recording | `start_recording`, `stop_recording`, `toggle_recording`, `record_status`, `set_realtime_enabled`, `record_mark`, `start_realtime_recording`, `subscribe_audio` | `RECORD_*`, `RECORD` |
| Files | `list_files`, `delete_file`, `clear_files`, `take_file_to` | `FILE_LIST`, `FILE_DELETE`, `FILE_CLEAR`, `FILE_UPLOAD` |
| WiFi / AP control | `open_wifi`, `close_wifi`, `scan_wifi`, `connect_wifi`, `disconnect_wifi`, `wifi_status`, `start_access_point`, `stop_access_point`, `access_point_status` | `WIFI_*`, `AP_*` through nRF UART passthrough |
| AP fast transfer | `AccessPointClient::{device_info,list_files,download_file_to,download_to_dir,delete_file}` | ESP32 HTTP `GET /api/files`, `GET /api/files/download`, `DELETE /api/files/delete` |
| AP-mode WiFi OTA | `AccessPointClient::{ota_status,upload_firmware}` | ESP32 HTTP `GET /api/ota/check`, `POST /api/ota/upload` (multipart `firmware`) |
| Main-controller DFU | `DeviceManager::{scan_dfu_targets,connect_dfu,update_main_firmware}`, `DfuTarget::upload`, `DfuPackage` | `OTA` to enter the bootloader, then Nordic Secure DFU over GATT `0xFE59` |
| OTA info | `OtaInfo::{fetch,is_newer_than,download}` | API server `GET /api/ota/info?mod=main\|wifi` |
| STA upload/auth | `list_uploads`, `upload_file_via_sta`, `delete_upload`, `clear_uploads`, `activate`, `clear_activation` | `UPLOAD_*`, `AUTH_*` |
| WiFi OTA | `check_wifi_ota`, `start_wifi_ota`, `update_wifi_firmware`, `wifi_ota_status` | `OTA_WIFI_CHECK`, `OTA_WIFI_UPDATE` (+ `OTA_WIFI_PROCESS` pushes), `OTA_WIFI_STATUS` |
| Device config | `set_config`, `get_config`, `config_value`, `delete_config`, `set_sta_*`/`sta_*`, `set_wifi_ota_info_path`/`wifi_ota_info_path`, `config_key_routes_to_wifi_module`, `set_led(LedState)`, `test_modules` | `CONFIG_*`, `LED`, `TEST_CHECK` |
| Button / switch bindings | `control_action`, `set_control_action`, `clear_control_action`, `control_bindings`, `apply_control_bindings` | `CONFIG_GET/SET/DEL` on the six keys `btn_single`, `btn_double`, `btn_long_press`, `btn_long_release`, `switch_on`, `switch_off` |
| Lifecycle | `restart`, `shutdown`, `enter_dfu_mode` | `RESTART`, `SHUTDOWN`, `OTA` |
| Escape hatch | `raw_command` | validated arbitrary command with a same-name response |

`FILE_DELETE` uses firmware's real `{"filename":"..."}` request field. `FILE_UPLOAD` uses `{"name":"..."}`. This distinction is deliberate.

`WIFI_STATUS` is only answerable while the ESP32 is powered: the nRF forwards it over UART, and an unpowered module simply never replies. `wifi_status` therefore reads `STATE_INFO.wifiOpen` first and returns the powered-down state (`status: "off"`, `enabled: false`) without a wire round-trip. Status vocabulary: `off` (module powered down, synthesized by the SDK), `disabled` / `enabled` / `connected` (reported by the ESP32).

This powered-down short-circuit is not unique to `WIFI_STATUS`: every UART-passthrough command (`WIFI_SCAN`, `WIFI_CONNECT`, `WIFI_DISCONNECT`, `AP_*`, `UPLOAD_*`, `AUTH_*`, `OTA_WIFI_*`) is pre-checked against `STATE_INFO.wifiOpen` and fails fast with `WifiModulePoweredDown` instead of burning its timeout on an unanswered frame. Only `open_wifi`/`close_wifi` (handled locally by the nRF) skip the check.

`TEST_CHECK` is special: the firmware returns exactly 11 module results without an `index: -1` terminator, so `test_modules` waits for all 11 rather than treating it as an indexed list.

### Button and side-switch bindings

The nRF stores one action per physical input in its KV settings (`setting_kv`, 16 slots shared with every other key, 32-byte keys / 64-byte values). When an input fires, `main.c` reads the key, reports the event **with the bound action** (`BTN_CLICK {"key","action"}` / `TOGGLE_SWITCH {"state","action"}`) and then performs the action itself on the device (deferred to the main loop because the button timer callback cannot touch the SD card or PDM). The host must not replay the action.

Division of responsibility: the SDK's job here is to deliver the typed events (`DeviceEvent::Button` / `ToggleSwitch`, plus the `RECORD_STATUS` push with `reason: button|switch`). The binding methods (`control_action` / `set_control_action` / `clear_control_action` / `control_bindings` / `apply_control_bindings`) are only a typed pass-through over `CONFIG_GET/SET/DEL` for the six keys below — **optional**: whether, when and with which defaults the bindings are written is the application's decision, and the SDK ships no default binding (`ControlBindings::recorder()` is an example preset, nothing more).

| Input | KV key | Reported as |
| --- | --- | --- |
| button single click | `btn_single` | `BTN_CLICK key=single` |
| button double click | `btn_double` | `BTN_CLICK key=double` |
| button long press (threshold reached) | `btn_long_press` | `BTN_CLICK key=long_press` |
| button released after a long press | `btn_long_release` | `BTN_CLICK key=long_release` (always paired with the preceding `long_press`) |
| side switch moved to on | `switch_on` | `TOGGLE_SWITCH state=on` |
| side switch moved to off | `switch_off` | `TOGGLE_SWITCH state=off` |

Action vocabulary (`ControlAction`): `none`, `record_start`, `record_stop`, `record_toggle`, `record_mark`. A missing key is treated by the firmware as `none`; `CONFIG_GET`/`CONFIG_DEL` on a missing key answer `code 3 "not found"`, which `control_action` maps to `ControlAction::None` and `clear_control_action` treats as success. Any other string is stored and echoed verbatim but performs nothing on v0.3.x; the SDK carries it as `ControlAction::Other`. `record_mark` outside a recording is a no-op on the device. Actions bound this way surface in `RECORD_STATUS` pushes with `reason: "button"` / `"switch"`.

### AP and STA file retrieval

After `WIFI_OPEN`, the ESP32 owns SD-card access: every nRF-side card operation (`STORAGE`, `FILE_LIST`, `FILE_DELETE`, `FILE_CLEAR`, `FILE_UPLOAD`/take, `RECORD_START`, `RECORD_MARK`) answers `sd not mounted` until `WIFI_CLOSE` hands the card back. The SDK pre-checks `STATE_INFO.wifiOpen` for those methods and fails fast with `SdOwnedByWifiModule` (call `close_wifi` first, or use the AP quick-transfer client for files while the module is powered). The check is the mirror image of `WifiModulePoweredDown`: one of the two sides owns the card at any time.

`open_wifi` is idempotent. After a BLE drop the ESP32 usually stays powered (`wifiOpen == true` on reconnect); the SDK then skips `WIFI_OPEN`, returns success and marks the AP ready — the AP auto-started at power-up and no new `WIFI_AP_START` will arrive on this connection.

`WIFI_STATUS.mode` reports `null`, `sta`, `ap`, or `apsta`. The SDK supports both file paths, which must not be confused:

| Mode | Workflow | Source-file behaviour |
| --- | --- | --- |
| AP quick transfer | `open_wifi` → wait for `WIFI_AP_START` (`wait_for_ap_ready`); join the AP `DYJ-HD-AP`; `access_point_client()` resolves the fixed endpoint `http://192.168.4.1:80` without any query; list and stream files over `/api/files`, `/api/files/download?path=…` (the firmware's traversal guard rejects a leading `/` in `path` with 403 — the client strips it, and the server joins the SD root itself). | HTTP download is read-only. HTTP deletion requires an explicit `delete_file`. |
| STA cloud upload | Connect to a router, then call `upload_file_via_sta_to(name, url)` (or configure `upload_url` once and use `upload_file_via_sta`). | ESP32 streams the file via **HTTP POST** to `<url>/<generated-name>` and deletes the local file after a 2xx response. Remote retrieval is governed by the receiving service, not this device protocol. |

AP mode is the firmware default: every `WIFI_OPEN` power-up auto-starts the SoftAP (fixed SSID `DYJ-HD-AP` with a fixed password that is deliberately not shipped in the SDK — the HTTP file API is unauthenticated, so the password is the only access control; take it from the device documentation —, IP `192.168.4.1`, HTTP port 80 — `ap_manager` never calls `softAPConfig` and its IP getter hardcodes the value) plus the file web server, then reports `WIFI_AP_START` as the readiness event. Because the AP endpoint is fixed, `access_point_client()` waits for that event and uses the constants (`AP_DEFAULT_IP`/`AP_DEFAULT_PORT`) instead of querying.

`AP_START` / `AP_STOP` / `AP_STATUS` passthrough is **not** in the flashed v0.3.x nRF firmware — those commands are silently dropped as unknown, so querying them only burns the timeout. SDK methods `start_access_point`/`stop_access_point`/`access_point_status` remain for a future firmware that carries the `AP_` passthrough (already in this tree, `nrf52840dk/src/protocol/app_protocol.c`); with a custom AP configuration they become the way to learn the real endpoint.

Recording names carry the capture time. Each `.apm` has a sibling `.txt` (`{"timestamp":<unix s>,"synced":<bool>}`); the firmware derives `synced_<unix>.apm` (clock was synced) or `nosync_<unix>.apm` (clock was not — the value is not wall-clock time) and surfaces that name both in the `FILE_UPLOAD` start frame (`name`) and in the AP download's `Content-Disposition` filename (`AccessPointDownloadedFile::suggested_name`). `RecordingStamp::parse` decodes either; when the `.txt` is missing the firmware falls back to the raw `XXXXXXXX.APM` name and parsing yields `None`. There is no need to download the `.txt` separately.

### STA upload protocol (WiFi 0.1.0)

`UPLOAD_FILE {"name": "<SD file>", "url": "<base URL>"}` streams the file to the server over **plain HTTP POST** (the 0.0.9 hand-rolled WebSocket upload and its `auth/exchange` token gate are removed):

- The device resolves the target as `url` from the request > `upload_url` config key > error `"上传地址未配置"`; it appends `/` + the generated recording name (`synced_/nosync_<unix>.apm`), so the URL is a per-device base such as `https://dyj2.dev.reai.com/api/upload/<deviceId>`.
- Request: `Content-Type: application/octet-stream`, body = the raw file streamed from SD; headers `x-filename` (generated name), `x-filesize`, `x-format: adpcm`, plus `x-device-id` / `authorization: Bearer <token>` **when** those are set (no more hard requirement — activation is optional).
- The command answers only after the transfer finishes (SDK timeout 600 s); a 2xx deletes the source `.apm` + `.txt` on the card, anything else keeps both and reports `上传失败`.
- `UPLOAD_LIST` / `UPLOAD_DELETE` / `UPLOAD_CLEAR` are unchanged. The automatic upload loop stays disabled in firmware; uploads happen only on `UPLOAD_FILE`.

The reference receiver is `dyj2.dev.reai.com` (`tools/uploadServer/` in the firmware repo): it validates `x-filesize`, stores under `uploads/<deviceId>/`, and answers `{"ok":true,"size":…,"sha256":…}` for host-side verification.

### Custom display content (DISPLAY_SHOW / DISPLAY_CLEAR, main 0.3.7)

The 128×64 OLED is driven over I2C from `display_buffer[8][128]`; only pages 0–4 × columns 0–71 fall inside the visible glass window, i.e. **72×40 px** with the 5×8 ASCII font giving 12 characters × 5 lines. Two new commands drive it from the host:

- `DISPLAY_SHOW {"lines": ["AB", "CD"], "bitmap": "<base64>", "persistent": false, "timeoutMs": 5000}` — text uses the built-in font (printable ASCII only); `bitmap` decodes to exactly **360 bytes** written straight into `display_buffer[0..5][0..72]`. Byte layout mirrors the font rendering path: one byte per column, bit 0 = top row of its 8-pixel page. When both are present the bitmap wins. `persistent: true` keeps the frame until the next system screen or `DISPLAY_CLEAR`; otherwise `timeoutMs` (default 5000, ≤ 3 600 000) clears it via the existing `display_process()` tick. Any text longer than 12 chars per line and CJK/graphics must be rendered into a bitmap by the host (the `dyj_web` canvas panel does this, including Chinese).
- `DISPLAY_CLEAR` — blank the screen and reset the timeout to the 5 s default.

Precedence is deliberately boring: the custom frame is just another transient screen. Charging, recording, BLE and battery screens still override it whenever their events fire, and a non-persistent frame clears itself after `timeoutMs`. Frame budget: the nRF RX buffer and command-queue JSON grew from 256/255 to **1024 bytes** in 0.3.7 for exactly this command — a bitmap request is ≈530 bytes on the wire, so older firmware cannot receive it (the SDK does not gate this by version; a pre-0.3.7 device answers `unknown command`).

### Device KV configuration

`CONFIG_SET/GET/DEL` is one command family backed by two stores. The nRF parses the request first (`handler_sys.c`, buffers `key[32]` / `value[64]`) and then routes it: a key that **starts with `wifi_` or ends with `_wifi`**, or — since main 0.3.7 / WiFi 0.1.0 — is exactly `upload_url`, `api_url` or `ota_path`, is forwarded unchanged over UART to the ESP32 (`Preferences`, prefix `cfg_`), anything else lands in the nRF's own `setting_kv` (16 slots shared with every key, 32-byte keys / 64-byte values). The SDK exposes the same rule as `config_key_routes_to_wifi_module(key)` and applies it in three places:

- **Powered-module pre-check.** Routed keys need the ESP32 powered; otherwise the UART frame is never answered. `set_config`/`get_config`/`config_value`/`delete_config` fail fast with `WifiModulePoweredDown` for routed keys when `STATE_INFO.wifiOpen` is false.
- **"Missing key" normalisation.** The two stores disagree: nRF answers `code 3 "not found"`, the ESP32 answers `{"key":…,"value":""}` (`Setting::getConfig(key, "")`). `config_value` returns `Ok(None)` for both (an empty string on a routed key counts as unset); `get_config` returns the raw response and therefore still errors on the nRF side.
- **Length validation.** Keys longer than `CONFIG_KEY_MAX_BYTES` (31) or values longer than `CONFIG_VALUE_MAX_BYTES` (63) overflow the nRF's `mjson_get_string` buffers and come back as the misleading `code 2 "missing key"` / `code 3 "missing value"`; the SDK rejects them locally with `InvalidCommand`. Empty keys are rejected too.

Error codes: `CONFIG_SET` → `1` missing json, `2` missing key, `3` missing value, `4` persist failed (slot table full or flash write error); `CONFIG_GET`/`CONFIG_DEL` → `1`/`2` as above, `3` not found. ESP32-side failures are always `code 1` with a message — including `CONFIG_DEL` of a key that does not exist (`Preferences::remove` returns false → `"配置删除失败"`), which is indistinguishable from a real failure. `clear_config` therefore reads routed keys first and returns `Ok(false)` instead of deleting; `delete_config` keeps the raw semantics.

Known keys on v0.3.x / WiFi v0.0.9:

| Key | Store | Written by | Meaning |
| --- | --- | --- | --- |
| `btn_single`, `btn_double`, `btn_long_press`, `btn_long_release` | nRF | host (`set_control_action`) | button gesture → `ControlAction` |
| `switch_on`, `switch_off` | nRF | host (`set_control_action`) | side switch position → `ControlAction` |
| `low_power_mode` | nRF | firmware | deep-sleep flag used at boot; surfaced as `STATE_INFO.lowPowerMode`. Read-only for the host. |
| `wifi_ver`, `wifi_ver_code` | nRF | firmware (`WIFI_SYNC`) | cached ESP32 version; surfaced as `BASE_INFO.wifiVersion/wifiVersionCode`. Despite the `wifi_` prefix these are stored on the nRF (the firmware writes them internally, bypassing the routing rule) — reading them from the host **is** routed to the ESP32 and returns `""`. |
| `api_url` | ESP32 | host (`set_sta_api_base_url`) | `http://`/`https://` base URL for auth and OTA info (WiFi 0.1.0; ≤63 bytes like every config value) |
| `upload_url` | ESP32 | host (`set_wifi_upload_url`) | default upload **base** URL for `UPLOAD_FILE` when the request carries no `url` (WiFi 0.1.0) |
| `ota_path` | ESP32 | host (`set_wifi_ota_info_path`) | relative OTA-info route; firmware default `/ota/info?mod=wifi` when unset |

**NVS key-length rule (and the v0.0.9 bug it explains).** ESP32 NVS rejects keys longer than 15 bytes, and `Setting::setConfig` prepends the `cfg_` prefix, so a config key may be at most **11 bytes**. The pre-0.1.0 keys `wifi_api_base_url`, `wifi_ws_base_url`, `wifi_file_upload_path`, `wifi_ota_info_path` (17–21 bytes, 20–25 with prefix) were therefore **never persistable**: `CONFIG_SET` on them always answered `code 1 "配置设置失败"` and every read silently fell back to the firmware defaults. WiFi 0.1.0 renames them to the short forms above (nothing to migrate — the old keys never held data), adds `upload_url`, and turns an over-long key into an explicit failure in `Setting::setConfig`. On main controllers older than 0.3.7 the short keys are *not* in the forwarding set and would be stored locally on the nRF instead — the `upload_url` fallback therefore needs main ≥0.3.7; the per-request `url` in `UPLOAD_FILE` works with any WiFi version that understands it (older firmware ignores the field).

Slot budget on the nRF: 16 total, 3 used by firmware-internal keys, 6 by the control bindings → 7 free for application keys. ESP32 keys do not consume nRF slots.

### WiFi module OTA

The ESP32 updates itself over STA; the nRF is not involved beyond passthrough. Preconditions, all checked by the ESP32 and reported as `code 1` with a message: STA connected to a router (`"WiFi未连接…"`), and an HTTP service reachable at `wifi_api_base_url` + `wifi_ota_info_path` returning `{"version":…, "versionCode":…, "url":…}` (`"OTA检查更新失败"` otherwise). Flow:

1. `check_wifi_ota()` → `OTA_WIFI_CHECK` → `WifiOtaCheck { update_available }`. The result is cached in the ESP32 **for the current power cycle only** (`_updateAvailable` is not persisted).
2. `start_wifi_ota()` → `OTA_WIFI_UPDATE`; refused with `"没有可用的更新"` unless step 1 succeeded with `true` in the same power cycle. The ESP32 answers the command first, then downloads via `HTTPUpdate` and pushes `OTA_WIFI_PROCESS {status, progress, total, percentage}` on start, every further 5 %, and at the end.
3. `update_wifi_firmware(wait)` wraps step 2: subscribes to events *before* sending the command, then returns the first progress whose `status.is_terminal()` (`Success` or `Failed`), or `Timeout { command: "OTA_WIFI_PROCESS" }` when `wait` elapses. Lagged event receivers are skipped, not treated as errors.
4. On `Success` the ESP32 calls `ESP.restart()`. The module stays powered, so `wifiOpen` remains true, but the AP and HTTP server restart — wait for `WIFI_AP_START` (`wait_for_ap_ready`) before using `access_point_client()` again, and expect the version in `BASE_INFO.wifiVersion` to change only after the next `WIFI_SYNC`.

Status vocabulary (`OtaStatus`): `IDLE`, `CHECKING`, `UPDATE_AVAILABLE`, `DOWNLOADING`, `SUCCESS`, `FAILED`; anything else is carried as `Other(String)`. `wifi_ota_status()` reads the same structure on demand.

### AP-mode WiFi OTA (no internet needed)

The ESP32 web server that serves the fast-transfer file API also accepts firmware while the host is joined to the device hotspot:

- `GET /api/ota/check` → `{currentVersion, currentVersionCode, chipModel, freeHeap, otaReady}` (`AccessPointClient::ota_status`). The device has no uplink in AP mode, so the *latest* version must come from `OtaInfo::fetch(..., OtaModule::Wifi)` obtained while the host still had internet; compare `version_code` with `current_version_code` on the host.
- `POST /api/ota/upload`, `multipart/form-data` with the `.bin` in field `firmware` (`AccessPointClient::upload_firmware`). The ESP32 streams it into the inactive OTA partition (`Update.begin(UPDATE_SIZE_UNKNOWN)`), answers `200 {"success":true,"size":N,"restart":true}` and **restarts 3 seconds later** — the hotspot and HTTP server go away with it, `wifiOpen` stays true, and a fresh `WIFI_AP_START` arrives when the new firmware is up. Failure is `500 {"success":false,"error":"OTA update failed"}`, surfaced as `SdkError::Device { command: "POST /api/ota/upload", code: 500 }`; the ESP32 keeps running the old partition. There is no version check on the device: re-flashing the running version succeeds. The SDK allows 10 minutes for the upload (≈1.3 MB over the hotspot).

### Main-controller DFU (Nordic Secure DFU over BLE)

`OTA` (`enter_dfu_mode`) answers `code 0`, sets `GPREGRET = 0xB1` and resets. The nRF52840 comes back in the Secure Bootloader (nRF5 SDK 17.1, `nrf52840dk/bootloader`): advertising name `DFU_DYJ`, Secure DFU service `0xFE59`, no bonding required, `NRF_DFU_APP_DOWNGRADE_PREVENTION = 1`, hardware version 52, SoftDevice S140 7.2.0 (`--sd-req 0x100`), MTU up to 247. The device no longer exposes NUS, so the normal `DyjDevice` cannot talk to it; the SDK implements the Secure DFU client itself (`src/dfu.rs`) because Nordic ships DFU libraries for iOS/Android/RN/Flutter but nothing for Rust/desktop.

Package: `nrfutil pkg generate --application <hex> --application-version N --hw-version 52 --sd-req 0x100 --key-file dfu_private_key.pem app_update_vN.zip` (`nrf52840dk/ota_package.sh`). The zip holds `manifest.json` → `application.{dat_file,bin_file}`; `DfuPackage::from_zip_*` reads exactly that and refuses packages that also carry SoftDevice/bootloader images (two-stage transfer, not implemented). The repository's `dfu_packages/app_update_v3xx.zip` version numbers equal the firmware `versionCode`.

Protocol as driven by `DfuTarget::upload` (all integers little-endian):

| Step | Control Point write | Response `0x60 <op> <result> …` |
| --- | --- | --- |
| Set PRN | `02 <prn u16>` | — |
| Select command object | `06 01` | `max_size u32, offset u32, crc32 u32` |
| Create command object | `01 01 <len u32>` | — |
| init packet bytes | Packet characteristic, write-without-response, ≤ `packet_size` each | every `prn` packets: `60 03 01 offset crc` |
| Calculate CRC / Execute | `03` / `04` | CRC → `offset crc`; Execute validates signature, hw/sd/fw version → extended errors |
| Select data object | `06 02` | `max_size` (4096) + what the bootloader already holds |
| per object | `01 02 <size>` → packets → `03` → `04` | repeated until the image is complete |

Behaviour worth knowing: the CRC the bootloader reports is cumulative over the whole image, so the client keeps a running CRC32 (`crc32fast`) and compares after every PRN batch and before every Execute. When they disagree the client first checks whether the bootloader simply holds a *shorter but correct* prefix — `crc(bootloader) == crc32(image[..offset])` — and if so **rewinds** to that offset and keeps writing the same object (`DfuReport::rewinds`); only a genuinely different prefix makes it re-create the object (`object_retries`, default 3, counted in `retries`). `Select` also enables **resume**: if the bootloader already holds a prefix of the same image, the client continues inside the partially written object (or re-executes a completed-but-unexecuted one; the bootloader answers `operation not permitted` for a second Execute, which the SDK tolerates). A prefix that does not match the package restarts the current object; a mismatch inside already-executed data cannot be repaired — power-cycle the device. After the last Execute the bootloader checks the whole image and resets into the application; the link drops without a further response. Result codes map to `SdkError::Dfu { operation, result, extended }`; extended `0x05` is downgrade prevention (package `--application-version` below the installed one — equal versions pass), `0x06` hw-version, `0x07` sd-req, `0x0C` signature does not match the bootloader's public key.

**Flow control — measured on the device (2026-10-20).** The bootloader has `NRF_DFU_BLE_BUFFERS = 8` receive buffers (`nrf_balloc`, one per incoming Packet write, released when its flash write completes) and `NRF_FSTORAGE_SD_MAX_WRITE_SIZE = 20`. macOS flushes queued write-without-response packets into a single connection event, so a burst of 10 packets overflowed the pool: the PRN notification arrived with `offset = 300` after 20 packets had been sent — the bootloader keeps the correct first 300 bytes (`crc32(image[..300])` matched) and silently dropped the other 5; its PRN counter counts *arriving* packets, including the dropped ones, so the receipt still came after the 10th. Re-creating the object immediately afterwards failed with `0x05 invalid object` because `Create` erases the target page while the previous writes were still queued in `nrf_fstorage`. The SDK therefore (a) defaults to `prn = 4` (≤ 4 packets in flight, half the pool), (b) rewinds instead of re-creating when the prefix is intact, (c) re-sends `Set PRN` after every `Create` and before any mid-object continuation so the bootloader's arrival counter lines up with the client's batches, and (d) treats `invalid object` on `Create` as "flash busy", waiting 250 ms and retrying up to 8 times. With these defaults a batch is 80 bytes per round trip (≈2.7–5 KB/s, 35–65 s for a 170 KB application).

Transport defaults (`DfuOptions`): `packet_size = 20` (safe under the default 23-byte ATT MTU; btleplug does not expose the negotiated MTU, raise it only if you know the link allows up to 244 — a truncated write shows up as a CRC mismatch and is retried, so the failure mode is slow, not silent), `prn = 4` (see above; raise only when `DfuReport::rewinds` stays 0), `response_timeout = 10 s` (init-packet Execute includes an ECDSA verify, ≈1 s).

Known platform caveat: entering the bootloader via `GPREGRET` keeps the BLE address, so CoreBluetooth may serve the cached NUS GATT table on the first connection and no DFU characteristics are found. `connect_dfu` disconnects, waits one second and reconnects once; `scan_dfu_targets` filters on the advertised `0xFE59` service (advertising data is never cached). `DeviceManager::update_main_firmware` strings everything together: `OTA` → wait up to 20 s for `DFU_DYJ` → connect → upload. If it fails midway the device stays in DFU mode and keeps advertising; `scan_dfu_targets` + `connect_dfu` + `upload` resumes without sending `OTA` again.

### OTA info API

The product OTA service (`https://dyj2.dev.reai.com`, static nginx, live since 2026-10) publishes `GET {api_base}/ota/info?mod=main|wifi` (no auth) → `{"version":"…","versionCode":N,"url":"https://…"}`; a missing/invalid `mod` answers 400. `api_base` is exactly the device KV value `wifi_api_base_url` — every WiFi firmware build ships with `OTA_DEFAULT_API_BASE_URL = "https://dyj2.dev.reai.com/api"` and `OTA_DEFAULT_INFO_PATH = "/ota/info?mod=wifi"`, so old devices find the service without any configuration. `OtaInfo::fetch_with_path(base, path)` / `fetch(base, module)` mirror the firmware's URL building, `download` fetches `url`, and `is_newer_than(installed_version_code)` compares **only** `versionCode` (the `url` file name may lag behind `version`). For the main controller the comparison is the host's job (`BASE_INFO.versionCode`); for the WiFi module the ESP32 compares on its own during `OTA_WIFI_CHECK`, so hosts call `OtaInfo` only for display or the AP-mode comparison.

### WiFi online OTA compatibility (old firmware → 0.0.9)

Verified against the repository history (commit-level):

| Concern | Since | Note |
| --- | --- | --- |
| `OTA_WIFI_CHECK/UPDATE/STATUS` + `OTA_WIFI_PROCESS` | WiFi 0.0.1 | Response formats are byte-identical through 0.0.9; `updateAvailable = server versionCode > local` (strict). `startUpdate` requires a successful CHECK in the same power cycle. |
| `wifi_api_base_url` runtime override | WiFi 0.0.1 (key `wifi_api_base_url` since 0.0.3-era) | Arbitrary `CONFIG_SET` keys are stored; defaults always pointed at `dyj2.dev.reai.com/api` — the old `hdd.dev.reai.com` domain only ever existed in docs and the legacy Android app. |
| nRF passthrough of `OTA_WIFI_*` and `wifi_*` config | main v0.1.5 (2025-08) | Every shipped main firmware qualifies. |
| AP-mode local upload (`/api/ota/upload`) | WiFi 0.0.4 (2026-03) | Older devices can only be upgraded online — which is the point of the dedicated flow. |
| `BASE_INFO.wifiVersion/wifiVersionCode` | main v0.3.5 | Older mains omit it; treat "unknown module version" as normal and let `OTA_WIFI_CHECK` decide. |
| Partition table | — | Not recorded for field devices. The 0.0.9 app-only bin is 1,297,616 bytes; a default 1.25 MB slot leaves ~13 KB, a single-slot (huge_app) table cannot OTA at all → `FAILED`, device keeps running; USB re-flash recovers. |

`DyjDevice::check_and_update_wifi_firmware(wait)` wraps CHECK → UPDATE for this flow and returns `WifiOnlineOtaOutcome::{UpToDate, Finished(OtaProgress)}`; progress arrives as `OTA_WIFI_PROCESS` events (every 5 %). `OTA_WIFI_CHECK` gets a 30 s timeout: the ESP32 performs the HTTPS GET synchronously.

Field observation (2026-10-20, main v0.3.3 + unknown WiFi firmware): after `WIFI_CONNECT` succeeded, `WIFI_STATUS` stopped answering (10 s timeout, repeatedly) while `OTA_WIFI_CHECK` answered normally and reached the server. Treat a missing `WIFI_STATUS` after STA connect as a firmware quirk, not as "not connected": the SDK's `WifiStatus` fields are `serde(default)`, and host flows should fall through to `OTA_WIFI_CHECK`, whose own `"WiFi未连接"` error is the authoritative connectivity check.

## Events

The event receiver emits typed `DeviceEvent`s for `BTN_CLICK` (`ButtonEvent { gesture, action }`), `TOGGLE_SWITCH` (`ToggleSwitchEvent { position, action }`), `RECORD_STATUS` (`RecordStatus` with `reason` and, on failure, `error`), `WIFI_LIST`, `WIFI_OFF`, `UPLOAD_COMPLETE`, `OTA_WIFI_PROCESS` and `WIFI_AP_START`. `WIFI_LIST` may also be consumed by a currently running `scan_wifi` call; event observers still receive it. A `BTN_CLICK`/`TOGGLE_SWITCH` whose `key`/`state` the SDK does not know (a future firmware gesture) is delivered as `DeviceEvent::Unhandled { command, payload }` rather than dropped.

`RECORD_STATUS` pushes carry `reason` (`app`, `button`, `switch`, `auto` for the 10-minute auto-split, `unknown`) and, for `RECORD_EVT_ERROR`, an `error` code with `isRecording: false`. The same command name answers the `record_status()` query; that response has no `reason` and is also broadcast on the event channel.

### Power notifications: format fixed in 0.3.7, charging event added

The protocol defines one wire format for everything the device sends, including unsolicited events: `NAME:json\r\n`. Up to v0.3.6 the power manager did **not** follow it — `power_mgmt_event_handler` in `main.c` wrote three messages straight to `ble_uart_send` without a command name and without the `\r\n` terminator (`{"type":"shutdown_imminent","seconds":5}`, `{"type":"device_wakeup"}`, `{"type":"startup_complete"}`). A line-oriented parser glues the first one to whatever frame follows and loses both — in practice, any command answered during the 5-second shutdown countdown would time out.

**Fixed in main 0.3.7**: the three notices now go through `app_protocol_send_event("POWER", json)` and arrive as ordinary `POWER:{...}\r\n` frames. **0.3.7 also adds a charging notification**: the charging-state edge (plug in / unplug, sampled every 5 s) pushes `POWER:{"type":"charging","charging":<bool>,"level":<pct>}`.

The SDK accepts both shapes and surfaces them as the same `DeviceEvent::Power(PowerEvent)`:

| Shape | Who sends it | How the SDK handles it |
| --- | --- | --- |
| Raw `{"type":...}` with no name and no CRLF | firmware ≤ v0.3.6 | `FrameDecoder` treats a buffer starting with `{` as a raw JSON object, cuts it at the JSON boundary as soon as it is complete (no newline needed), and resumes line parsing on the remaining bytes. Unknown `type` → `Unhandled` with an empty `command`; malformed raw JSON is dropped up to the next newline so the stream resynchronizes. |
| Framed `POWER:{"type":...}\r\n` | firmware ≥ 0.3.7 | ordinary text frame; `event_from_frame` maps command `POWER` to `PowerEvent`, unknown `type` → `Unhandled { command: "POWER" }`. |

`PowerEvent` variants: `ShutdownImminent { seconds }`, `DeviceWakeup`, `StartupComplete`, `Charging { charging: bool, level: Option<u8> }` (the latter only from ≥0.3.7).

Event availability caveat: with the current ESP32 firmware the `UPLOAD_COMPLETE` notification is only sent from the automatic-upload loop, which is disabled (`UploadManager::loop` returns immediately). The `UPLOAD_FILE` command therefore reports completion through its own (delayed) response, not through `UPLOAD_COMPLETE`; treat `DeviceEvent::UploadComplete` as reserved for future firmware. `WIFI_AP_START` is emitted by the ESP32 boot flow once its AP and HTTP server are both up.

## Realtime audio

Recording is PDM 16 kHz / 16-bit / mono, IMA-ADPCM encoded (4-bit, 4:1, low nibble first) with the decoder state continuous across the whole recording — the firmware resets it only at `RECORD_START`. The ADPCM stream always goes to the SD-card `.apm` file; when `recordRealtimeEnabled` is set (persisted; `RECORD_REALTIME_ENABLE {"enabled":…}`, surfaced in `STATE_INFO`), it is additionally copied into a 2 KB ring buffer and emitted as `0x00`-prefixed binary BLE notifications: at most 200 bytes every 20 ms (≈8 kB/s audio, 10 kB/s wire ceiling).

SDK handling:

- `start_realtime_recording()` enables realtime transfer, starts the recording and returns a `RealtimeRecordingSession`; `session.stop()` (or dropping the session) stops the recording and disables the switch.
- The notification task decodes the stream once (state continuity lives there) and broadcasts `PcmChunk`s (16 kHz mono `i16`) to `subscribe_audio()` subscribers. Backlogged subscribers get `RecvError::Lagged`, which marks a stream discontinuity.
- The notification task resets its ADPCM decoder at each recording start, mirroring the firmware encoder reset: on a successful `RECORD_START` reply or a `RECORD_STATUS` push with `reason` and `isRecording: true`, unless audio of the current segment is already flowing (a duplicate start signal). A `RECORD_STATUS` query reply (no `reason`) only resets when no audio has been decoded since the last stop, so connecting mid-recording or polling during a recording never inserts an audible break. `RECORD_STOP` replies and `isRecording: false` frames mark the stop; the ambiguous `RECORD` toggle reply relies on the following push.
- Recording can also be triggered on the device (button/switch) with realtime enabled persisted — stray binary frames then arrive without any SDK session. They are decoded and broadcast as audio, never treated as protocol faults.
- `take_file_to` refuses while an SDK-initiated realtime session is active (`SdkError::RealtimeRecordingActive`): binary frames carry no header, so audio and file data cannot be told apart on the wire, and mixing them would corrupt the taken file. During a file transfer the notification task suppresses audio broadcast so file bytes are not misrouted. A recording that the device itself started while a file is being taken remains an inherent firmware limitation — avoid taking files while recording.
- `reai_card_sdk::AdpcmDecoder` decodes offline `.apm` files with the same stateful algorithm; `wav_bytes()` wraps PCM into a playable WAV.

Host-side throughput note: NUS runs 247-byte ATT MTU (244-byte notifications, TX queue 8×244, one notification attempted per main-loop pass). At typical connection intervals this sustains the 8 kB/s stream; the firmware drops the oldest bytes when its 2 KB buffer overflows (logged on device), which surfaces as a brief decoding glitch, not a stream failure.

## File transfer is a move

`FILE_UPLOAD` starts with `{"state":"start","name":"...","size":N}`, sends tagged binary packets, then sends `{"state":"end"}`. The current firmware deletes the source file after its transfer loop ends successfully; for an uppercase `.APM` it can also delete a matching `.TXT`. The SDK therefore exposes only `take_file_to` and verifies exactly `N` received bytes. A partial destination must be treated as invalid by the caller.

## Deliberately unavailable in the Rust SDK

| Item | Reason |
| --- | --- |
| `TEST_UPLOAD_EDIT` | ESP32 has a handler, but the nRF BLE parser does not pass this `TEST_` command through. |
| SoftDevice / bootloader DFU packages | `DfuPackage` accepts application-only packages; two-stage SoftDevice+bootloader updates are refused. Application updates are fully supported (see "Main-controller DFU"). |
| Web Bluetooth UI | An independent manual test page kept with the firmware; not part of this SDK. |
