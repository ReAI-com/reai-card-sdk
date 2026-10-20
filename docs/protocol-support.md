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
| AP fast transfer | `AccessPointClient::{device_info,list_files,download_file_to,delete_file}` | ESP32 HTTP `GET /api/files`, `GET /api/files/download`, `DELETE /api/files/delete` |
| STA upload/auth | `list_uploads`, `upload_file_via_sta`, `delete_upload`, `clear_uploads`, `activate`, `clear_activation` | `UPLOAD_*`, `AUTH_*` |
| WiFi OTA | `check_wifi_ota`, `start_wifi_ota`, `wifi_ota_status` | `OTA_WIFI_*` |
| Device config | `set_config`, `get_config`, `config_value`, `delete_config`, `set_led(LedState)`, `test_modules` | `CONFIG_*`, `LED`, `TEST_CHECK` |
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
| STA cloud upload | Connect to a router, configure endpoint values if needed, then call `upload_file_via_sta`. | ESP32 upload manager deletes the local file after a successful server upload. Remote retrieval is governed by the configured service API, not this device protocol. |

AP mode is the firmware default: every `WIFI_OPEN` power-up auto-starts the SoftAP (fixed SSID `DYJ-HD-AP` with a fixed password that is deliberately not shipped in the SDK — the HTTP file API is unauthenticated, so the password is the only access control; take it from the device documentation —, IP `192.168.4.1`, HTTP port 80 — `ap_manager` never calls `softAPConfig` and its IP getter hardcodes the value) plus the file web server, then reports `WIFI_AP_START` as the readiness event. Because the AP endpoint is fixed, `access_point_client()` waits for that event and uses the constants (`AP_DEFAULT_IP`/`AP_DEFAULT_PORT`) instead of querying.

`AP_START` / `AP_STOP` / `AP_STATUS` passthrough is **not** in the flashed v0.3.x nRF firmware — those commands are silently dropped as unknown, so querying them only burns the timeout. SDK methods `start_access_point`/`stop_access_point`/`access_point_status` remain for a future firmware that carries the `AP_` passthrough (already in this tree, `nrf52840dk/src/protocol/app_protocol.c`); with a custom AP configuration they become the way to learn the real endpoint.

Recording names carry the capture time. Each `.apm` has a sibling `.txt` (`{"timestamp":<unix s>,"synced":<bool>}`); the firmware derives `synced_<unix>.apm` (clock was synced) or `nosync_<unix>.apm` (clock was not — the value is not wall-clock time) and surfaces that name both in the `FILE_UPLOAD` start frame (`name`) and in the AP download's `Content-Disposition` filename (`AccessPointDownloadedFile::suggested_name`). `RecordingStamp::parse` decodes either; when the `.txt` is missing the firmware falls back to the raw `XXXXXXXX.APM` name and parsing yields `None`. There is no need to download the `.txt` separately.

### STA upload endpoint configuration

These persistent ESP32 settings are routed by the nRF because their keys start with `wifi_`:

| Rust method | ESP32 key | Expected value |
| --- | --- | --- |
| `set_sta_api_base_url` | `wifi_api_base_url` | `http://` or `https://` API base URL |
| `set_sta_websocket_base_url` | `wifi_ws_base_url` | `ws://` or `wss://` WebSocket base URL |
| `set_sta_upload_path` | `wifi_file_upload_path` | Relative route beginning with `/`, e.g. `/record` |

## Events

The event receiver emits typed `DeviceEvent`s for `BTN_CLICK` (`ButtonEvent { gesture, action }`), `TOGGLE_SWITCH` (`ToggleSwitchEvent { position, action }`), `RECORD_STATUS` (`RecordStatus` with `reason` and, on failure, `error`), `WIFI_LIST`, `WIFI_OFF`, `UPLOAD_COMPLETE`, `OTA_WIFI_PROCESS` and `WIFI_AP_START`. `WIFI_LIST` may also be consumed by a currently running `scan_wifi` call; event observers still receive it. A `BTN_CLICK`/`TOGGLE_SWITCH` whose `key`/`state` the SDK does not know (a future firmware gesture) is delivered as `DeviceEvent::Unhandled { command, payload }` rather than dropped.

`RECORD_STATUS` pushes carry `reason` (`app`, `button`, `switch`, `auto` for the 10-minute auto-split, `unknown`) and, for `RECORD_EVT_ERROR`, an `error` code with `isRecording: false`. The same command name answers the `record_status()` query; that response has no `reason` and is also broadcast on the event channel.

### Protocol deviation: unframed power notifications

The protocol defines one wire format for everything the device sends, including unsolicited events: `NAME:json\r\n` (`BTN_CLICK:{...}\r\n`, `RECORD_STATUS:{...}\r\n`, …), produced by `app_protocol.c respond()`. The power manager does **not** follow it. `power_mgmt_event_handler` in `main.c` writes three messages straight to `ble_uart_send`, **without** a command name and **without** the `\r\n` terminator: `{"type":"shutdown_imminent","seconds":5}` (right after the `SHUTDOWN` response, and before a low-battery/timer shutdown), `{"type":"device_wakeup"}` and `{"type":"startup_complete"}` (normally emitted before any BLE link exists, so rarely observed). A line-oriented parser glues the first one to whatever frame follows and loses both — in practice, any command answered during the 5-second shutdown countdown would time out.

The SDK accepts both shapes and surfaces them as the same `DeviceEvent::Power(PowerEvent)`:

| Shape | Who sends it | How the SDK handles it |
| --- | --- | --- |
| Raw `{"type":...}` with no name and no CRLF | v0.3.x firmware (current) | `FrameDecoder` treats a buffer starting with `{` as a raw JSON object, cuts it at the JSON boundary as soon as it is complete (no newline needed), and resumes line parsing on the remaining bytes. Unknown `type` → `Unhandled` with an empty `command`; malformed raw JSON is dropped up to the next newline so the stream resynchronizes. |
| Framed `POWER:{"type":...}\r\n` | a future firmware that fixes the deviation | ordinary text frame; `event_from_frame` maps command `POWER` to `PowerEvent`, unknown `type` → `Unhandled { command: "POWER" }`. |

Firmware-side fix (recorded, **not applied** — the v0.3.x firmware is frozen): replace the three `ble_uart_send(...)` calls in `power_mgmt_event_handler` with `app_protocol_send_event("POWER", json)` so the notices go through `respond()` and get the standard name + CRLF framing. Both SDK paths stay valid before and after that change.

Event availability caveat: with the current ESP32 firmware the `UPLOAD_COMPLETE` notification is only sent from the automatic-upload loop, which is disabled (`UploadManager::loop` returns immediately). The `UPLOAD_FILE` command therefore reports completion through its own (delayed) response, not through `UPLOAD_COMPLETE`; treat `DeviceEvent::UploadComplete` as reserved for future firmware. `WIFI_AP_START` is emitted by the ESP32 boot flow once its AP and HTTP server are both up.

## Realtime audio

Recording is PDM 16 kHz / 16-bit / mono, IMA-ADPCM encoded (4-bit, 4:1, low nibble first) with the decoder state continuous across the whole recording — the firmware resets it only at `RECORD_START`. The ADPCM stream always goes to the SD-card `.apm` file; when `recordRealtimeEnabled` is set (persisted; `RECORD_REALTIME_ENABLE {"enabled":…}`, surfaced in `STATE_INFO`), it is additionally copied into a 2 KB ring buffer and emitted as `0x00`-prefixed binary BLE notifications: at most 200 bytes every 20 ms (≈8 kB/s audio, 10 kB/s wire ceiling).

SDK handling:

- `start_realtime_recording()` enables realtime transfer, starts the recording and returns a `RealtimeRecordingSession`; `session.stop()` (or dropping the session) stops the recording and disables the switch.
- The notification task decodes the stream once (state continuity lives there) and broadcasts `PcmChunk`s (16 kHz mono `i16`) to `subscribe_audio()` subscribers. Backlogged subscribers get `RecvError::Lagged`, which marks a stream discontinuity.
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
| Complete Nordic DFU transfer | `OTA` only switches firmware into DFU mode; package discovery and transfer use a separate Nordic DFU protocol and are not implemented. |
| Web Bluetooth UI | An independent manual test page kept with the firmware; not part of this SDK. |
