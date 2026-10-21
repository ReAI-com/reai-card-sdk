//! DYJ 设备手动测试 CLI：扫描、连接并驱动 SDK 的全部公开 API。
//!
//! 用法（`--id` 可选，缺省取扫描到的第一台 DYJ 设备）：
//!
//! ```text
//! cargo run --example dyj_cli -- scan
//! cargo run --example dyj_cli -- check
//! cargo run --example dyj_cli -- info
//! cargo run --example dyj_cli -- take RECORD_001.APM out.apm
//! cargo run --example dyj_cli -- wifi open
//! cargo run --example dyj_cli -- raw 'LED' '{"state":"on"}'
//! ```
//!
//! 连接期间会起一个事件打印任务，按键/开关/录音状态等主动上报会实时打到 stdout。

use std::time::Duration;

use reai_card_sdk::{
    ControlAction, ControlBindings, ControlInput, DeviceEvent, DeviceManager, DfuOptions,
    DfuPackage, DfuProgress, DfuStage, DisplayShowRequest, DyjDevice, LedState, OtaInfo, OtaModule,
    SdkError,
};
use serde_json::Value;

#[path = "common/mod.rs"]
mod common;

const DEFAULT_SCAN_SECS: u64 = 5;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (global, command) = split_globals(&args);
    common::init_tracing(global.verbosity);
    let result = run(&global, &command).await;
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

struct Globals {
    id: Option<String>,
    scan_secs: u64,
    verbosity: u8,
}

/// 把 `--id x` / `--seconds n` / `--verbose` / `--trace` 从子命令参数里摘出来。
fn split_globals(args: &[String]) -> (Globals, Vec<String>) {
    let mut globals = Globals {
        id: None,
        scan_secs: DEFAULT_SCAN_SECS,
        verbosity: 0,
    };
    let mut command = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--id" => globals.id = iter.next().cloned(),
            "--seconds" => {
                globals.scan_secs = iter
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(DEFAULT_SCAN_SECS);
            }
            "--verbose" => globals.verbosity = globals.verbosity.max(1),
            "--trace" => globals.verbosity = 2,
            _ => command.push(arg.clone()),
        }
    }
    (globals, command)
}

fn usage() {
    println!(
        "commands:
  scan                              列出广播中的 DYJ 设备
  check                             只读全流程验收（scan→connect→info→state→battery→storage→modules→files）
  info | state | battery | storage  查询类
  record start|stop|toggle|status|mark | realtime on|off
  realtime <seconds> [output.wav]   开始实时录音并收流 N 秒（Ctrl-C 中止），可选保存 16k 单声道 WAV
  decode <in.apm> <out.wav>         离线把录音 .apm（IMA ADPCM）转成 16k 单声道 WAV，不需要设备
  files [folder]                    FILE_LIST
  take <name> <output>              FILE_UPLOAD（取走语义：设备会删源文件，先写 .part 再改名）
  delete <filename> | clear-files   删除类（clear-files 会删根目录全部文件）
  wifi open|close|status|scan|disconnect | wifi connect <ssid> <password>
  ap start|stop|status              ESP32 热点与快传入口
  ap ota-check                      热点模式读 WiFi 模块当前固件版本（需已连热点）
  ap ota-upload <fw.bin>            热点模式本地升级 WiFi 模块（multipart 上传，成功后 ESP32 3s 自重启）
  uploads list|upload <name> [url]|upload-all [url]|delete <name>|clear
  display text <行1>|<行2> ...      OLED 显示 ASCII 文本（0.3.7+，每行≤12字符、最多5行）
  display bitmap <360字节.bin>      OLED 显示 72×40 点阵（纵向字节，bit0=页顶）
  display clear                     清屏
  config get <key> | config set <key> <value> | config del <key>
  controls list                      读六个按键/开关输入的绑定表
  controls set <input> <action>      input: btn_single|btn_double|btn_long_press|btn_long_release|switch_on|switch_off
                                     action: none|record_start|record_stop|record_toggle|record_mark
  controls clear <input> | controls preset recorder | controls reset
  led on|off|toggle
  ota check|start|status            ESP32 在线 OTA（需 STA 已连 + api_url 服务）
  ota update [secs]                 check→update→跟进 OTA_WIFI_PROCESS 到 SUCCESS/FAILED（默认上限 600s）
  ota info <api_base_url> main|wifi 查服务器最新版本（不需要设备）
  dfu                               仅让主控进 DFU 模式（之后广播 DFU_DYJ）
  dfu <package.zip> [packet_size] [prn]   主控完整升级：OTA 进 DFU → 扫 DFU_DYJ → Secure DFU 推包（默认 20 字节包、每 4 包回执）
  dfu-scan                          扫描已在 DFU 模式的主控（不需要先连设备）
  dfu-resume <package.zip> [packet_size] [prn]  对已在 DFU 模式的主控直接推包/续传
  auth activate <deviceId> <name> <code> | auth clear
  restart | shutdown | dfu
  events                             挂机只打印主动上报事件（Ctrl-C 退出）
  raw <CMD> [json]                  发送任意协议命令

global options: --id <device-id>  --seconds <scan-secs>  --verbose(调试日志)  --trace(字节级)"
    );
}

async fn run(globals: &Globals, command: &[String]) -> reai_card_sdk::Result<()> {
    let Some(first) = command.first() else {
        usage();
        return Ok(());
    };
    // 离线命令：不需要蓝牙适配器
    if first == "decode" {
        return decode(&command[1..]).await;
    }

    // 不需要设备的在线命令
    if first == "ota" && command.get(1).map(String::as_str) == Some("info") {
        let base = required(&command[2..], 0, "api base url")?;
        let module = match required(&command[2..], 1, "main|wifi")? {
            "main" => OtaModule::Main,
            "wifi" => OtaModule::Wifi,
            other => return Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
        };
        return print_json(serde_json::to_value(OtaInfo::fetch(base, module).await?)?);
    }

    let manager = DeviceManager::new().await?;
    if first == "scan" {
        return scan(&manager, globals.scan_secs).await;
    }
    // bootloader 不提供 NUS，这两条直接找 DFU_DYJ，不走普通连接
    if first == "dfu-scan" {
        let targets = manager
            .scan_dfu_targets(Duration::from_secs(globals.scan_secs))
            .await?;
        println!("{} DFU target(s)", targets.len());
        for target in &targets {
            println!("{}  name={} rssi={:?}", target.id, target.name, target.rssi);
        }
        return Ok(());
    }
    if first == "dfu-resume" {
        let package = DfuPackage::from_zip_path(required(&command[1..], 0, "package.zip")?)?;
        let options = dfu_options(command.get(2), command.get(3))?;
        let target = manager
            .scan_dfu_targets(Duration::from_secs(globals.scan_secs))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| reai_card_sdk::SdkError::DeviceNotFound("DFU_DYJ".into()))?;
        println!("connecting DFU target {} ({}) ...", target.name, target.id);
        let dfu = manager.connect_dfu(&target).await?;
        let report = dfu
            .upload(&package, &options, dfu_progress_printer())
            .await?;
        let _ = dfu.disconnect().await;
        return print_dfu_report(&report);
    }

    let device = connect(&manager, globals).await?;
    let rest = &command[1..];
    match first.as_str() {
        "check" => check(&device, rest).await,
        "info" => print_json(serde_json::to_value(device.device_info().await?)?),
        "state" => print_json(serde_json::to_value(device.state().await?)?),
        "battery" => print_json(serde_json::to_value(device.battery().await?)?),
        "storage" => print_json(serde_json::to_value(device.storage().await?)?),
        "record" => record(&device, rest).await,
        "realtime" => realtime_stream(&device, rest).await,
        "files" => {
            let files = device
                .list_files(rest.first().map(|value| value.as_str()))
                .await?;
            println!("{} entries", files.len());
            print_json(serde_json::to_value(files)?)?;
            Ok(())
        }
        "take" => take(&device, rest).await,
        "delete" => {
            let name = required(rest, 0, "filename")?;
            print_json(serde_json::to_value(device.delete_file(name).await?)?)
        }
        "clear-files" => {
            let files = device.clear_files().await?;
            println!("cleared {} entries", files.len());
            Ok(())
        }
        "wifi" => wifi(&device, rest).await,
        "ap" => ap(&device, rest).await,
        "uploads" => uploads(&device, rest).await,
        "config" => config(&device, rest).await,
        "controls" => controls(&device, rest).await,
        "led" => {
            let state: LedState = required(rest, 0, "state (on/off/toggle)")?.parse()?;
            print_json(serde_json::to_value(device.set_led(state).await?)?)
        }
        "display" => match rest.first().map(String::as_str) {
            Some("clear") => print_json(serde_json::to_value(device.display_clear().await?)?),
            Some("text") => {
                let lines: Vec<String> = rest[1..]
                    .iter()
                    .flat_map(|line| line.split('|'))
                    .map(str::to_owned)
                    .collect();
                print_json(serde_json::to_value(
                    device
                        .display_show(&DisplayShowRequest::text(lines))
                        .await?,
                )?)
            }
            Some("bitmap") => {
                let bytes = tokio::fs::read(&rest[1]).await?;
                print_json(serde_json::to_value(
                    device
                        .display_show(&DisplayShowRequest::bitmap(bytes))
                        .await?,
                )?)
            }
            _ => Err(SdkError::InvalidCommand(
                "用法: display text <行1>|<行2> | display bitmap <360字节.bin> | display clear"
                    .into(),
            )),
        },
        "ota" => ota(&device, rest).await,
        "auth" => auth(&device, rest).await,
        "restart" => print_json(serde_json::to_value(device.restart().await?)?),
        "shutdown" => print_json(serde_json::to_value(device.shutdown().await?)?),
        "dfu" => match rest.first() {
            None => print_json(serde_json::to_value(device.enter_dfu_mode().await?)?),
            Some(path) => {
                let package = DfuPackage::from_zip_path(path)?;
                let options = dfu_options(rest.get(1), rest.get(2))?;
                println!(
                    "updating main firmware: init {} B, image {} B, packet {} B, prn {}",
                    package.init_packet.len(),
                    package.firmware.len(),
                    options.packet_size,
                    options.prn
                );
                let report = manager
                    .update_main_firmware(&device, &package, &options, dfu_progress_printer())
                    .await?;
                print_dfu_report(&report)
            }
        },
        "events" => {
            println!("listening for device events, Ctrl-C to exit");
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        "raw" => {
            let name = required(rest, 0, "command")?;
            let payload = rest.get(1).map(|json| {
                serde_json::from_str::<Value>(json).expect("payload must be valid JSON")
            });
            print_json(device.raw_command(name, payload).await?)
        }
        other => {
            println!("unknown command `{other}`");
            usage();
            Ok(())
        }
    }
}

async fn scan(manager: &DeviceManager, secs: u64) -> reai_card_sdk::Result<()> {
    let devices = manager.scan(Duration::from_secs(secs)).await?;
    if devices.is_empty() {
        println!("no DYJ device found in {secs}s");
        return Ok(());
    }
    for device in &devices {
        println!(
            "{}  name={} rssi={:?}",
            device.id,
            device.name,
            device
                .rssi
                .map(|rssi| rssi.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

async fn connect(manager: &DeviceManager, globals: &Globals) -> reai_card_sdk::Result<DyjDevice> {
    let devices = manager.scan(Duration::from_secs(globals.scan_secs)).await?;
    let found = match &globals.id {
        Some(id) => devices.iter().find(|device| &device.id == id),
        None => devices.first(),
    }
    .ok_or_else(|| {
        reai_card_sdk::SdkError::DeviceNotFound(globals.id.clone().unwrap_or_default())
    })?;
    println!("connecting {} ({}) ...", found.name, found.id);
    let device = manager.connect(found).await?;
    spawn_event_printer(&device);
    Ok(device)
}

fn spawn_event_printer(device: &DyjDevice) {
    let mut events = device.subscribe_events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => println!("event: {}", summarize(event)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    eprintln!("event printer lagged by {count}");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

fn summarize(event: DeviceEvent) -> String {
    fn json(value: impl serde::Serialize) -> String {
        serde_json::to_string(&value).unwrap_or_default()
    }
    match event {
        DeviceEvent::Button(event) => format!("button {} → {}", event.gesture, event.action),
        DeviceEvent::ToggleSwitch(event) => {
            format!("switch {} → {}", event.position, event.action)
        }
        DeviceEvent::RecordStatus(status) => json(status),
        DeviceEvent::WifiNetwork(network) => json(network),
        DeviceEvent::WifiOff(response) => json(response),
        DeviceEvent::UploadComplete(response) => json(response),
        DeviceEvent::WifiOtaProgress(progress) => json(progress),
        DeviceEvent::AccessPointReady(response) => json(response),
        DeviceEvent::Power(power) => format!("power {}", json(power)),
        DeviceEvent::Unhandled { command, payload } if command.is_empty() => {
            format!("raw:{payload}")
        }
        DeviceEvent::Unhandled { command, payload } => format!("{command}:{payload}"),
    }
}

/// 只读验收：连接后依次跑一遍查询类 API，适合拿到新固件后先跑这个。
async fn check(device: &DyjDevice, _rest: &[String]) -> reai_card_sdk::Result<()> {
    println!("== device_info ==");
    print_json(serde_json::to_value(device.device_info().await?)?)?;
    println!("== state ==");
    print_json(serde_json::to_value(device.state().await?)?)?;
    println!("== battery ==");
    print_json(serde_json::to_value(device.battery().await?)?)?;
    println!("== storage ==");
    print_json(serde_json::to_value(device.storage().await?)?)?;
    println!("== record_status ==");
    print_json(serde_json::to_value(device.record_status().await?)?)?;
    println!("== test_modules ==");
    for module in device.test_modules().await? {
        println!(
            "  {:<18} {}",
            module.key,
            if module.result { "OK" } else { "FAIL" }
        );
    }
    println!("== files ==");
    for file in device.list_files(None).await? {
        println!(
            "  {:<28} size={:<10} dir={}",
            file.name.as_deref().unwrap_or("-"),
            file.size
                .map(|size| size.to_string())
                .unwrap_or_else(|| "-".into()),
            file.is_dir.unwrap_or(0) == 1
        );
    }
    println!("check finished");
    Ok(())
}

async fn record(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "record subcommand")? {
        "start" => print_json(serde_json::to_value(device.start_recording().await?)?),
        "stop" => print_json(serde_json::to_value(device.stop_recording().await?)?),
        "toggle" => print_json(serde_json::to_value(device.toggle_recording().await?)?),
        "status" => print_json(serde_json::to_value(device.record_status().await?)?),
        "mark" => print_json(serde_json::to_value(device.record_mark().await?)?),
        "realtime" => {
            let enabled = match required(rest, 1, "realtime on|off")? {
                value if value.eq_ignore_ascii_case("on") => true,
                value if value.eq_ignore_ascii_case("off") => false,
                other => return Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
            };
            print_json(serde_json::to_value(
                device.set_realtime_enabled(enabled).await?,
            )?)
        }
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

/// `realtime <seconds> [output.wav]`：开实时录音收流，每秒打印速率，结束停录。
async fn realtime_stream(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    let seconds: u64 = rest
        .first()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let output = rest.get(1);

    let session = device.start_realtime_recording().await?;
    let mut audio = session.subscribe();
    println!("recording {seconds}s (Ctrl-C to abort), 16 kHz mono ...");

    let started = tokio::time::Instant::now();
    let mut samples_total = 0_usize;
    let mut chunks_total = 0_usize;
    let mut lagged_total = 0_u64;
    let mut collected: Option<Vec<i16>> = output.map(|_| Vec::new());
    let deadline = started + Duration::from_secs(seconds);
    let mut next_report = started + Duration::from_secs(1);

    loop {
        let recv = tokio::time::timeout_at(deadline, audio.recv()).await;
        let chunk = match recv {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(count))) => {
                lagged_total += count;
                continue;
            }
            // 超时到期或通道关闭都走正常收尾
            _ => break,
        };
        chunks_total += 1;
        samples_total += chunk.samples.len();
        if let Some(buffer) = collected.as_mut() {
            buffer.extend_from_slice(&chunk.samples);
        }
        if tokio::time::Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            let elapsed = started.elapsed().as_secs_f64();
            println!(
                "  {elapsed:5.1}s  chunks={chunks_total}  samples={samples_total}  rate={:.0} samples/s",
                samples_total as f64 / elapsed
            );
        }
    }

    let result = session.stop().await;
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "done: {chunks_total} chunks, {samples_total} samples ({:.1}s, {:.0} samples/s), lagged={lagged_total}",
        elapsed,
        samples_total as f64 / elapsed.max(0.001)
    );
    if let (Some(buffer), Some(path)) = (collected.as_ref(), output) {
        tokio::fs::write(
            path,
            reai_card_sdk::wav_bytes(buffer, reai_card_sdk::SAMPLE_RATE_HZ),
        )
        .await?;
        println!("wav saved -> {path} ({} samples)", buffer.len());
    }
    result
}

/// `decode <in.apm> <out.wav>`：固件录音是裸 IMA ADPCM 流，离线解码成可播放的 WAV。
async fn decode(rest: &[String]) -> reai_card_sdk::Result<()> {
    let input = required(rest, 0, "input .apm")?;
    let output = required(rest, 1, "output .wav")?;
    let adpcm = tokio::fs::read(input).await?;
    let samples = reai_card_sdk::AdpcmDecoder::new().decode(&adpcm);
    tokio::fs::write(
        output,
        reai_card_sdk::wav_bytes(&samples, reai_card_sdk::SAMPLE_RATE_HZ),
    )
    .await?;
    println!(
        "decoded {} bytes ADPCM -> {} samples ({:.2}s @16kHz) -> {output}",
        adpcm.len(),
        samples.len(),
        samples.len() as f64 / reai_card_sdk::SAMPLE_RATE_HZ as f64
    );
    Ok(())
}

async fn take(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    let name = required(rest, 0, "file name")?;
    let output = required(rest, 1, "output path")?;
    let part = format!("{output}.part");
    let mut writer = tokio::fs::File::create(&part).await?;
    let result = device.take_file_to(name, &mut writer).await?;
    tokio::fs::rename(&part, output).await?;
    println!(
        "took {} bytes (device file `{}` removed) -> {output}",
        result.bytes_written, result.name
    );
    Ok(())
}

async fn wifi(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "wifi subcommand")? {
        "open" => print_json(serde_json::to_value(device.open_wifi().await?)?),
        "close" => print_json(serde_json::to_value(device.close_wifi().await?)?),
        "status" => print_json(serde_json::to_value(device.wifi_status().await?)?),
        "scan" => {
            let networks = device.scan_wifi().await?;
            println!("{} networks", networks.len());
            for network in networks {
                println!(
                    "  [{:<2}] {:<32} rssi={:<5} {}",
                    network.index,
                    network.ssid.as_deref().unwrap_or("<hidden>"),
                    network
                        .rssi
                        .map(|rssi| rssi.to_string())
                        .unwrap_or_else(|| "-".into()),
                    network.encryption.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
        "connect" => {
            let ssid = required(rest, 1, "ssid")?;
            let password = required(rest, 2, "password")?;
            print_json(serde_json::to_value(
                device.connect_wifi(ssid, password).await?,
            )?)
        }
        "disconnect" => print_json(serde_json::to_value(device.disconnect_wifi().await?)?),
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

async fn ap(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "ap subcommand")? {
        "start" => print_json(serde_json::to_value(
            device
                .start_access_point(
                    rest.get(1).map(|value| value.as_str()),
                    rest.get(2).map(|value| value.as_str()),
                )
                .await?,
        )?),
        "stop" => print_json(serde_json::to_value(device.stop_access_point().await?)?),
        "status" => print_json(serde_json::to_value(device.access_point_status().await?)?),
        "ota-check" => {
            let client = device.access_point_client().await?;
            print_json(serde_json::to_value(client.ota_status().await?)?)
        }
        "ota-upload" => {
            let path = required(rest, 1, "firmware .bin path")?;
            let firmware = tokio::fs::read(path).await?;
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("firmware.bin")
                .to_owned();
            let client = device.access_point_client().await?;
            let before = client.ota_status().await?;
            println!(
                "current WiFi firmware {} (code {}), uploading {} ({} bytes) ...",
                before.current_version,
                before.current_version_code,
                name,
                firmware.len()
            );
            let result = client.upload_firmware(firmware, &name).await?;
            println!("ESP32 restarts in ~3s; wait for WIFI_AP_START before using the AP again");
            print_json(serde_json::to_value(result)?)
        }
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

fn dfu_options(
    packet_size: Option<&String>,
    prn: Option<&String>,
) -> reai_card_sdk::Result<DfuOptions> {
    let mut options = DfuOptions::default();
    if let Some(size) = packet_size {
        options.packet_size = size.parse().map_err(|_| {
            reai_card_sdk::SdkError::InvalidCommand(format!("invalid packet size `{size}`"))
        })?;
    }
    if let Some(prn) = prn {
        options.prn = prn
            .parse()
            .map_err(|_| reai_card_sdk::SdkError::InvalidCommand(format!("invalid prn `{prn}`")))?;
    }
    Ok(options)
}

/// 每 5% 或阶段切换时打一行进度。
fn dfu_progress_printer() -> impl FnMut(DfuProgress) + Send {
    let mut last: Option<(DfuStage, u8)> = None;
    move |progress| {
        let bucket = progress.percent() / 5;
        if last != Some((progress.stage, bucket)) {
            let label = match progress.stage {
                DfuStage::InitPacket => "init packet",
                DfuStage::Firmware => "firmware",
            };
            println!(
                "dfu {label}: {:>3}% ({}/{} bytes)",
                progress.percent(),
                progress.sent,
                progress.total
            );
            last = Some((progress.stage, bucket));
        }
    }
}

fn print_dfu_report(report: &reai_card_sdk::DfuReport) -> reai_card_sdk::Result<()> {
    println!(
        "dfu done: {} bytes in {:.1}s, {} objects, {} rewinds (dropped tail packets), {} rebuilds{}; bootloader validates and reboots into the new app",
        report.firmware_bytes,
        report.elapsed.as_secs_f64(),
        report.objects,
        report.rewinds,
        report.retries,
        if report.resumed_from > 0 {
            format!(", resumed from {}", report.resumed_from)
        } else {
            String::new()
        }
    );
    Ok(())
}

async fn uploads(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "uploads subcommand")? {
        "list" => {
            let files = device.list_uploads().await?;
            println!("{} uploads", files.len());
            print_json(serde_json::to_value(files)?)?;
            Ok(())
        }
        "upload" => {
            let name = required(rest, 1, "name")?;
            let url = rest.get(2).map(String::as_str);
            println!("uploading {name} (blocks until the device finishes the HTTP upload) ...");
            print_json(serde_json::to_value(
                device.upload_file_via_sta_to(name, url).await?,
            )?)
        }
        "upload-all" => {
            let url = rest.get(1).map(String::as_str);
            let results = device.upload_all_via_sta(url).await?;
            for (name, outcome) in &results {
                match outcome {
                    Ok(_) => println!("✅ {name}"),
                    Err(error) => println!("❌ {name}: {error}"),
                }
            }
            let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
            println!("{}/{} uploaded", ok, results.len());
            Ok(())
        }
        "delete" => print_json(serde_json::to_value(
            device.delete_upload(required(rest, 1, "name")?).await?,
        )?),
        "clear" => print_json(serde_json::to_value(device.clear_uploads().await?)?),
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

async fn config(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "config subcommand")? {
        "get" => print_json(device.get_config(required(rest, 1, "key")?).await?),
        "set" => print_json(serde_json::to_value(
            device
                .set_config(required(rest, 1, "key")?, required(rest, 2, "value")?)
                .await?,
        )?),
        "del" => print_json(serde_json::to_value(
            device.delete_config(required(rest, 1, "key")?).await?,
        )?),
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

/// 按键 / 侧边开关绑定：固件在设备侧执行动作并上报事件，这里只改配置表。
async fn controls(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    fn input(text: &str) -> reai_card_sdk::Result<ControlInput> {
        ControlInput::from_config_key(text).ok_or_else(|| {
            reai_card_sdk::SdkError::InvalidCommand(format!(
                "unknown input `{text}`; expected one of {}",
                ControlInput::ALL
                    .iter()
                    .map(|input| input.config_key())
                    .collect::<Vec<_>>()
                    .join("|")
            ))
        })
    }
    match required(rest, 0, "controls subcommand")? {
        "list" => print_json(serde_json::to_value(device.control_bindings().await?)?),
        "set" => {
            let target = input(required(rest, 1, "input")?)?;
            let action = ControlAction::parse(required(rest, 2, "action")?);
            device.set_control_action(target, action.clone()).await?;
            println!("{target} → {action}");
            Ok(())
        }
        "clear" => {
            let target = input(required(rest, 1, "input")?)?;
            device.clear_control_action(target).await?;
            println!("{target} cleared");
            Ok(())
        }
        "preset" => match required(rest, 1, "preset name")? {
            "recorder" => {
                let bindings = ControlBindings::recorder();
                device.apply_control_bindings(&bindings).await?;
                print_json(serde_json::to_value(bindings)?)
            }
            other => Err(reai_card_sdk::SdkError::InvalidCommand(format!(
                "unknown preset `{other}`"
            ))),
        },
        "reset" => {
            for target in ControlInput::ALL {
                device.clear_control_action(target).await?;
            }
            println!("all six inputs cleared");
            Ok(())
        }
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

async fn ota(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "ota subcommand")? {
        "check" => print_json(serde_json::to_value(device.check_wifi_ota().await?)?),
        "start" => print_json(serde_json::to_value(device.start_wifi_ota().await?)?),
        "status" => print_json(serde_json::to_value(device.wifi_ota_status().await?)?),
        "update" => {
            let secs = rest
                .get(1)
                .map(|value| value.parse::<u64>().expect("seconds must be a number"))
                .unwrap_or(600);
            let check = device.check_wifi_ota().await?;
            if !check.update_available {
                println!("no update available");
                return Ok(());
            }
            let final_progress = device
                .update_wifi_firmware(Duration::from_secs(secs))
                .await?;
            print_json(serde_json::to_value(final_progress)?)
        }
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

async fn auth(device: &DyjDevice, rest: &[String]) -> reai_card_sdk::Result<()> {
    match required(rest, 0, "auth subcommand")? {
        "activate" => print_json(serde_json::to_value(
            device
                .activate(
                    required(rest, 1, "deviceId")?,
                    required(rest, 2, "deviceName")?,
                    required(rest, 3, "code")?,
                )
                .await?,
        )?),
        "clear" => print_json(serde_json::to_value(device.clear_activation().await?)?),
        other => Err(reai_card_sdk::SdkError::InvalidCommand(other.into())),
    }
}

fn required<'a>(args: &'a [String], index: usize, what: &str) -> reai_card_sdk::Result<&'a str> {
    args.get(index)
        .map(|value| value.as_str())
        .ok_or_else(|| reai_card_sdk::SdkError::InvalidCommand(format!("missing argument: {what}")))
}

fn print_json(value: Value) -> reai_card_sdk::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
    Ok(())
}
