//! Nordic Secure DFU 客户端：给主控 nRF52840 升级固件。
//!
//! 主控收到 `OTA` 命令后重启进 Secure Bootloader（nRF5 SDK 17.1，广播名
//! [`DFU_ADVERTISED_NAME`]），之后由上位机按 Nordic Secure DFU 协议把 nrfutil 生成的
//! `.zip` 包推过去：先发 init packet（`.dat`，带签名与版本/硬件要求），再发固件镜像
//! （`.bin`），bootloader 校验签名后自行重启进新应用。
//!
//! 协议只有两个特征：Control Point（有响应写 + 通知）承载命令与响应，Packet（无响应写）
//! 承载数据。数据按"对象"分段：命令对象装 init packet，数据对象（典型 4096 字节）逐段装
//! 固件；每个对象经 `Create → 写入 → CRC 校验 → Execute` 提交。中断后重连可以从
//! bootloader 报告的偏移续传。
//!
//! 本模块不依赖具体 BLE 栈：[`DfuTransport`] 只要求能写两个特征、收通知；btleplug 实现
//! 在 `DeviceManager::connect_dfu`。

use std::{
    fmt,
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use tokio::{sync::Mutex, time::timeout};

use crate::{Result, SdkError, transport::NotificationStream};

/// Secure DFU 服务（16 位 UUID `0xFE59`）。
pub const DFU_SERVICE_UUID: &str = "0000fe59-0000-1000-8000-00805f9b34fb";
/// DFU Control Point：写命令、收响应/PRN 通知。
pub const DFU_CONTROL_POINT_UUID: &str = "8ec90001-f315-4f60-9fb8-838830daea50";
/// DFU Packet：无响应写，承载 init packet 与固件数据。
pub const DFU_PACKET_UUID: &str = "8ec90002-f315-4f60-9fb8-838830daea50";
/// bootloader 的广播名（`sdk_config.h` `NRF_DFU_BLE_ADV_NAME`）。
pub const DFU_ADVERTISED_NAME: &str = "DFU_DYJ";

const OP_CREATE: u8 = 0x01;
const OP_SET_PRN: u8 = 0x02;
const OP_CALCULATE_CRC: u8 = 0x03;
const OP_EXECUTE: u8 = 0x04;
const OP_SELECT: u8 = 0x06;
const OP_RESPONSE: u8 = 0x60;

const OBJECT_COMMAND: u8 = 0x01;
const OBJECT_DATA: u8 = 0x02;

const RESULT_SUCCESS: u8 = 0x01;
const RESULT_INVALID_OBJECT: u8 = 0x05;
const RESULT_OPERATION_NOT_PERMITTED: u8 = 0x08;
const RESULT_EXTENDED_ERROR: u8 = 0x0B;

/// 一个 nrfutil DFU 包里本 SDK 会传输的内容：init packet（`.dat`）+ 应用镜像（`.bin`）。
#[derive(Clone, PartialEq, Eq)]
pub struct DfuPackage {
    pub init_packet: Vec<u8>,
    pub firmware: Vec<u8>,
}

impl fmt::Debug for DfuPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DfuPackage")
            .field("init_packet_bytes", &self.init_packet.len())
            .field("firmware_bytes", &self.firmware.len())
            .finish()
    }
}

#[derive(Deserialize)]
struct ManifestFile {
    manifest: Manifest,
}

#[derive(Deserialize, Default)]
struct Manifest {
    application: Option<ManifestImage>,
    bootloader: Option<ManifestImage>,
    softdevice: Option<ManifestImage>,
    softdevice_bootloader: Option<ManifestImage>,
}

#[derive(Deserialize)]
struct ManifestImage {
    bin_file: String,
    dat_file: String,
}

impl DfuPackage {
    /// 直接给出两段内容（例如从自定义渠道取得）。
    pub fn from_parts(init_packet: Vec<u8>, firmware: Vec<u8>) -> Result<Self> {
        if init_packet.is_empty() {
            return Err(SdkError::DfuPackage("init packet is empty".into()));
        }
        if firmware.is_empty() {
            return Err(SdkError::DfuPackage("firmware image is empty".into()));
        }
        Ok(Self {
            init_packet,
            firmware,
        })
    }

    /// 解析 `nrfutil pkg generate` 产出的 zip（`manifest.json` + `.dat` + `.bin`）。
    ///
    /// 只支持应用镜像；带 SoftDevice / bootloader 镜像的包需要两段式传输，
    /// 本 SDK 不做，直接报错。
    pub fn from_zip_bytes(bytes: &[u8]) -> Result<Self> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|error| SdkError::DfuPackage(format!("not a zip archive: {error}")))?;
        let manifest: ManifestFile = {
            let mut file = archive
                .by_name("manifest.json")
                .map_err(|error| SdkError::DfuPackage(format!("manifest.json missing: {error}")))?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            serde_json::from_str(&text)
                .map_err(|error| SdkError::DfuPackage(format!("manifest.json invalid: {error}")))?
        };
        let manifest = manifest.manifest;
        if manifest.bootloader.is_some()
            || manifest.softdevice.is_some()
            || manifest.softdevice_bootloader.is_some()
        {
            return Err(SdkError::DfuPackage(
                "package carries SoftDevice/bootloader images; only application-only packages are supported"
                    .into(),
            ));
        }
        let image = manifest
            .application
            .ok_or_else(|| SdkError::DfuPackage("manifest has no `application` image".into()))?;
        let mut read_entry = |name: &str| -> Result<Vec<u8>> {
            let mut file = archive
                .by_name(name)
                .map_err(|error| SdkError::DfuPackage(format!("`{name}` missing: {error}")))?;
            let mut data = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut data)?;
            Ok(data)
        };
        let init_packet = read_entry(&image.dat_file)?;
        let firmware = read_entry(&image.bin_file)?;
        tracing::info!(target: "dyj.dfu", "📦 DFU 包解析完成 init={}字节 固件={}字节（{} / {}）", init_packet.len(), firmware.len(), image.dat_file, image.bin_file);
        Self::from_parts(init_packet, firmware)
    }

    pub fn from_zip_path(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref())?;
        Self::from_zip_bytes(&bytes)
    }
}

/// DFU 侧的 BLE 收发缝隙，便于不接硬件做协议测试。
#[async_trait]
pub trait DfuTransport: Send + Sync {
    /// 写 Control Point（有响应）。
    async fn write_control(&self, bytes: &[u8]) -> Result<()>;
    /// 写 Packet（无响应）。
    async fn write_packet(&self, bytes: &[u8]) -> Result<()>;
    /// Control Point 的通知流。
    async fn notifications(&self) -> Result<NotificationStream>;
    async fn disconnect(&self) -> Result<()>;
}

pub type SharedDfuTransport = Arc<dyn DfuTransport>;

/// 传输参数。
#[derive(Debug, Clone)]
pub struct DfuOptions {
    /// 每个 Packet 写的字节数。20 是 ATT 默认 MTU（23）下的安全值；若确认链路协商了
    /// 更大的 MTU（bootloader 上限 247 → 244 字节），可以调大换速度。
    pub packet_size: usize,
    /// Packet Receipt Notification：每写 `prn` 个包让 bootloader 回一次 CRC，既做流控
    /// （限制在途数据，避免无响应写被底层静默丢弃）也能尽早发现丢包。0 = 关闭。
    pub prn: u16,
    /// 单个对象 CRC 不符时的重建次数。
    pub object_retries: u32,
    /// 等待一条响应/PRN 通知的上限。init packet 的 Execute 含签名校验，约 1s。
    pub response_timeout: Duration,
}

impl Default for DfuOptions {
    fn default() -> Self {
        Self {
            packet_size: 20,
            prn: 10,
            object_retries: 3,
            response_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DfuStage {
    InitPacket,
    Firmware,
}

/// 进度：`sent`/`total` 为当前阶段的字节数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DfuProgress {
    pub stage: DfuStage,
    pub sent: usize,
    pub total: usize,
}

impl DfuProgress {
    pub fn percent(&self) -> u8 {
        if self.total == 0 {
            return 100;
        }
        (self.sent.saturating_mul(100) / self.total).min(100) as u8
    }
}

/// 一次成功上传的统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DfuReport {
    pub init_packet_bytes: usize,
    pub firmware_bytes: usize,
    /// 从 bootloader 已有的这个偏移开始续传（0 = 从头）。
    pub resumed_from: usize,
    /// 本次提交（Execute）的数据对象个数。
    pub objects: u32,
    /// 因 CRC 不符重建对象的次数。
    pub retries: u32,
    pub elapsed: Duration,
}

/// 已连接到 bootloader 的 DFU 目标。由 `DeviceManager::connect_dfu` 创建，
/// 或用自定义 [`DfuTransport`] 经 [`DfuTarget::new`] 创建。
pub struct DfuTarget {
    transport: SharedDfuTransport,
    notifications: Mutex<NotificationStream>,
}

/// Select / CRC 响应里的对象信息。
#[derive(Debug, Clone, Copy)]
struct ObjectInfo {
    max_size: u32,
    offset: u32,
    crc: u32,
}

/// 一段数据写完后的结果：全部写完，或 PRN 校验发现 bootloader 侧内容不符。
enum Written {
    Complete,
    Mismatch { offset: u32, crc: u32 },
}

impl DfuTarget {
    pub async fn new(transport: SharedDfuTransport) -> Result<Self> {
        let notifications = transport.notifications().await?;
        Ok(Self {
            transport,
            notifications: Mutex::new(notifications),
        })
    }

    /// 把整个包传给 bootloader。成功返回后 bootloader 会校验镜像并自行重启进新应用，
    /// 连接随之断开。`progress` 在每个 PRN 批次 / 每个对象结束时回调。
    pub async fn upload(
        &self,
        package: &DfuPackage,
        options: &DfuOptions,
        mut progress: impl FnMut(DfuProgress) + Send,
    ) -> Result<DfuReport> {
        if options.packet_size == 0 {
            return Err(SdkError::InvalidCommand(
                "DFU packet_size must be at least 1".into(),
            ));
        }
        let started = Instant::now();
        let mut stream = self.notifications.lock().await;
        let mut report = DfuReport {
            init_packet_bytes: package.init_packet.len(),
            firmware_bytes: package.firmware.len(),
            ..Default::default()
        };
        tracing::info!(target: "dyj.dfu", "⬆️ 开始 Secure DFU 上传 init={}字节 固件={}字节 packet={} prn={}", package.init_packet.len(), package.firmware.len(), options.packet_size, options.prn);

        self.request(
            &mut stream,
            OP_SET_PRN,
            &options.prn.to_le_bytes(),
            "set PRN",
            options,
        )
        .await?;
        self.send_init_packet(&mut stream, package, options, &mut progress, &mut report)
            .await?;
        self.send_firmware(&mut stream, package, options, &mut progress, &mut report)
            .await?;

        report.elapsed = started.elapsed();
        tracing::info!(target: "dyj.dfu", "✅ DFU 上传完成 {}字节 对象={} 重试={} 耗时={:.1}s，bootloader 校验后自行重启", report.firmware_bytes, report.objects, report.retries, report.elapsed.as_secs_f64());
        Ok(report)
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.transport.disconnect().await
    }

    async fn send_init_packet(
        &self,
        stream: &mut NotificationStream,
        package: &DfuPackage,
        options: &DfuOptions,
        progress: &mut (impl FnMut(DfuProgress) + Send),
        report: &mut DfuReport,
    ) -> Result<()> {
        let init = &package.init_packet;
        let info = self.select(stream, OBJECT_COMMAND, options).await?;
        if init.len() > info.max_size as usize {
            return Err(SdkError::DfuPackage(format!(
                "init packet is {} bytes but the bootloader command object holds at most {}",
                init.len(),
                info.max_size
            )));
        }
        let expected_crc = crc32fast::hash(init);
        if info.offset as usize == init.len() && info.crc == expected_crc {
            // 上次中断前已完整送达：不必重发，补一次 Execute（已执行过的会被拒绝，视为成功）。
            tracing::info!(target: "dyj.dfu", "📨 init packet 已在 bootloader 中，跳过发送");
            self.execute_tolerating_repeat(stream, options).await?;
        } else {
            let mut attempts = 0;
            loop {
                self.create(stream, OBJECT_COMMAND, init.len() as u32, options)
                    .await?;
                let mut hasher = crc32fast::Hasher::new();
                let written = self
                    .write_data(stream, init, 0, &mut hasher, options, &mut |sent| {
                        progress(DfuProgress {
                            stage: DfuStage::InitPacket,
                            sent,
                            total: init.len(),
                        })
                    })
                    .await?;
                let ok = match written {
                    Written::Complete => {
                        let (offset, crc) = self.calculate_crc(stream, options).await?;
                        offset as usize == init.len() && crc == expected_crc
                    }
                    Written::Mismatch { .. } => false,
                };
                if ok {
                    break;
                }
                attempts += 1;
                report.retries += 1;
                if attempts > options.object_retries {
                    return Err(dfu_error(
                        "init packet",
                        0,
                        None,
                        format!("CRC mismatch after {attempts} attempts"),
                    ));
                }
                tracing::warn!(target: "dyj.dfu", "⚠️ init packet CRC 不符，重发（第 {attempts} 次）");
            }
            // Execute 时 bootloader 校验签名 / 版本 / 硬件要求，失败以扩展错误码返回。
            self.request(stream, OP_EXECUTE, &[], "execute init packet", options)
                .await?;
            tracing::info!(target: "dyj.dfu", "📨 init packet 已被 bootloader 接受（签名/版本校验通过）");
        }
        progress(DfuProgress {
            stage: DfuStage::InitPacket,
            sent: init.len(),
            total: init.len(),
        });
        Ok(())
    }

    async fn send_firmware(
        &self,
        stream: &mut NotificationStream,
        package: &DfuPackage,
        options: &DfuOptions,
        progress: &mut (impl FnMut(DfuProgress) + Send),
        report: &mut DfuReport,
    ) -> Result<()> {
        let firmware = &package.firmware;
        let info = self.select(stream, OBJECT_DATA, options).await?;
        let max = info.max_size as usize;
        if max == 0 {
            return Err(dfu_error(
                "select data object",
                0,
                None,
                "bootloader reports a zero-size data object".into(),
            ));
        }
        tracing::debug!(target: "dyj.dfu", "📐 数据对象 max={} 已有 offset={} crc={:08x}", max, info.offset, info.crc);

        // done = bootloader 侧已确认与本包一致的字节数（已提交，或正在写的对象中已写入的部分）
        let mut done = 0usize;
        let mut hasher = crc32fast::Hasher::new();
        let bootloader_offset = info.offset as usize;
        if bootloader_offset > firmware.len() {
            return Err(dfu_error(
                "select data object",
                0,
                None,
                format!(
                    "bootloader already holds {bootloader_offset} bytes, more than this {}-byte image; power-cycle the device to start over",
                    firmware.len()
                ),
            ));
        }
        if bootloader_offset > 0 {
            let mut received = crc32fast::Hasher::new();
            received.update(&firmware[..bootloader_offset]);
            if received.clone().finalize() == info.crc {
                done = bootloader_offset;
                hasher = received;
                report.resumed_from = bootloader_offset;
                if bootloader_offset.is_multiple_of(max) || bootloader_offset == firmware.len() {
                    // 正好在对象边界：最后一个对象可能写完但没 Execute，补一次。
                    tracing::info!(target: "dyj.dfu", "⏩ 从 {bootloader_offset} 字节续传（对象边界，补 Execute）");
                    self.execute_tolerating_repeat(stream, options).await?;
                } else {
                    tracing::info!(target: "dyj.dfu", "⏩ 从 {bootloader_offset} 字节续传（对象中途）");
                }
            } else {
                // 未提交部分内容不符（或不是同一个包）：丢掉当前对象，从它的起点重建。
                done = (bootloader_offset / max) * max;
                hasher.update(&firmware[..done]);
                tracing::warn!(target: "dyj.dfu", "⚠️ bootloader 已有数据与本包不符，从 {done} 字节重建当前对象");
            }
        }

        while done < firmware.len() {
            let object_start = (done / max) * max;
            let object_end = (object_start + max).min(firmware.len());
            let mut attempts = 0;
            loop {
                if done == object_start {
                    self.create(
                        stream,
                        OBJECT_DATA,
                        (object_end - object_start) as u32,
                        options,
                    )
                    .await?;
                    hasher = crc32fast::Hasher::new();
                    hasher.update(&firmware[..object_start]);
                }
                let total = firmware.len();
                let written = self
                    .write_data(
                        stream,
                        &firmware[done..object_end],
                        done,
                        &mut hasher,
                        options,
                        &mut |sent| {
                            progress(DfuProgress {
                                stage: DfuStage::Firmware,
                                sent,
                                total,
                            })
                        },
                    )
                    .await?;
                let verified = match written {
                    Written::Complete => {
                        let (offset, crc) = self.calculate_crc(stream, options).await?;
                        let expected = hasher.clone().finalize();
                        if offset as usize == object_end && crc == expected {
                            true
                        } else {
                            tracing::warn!(target: "dyj.dfu", "⚠️ 对象 CRC 不符 offset={offset}/{object_end} crc={crc:08x}/{expected:08x}");
                            false
                        }
                    }
                    Written::Mismatch { offset, crc } => {
                        tracing::warn!(target: "dyj.dfu", "⚠️ PRN 校验不符 offset={offset} crc={crc:08x}");
                        false
                    }
                };
                if verified {
                    self.request(stream, OP_EXECUTE, &[], "execute data object", options)
                        .await?;
                    done = object_end;
                    report.objects += 1;
                    tracing::debug!(target: "dyj.dfu", "📦 对象已提交 {done}/{}", firmware.len());
                    break;
                }
                attempts += 1;
                report.retries += 1;
                if attempts > options.object_retries {
                    return Err(dfu_error(
                        "data object",
                        0,
                        None,
                        format!("object at {object_start} failed CRC {attempts} times"),
                    ));
                }
                tracing::warn!(target: "dyj.dfu", "🔁 重建对象 {object_start}..{object_end}（第 {attempts} 次）");
                done = object_start;
            }
        }
        progress(DfuProgress {
            stage: DfuStage::Firmware,
            sent: firmware.len(),
            total: firmware.len(),
        });
        Ok(())
    }

    /// 按 `packet_size` 分片写 `data`（它在整条数据流中的绝对起点是 `absolute_start`），
    /// 每 `prn` 个包等一次 PRN 通知并核对 offset/CRC。`hasher` 进来时是
    /// `absolute_start` 之前全部数据的 CRC 状态，出去时追加了已写的数据。
    async fn write_data(
        &self,
        stream: &mut NotificationStream,
        data: &[u8],
        absolute_start: usize,
        hasher: &mut crc32fast::Hasher,
        options: &DfuOptions,
        progress: &mut (impl FnMut(usize) + Send),
    ) -> Result<Written> {
        let mut sent = 0usize;
        let mut packets_since_prn: u16 = 0;
        for chunk in data.chunks(options.packet_size) {
            self.transport.write_packet(chunk).await?;
            hasher.update(chunk);
            sent += chunk.len();
            packets_since_prn += 1;
            if options.prn != 0 && packets_since_prn == options.prn {
                packets_since_prn = 0;
                let (offset, crc) = self.await_prn(stream, options).await?;
                let expected_offset = (absolute_start + sent) as u32;
                let expected_crc = hasher.clone().finalize();
                if offset != expected_offset || crc != expected_crc {
                    return Ok(Written::Mismatch { offset, crc });
                }
                progress(absolute_start + sent);
            }
        }
        progress(absolute_start + sent);
        Ok(Written::Complete)
    }

    /// 等 PRN 通知；等不到（计数口径与 bootloader 不一致时）退回显式 CRC 请求自愈。
    async fn await_prn(
        &self,
        stream: &mut NotificationStream,
        options: &DfuOptions,
    ) -> Result<(u32, u32)> {
        match timeout(options.response_timeout, stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                let payload = parse_response(&bytes, OP_CALCULATE_CRC, "PRN")?;
                parse_crc_payload(&payload)
            }
            Ok(Some(Err(error))) => Err(error),
            Ok(None) => Err(SdkError::Disconnected),
            Err(_) => {
                tracing::warn!(target: "dyj.dfu", "⚠️ 未收到 PRN 通知，改为显式请求 CRC");
                self.calculate_crc(stream, options).await
            }
        }
    }

    async fn select(
        &self,
        stream: &mut NotificationStream,
        object_type: u8,
        options: &DfuOptions,
    ) -> Result<ObjectInfo> {
        let payload = self
            .request(stream, OP_SELECT, &[object_type], "select object", options)
            .await?;
        if payload.len() < 12 {
            return Err(SdkError::InvalidFrame(format!(
                "DFU select response too short: {} bytes",
                payload.len()
            )));
        }
        Ok(ObjectInfo {
            max_size: u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            offset: u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]),
            crc: u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]),
        })
    }

    async fn create(
        &self,
        stream: &mut NotificationStream,
        object_type: u8,
        size: u32,
        options: &DfuOptions,
    ) -> Result<()> {
        let mut payload = vec![object_type];
        payload.extend_from_slice(&size.to_le_bytes());
        self.request(stream, OP_CREATE, &payload, "create object", options)
            .await?;
        Ok(())
    }

    async fn calculate_crc(
        &self,
        stream: &mut NotificationStream,
        options: &DfuOptions,
    ) -> Result<(u32, u32)> {
        let payload = self
            .request(stream, OP_CALCULATE_CRC, &[], "calculate CRC", options)
            .await?;
        parse_crc_payload(&payload)
    }

    /// 对可能已经执行过的对象再 Execute 一次：bootloader 会以"不允许/对象无效"拒绝，
    /// 这两种码在此视为成功。
    async fn execute_tolerating_repeat(
        &self,
        stream: &mut NotificationStream,
        options: &DfuOptions,
    ) -> Result<()> {
        match self
            .request(stream, OP_EXECUTE, &[], "execute object", options)
            .await
        {
            Ok(_) => Ok(()),
            Err(SdkError::Dfu {
                result: RESULT_OPERATION_NOT_PERMITTED | RESULT_INVALID_OBJECT,
                ..
            }) => {
                tracing::debug!(target: "dyj.dfu", "对象此前已执行，跳过");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// 写一条 Control Point 命令并取回它的响应载荷（结果码非成功时转成 [`SdkError::Dfu`]）。
    /// 夹在中间的迟到 PRN 通知会被跳过。
    async fn request(
        &self,
        stream: &mut NotificationStream,
        opcode: u8,
        payload: &[u8],
        operation: &'static str,
        options: &DfuOptions,
    ) -> Result<Vec<u8>> {
        let mut frame = Vec::with_capacity(1 + payload.len());
        frame.push(opcode);
        frame.extend_from_slice(payload);
        tracing::trace!(target: "dyj.dfu", "🔬 CP ← {operation} {:02x?}", frame);
        self.transport.write_control(&frame).await?;
        for _ in 0..4 {
            let bytes = match timeout(options.response_timeout, stream.next()).await {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(error))) => return Err(error),
                Ok(None) => return Err(SdkError::Disconnected),
                Err(_) => {
                    tracing::error!(target: "dyj.dfu", "⏱️ 等待 {operation} 响应超时（{}s）", options.response_timeout.as_secs());
                    return Err(SdkError::Timeout {
                        command: format!("DFU {operation}"),
                        seconds: options.response_timeout.as_secs(),
                    });
                }
            };
            tracing::trace!(target: "dyj.dfu", "🔬 CP → {:02x?}", bytes);
            if bytes.len() >= 2
                && bytes[0] == OP_RESPONSE
                && bytes[1] == OP_CALCULATE_CRC
                && opcode != OP_CALCULATE_CRC
            {
                tracing::debug!(target: "dyj.dfu", "跳过迟到的 PRN 通知");
                continue;
            }
            return parse_response(&bytes, opcode, operation);
        }
        Err(SdkError::InvalidFrame(format!(
            "DFU {operation}: no matching response among the last notifications"
        )))
    }
}

fn parse_crc_payload(payload: &[u8]) -> Result<(u32, u32)> {
    if payload.len() < 8 {
        return Err(SdkError::InvalidFrame(format!(
            "DFU CRC response too short: {} bytes",
            payload.len()
        )));
    }
    Ok((
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]),
    ))
}

/// 校验 `0x60 <opcode> <result> [payload]` 响应帧，成功时返回 payload。
fn parse_response(bytes: &[u8], expected_opcode: u8, operation: &'static str) -> Result<Vec<u8>> {
    if bytes.len() < 3 || bytes[0] != OP_RESPONSE {
        return Err(SdkError::InvalidFrame(format!(
            "DFU {operation}: malformed response {bytes:02x?}"
        )));
    }
    if bytes[1] != expected_opcode {
        return Err(SdkError::InvalidFrame(format!(
            "DFU {operation}: response is for opcode 0x{:02x}, expected 0x{expected_opcode:02x}",
            bytes[1]
        )));
    }
    match bytes[2] {
        RESULT_SUCCESS => Ok(bytes[3..].to_vec()),
        RESULT_EXTENDED_ERROR => {
            let extended = bytes.get(3).copied();
            Err(dfu_error(
                operation,
                RESULT_EXTENDED_ERROR,
                extended,
                extended_error_message(extended).to_owned(),
            ))
        }
        result => Err(dfu_error(
            operation,
            result,
            None,
            result_message(result).to_owned(),
        )),
    }
}

fn dfu_error(
    operation: &'static str,
    result: u8,
    extended: Option<u8>,
    message: String,
) -> SdkError {
    tracing::error!(target: "dyj.dfu", "❌ DFU {operation} 失败 result=0x{result:02x} ext={extended:?}: {message}");
    SdkError::Dfu {
        operation,
        result,
        extended,
        message,
    }
}

fn result_message(result: u8) -> &'static str {
    match result {
        0x00 => "invalid opcode",
        0x02 => "opcode not supported",
        0x03 => "invalid parameter",
        0x04 => "insufficient resources",
        0x05 => "invalid object",
        0x07 => "unsupported object type",
        0x08 => "operation not permitted",
        0x0A => "operation failed",
        _ => "unknown result code",
    }
}

/// nRF5 SDK `nrf_dfu_ext_error_code_t`。
fn extended_error_message(code: Option<u8>) -> &'static str {
    match code {
        Some(0x01) => "invalid error code",
        Some(0x02) => "wrong command format",
        Some(0x03) => "unknown command",
        Some(0x04) => "init command invalid (malformed or missing fields)",
        Some(0x05) => {
            "firmware version failure (downgrade prevention: package version is below the installed one)"
        }
        Some(0x06) => "hardware version failure (package built for a different hw-version)",
        Some(0x07) => {
            "SoftDevice version failure (package --sd-req does not match the installed SoftDevice)"
        }
        Some(0x08) => "signature missing",
        Some(0x09) => "wrong hash type",
        Some(0x0A) => "hash failed",
        Some(0x0B) => "wrong signature type",
        Some(0x0C) => "verification failed (signature does not match the bootloader public key)",
        Some(0x0D) => "insufficient space",
        _ => "extended error",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex as StdMutex},
    };

    use futures::stream;
    use tokio::sync::{Mutex, mpsc};

    use super::*;

    /// 模拟 nRF5 SDK 17 bootloader 的 Secure DFU 请求处理器。
    struct MockBootloader {
        notify_tx: mpsc::UnboundedSender<Result<Vec<u8>>>,
        notify_rx: Mutex<Option<mpsc::UnboundedReceiver<Result<Vec<u8>>>>>,
        state: StdMutex<State>,
        max_data_object: usize,
        /// 丢掉第 N 个数据包（一次），模拟无响应写被底层吞掉。
        drop_data_packet: Option<usize>,
        /// 让 init packet 的 Execute 以扩展错误码失败。
        init_execute_ext_error: Option<u8>,
    }

    #[derive(Default)]
    struct State {
        prn: u16,
        prn_count: u16,
        current: Option<u8>,
        init: Vec<u8>,
        init_created: usize,
        init_executed: bool,
        committed: Vec<u8>,
        pending: Vec<u8>,
        pending_created: Option<usize>,
        creates: u32,
        data_packets: usize,
        dropped: bool,
    }

    impl MockBootloader {
        fn new(max_data_object: usize) -> Arc<Self> {
            let (notify_tx, notify_rx) = mpsc::unbounded_channel();
            Arc::new(Self {
                notify_tx,
                notify_rx: Mutex::new(Some(notify_rx)),
                state: StdMutex::new(State::default()),
                max_data_object,
                drop_data_packet: None,
                init_execute_ext_error: None,
            })
        }

        fn respond(&self, opcode: u8, result: u8, payload: &[u8]) {
            let mut frame = vec![OP_RESPONSE, opcode, result];
            frame.extend_from_slice(payload);
            let _ = self.notify_tx.send(Ok(frame));
        }

        fn crc_payload(offset: usize, data: &[u8]) -> Vec<u8> {
            let mut payload = (offset as u32).to_le_bytes().to_vec();
            payload.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            payload
        }

        fn data_stream(state: &State) -> Vec<u8> {
            let mut all = state.committed.clone();
            all.extend_from_slice(&state.pending);
            all
        }
    }

    #[async_trait]
    impl DfuTransport for MockBootloader {
        async fn write_control(&self, bytes: &[u8]) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            match bytes[0] {
                OP_SET_PRN => {
                    state.prn = u16::from_le_bytes([bytes[1], bytes[2]]);
                    state.prn_count = 0;
                    self.respond(OP_SET_PRN, RESULT_SUCCESS, &[]);
                }
                OP_SELECT => {
                    let (max, offset, data) = match bytes[1] {
                        OBJECT_COMMAND => (512usize, state.init.len(), state.init.clone()),
                        _ => {
                            let data = Self::data_stream(&state);
                            (self.max_data_object, data.len(), data)
                        }
                    };
                    let mut payload = (max as u32).to_le_bytes().to_vec();
                    payload.extend_from_slice(&Self::crc_payload(offset, &data));
                    self.respond(OP_SELECT, RESULT_SUCCESS, &payload);
                }
                OP_CREATE => {
                    let size =
                        u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize;
                    state.current = Some(bytes[1]);
                    state.prn_count = 0;
                    state.creates += 1;
                    match bytes[1] {
                        OBJECT_COMMAND => {
                            state.init.clear();
                            state.init_created = size;
                            state.init_executed = false;
                        }
                        _ => {
                            state.pending.clear();
                            state.pending_created = Some(size);
                        }
                    }
                    self.respond(OP_CREATE, RESULT_SUCCESS, &[]);
                }
                OP_CALCULATE_CRC => {
                    let payload = match state.current {
                        Some(OBJECT_COMMAND) => Self::crc_payload(state.init.len(), &state.init),
                        _ => {
                            let data = Self::data_stream(&state);
                            Self::crc_payload(data.len(), &data)
                        }
                    };
                    self.respond(OP_CALCULATE_CRC, RESULT_SUCCESS, &payload);
                }
                OP_EXECUTE => match state.current {
                    Some(OBJECT_COMMAND) => {
                        if state.init_executed {
                            self.respond(OP_EXECUTE, RESULT_OPERATION_NOT_PERMITTED, &[]);
                        } else if state.init.len() != state.init_created {
                            self.respond(OP_EXECUTE, RESULT_INVALID_OBJECT, &[]);
                        } else if let Some(code) = self.init_execute_ext_error {
                            self.respond(OP_EXECUTE, RESULT_EXTENDED_ERROR, &[code]);
                        } else {
                            state.init_executed = true;
                            self.respond(OP_EXECUTE, RESULT_SUCCESS, &[]);
                        }
                    }
                    _ => match state.pending_created {
                        None => self.respond(OP_EXECUTE, RESULT_OPERATION_NOT_PERMITTED, &[]),
                        Some(size) if size != state.pending.len() => {
                            self.respond(OP_EXECUTE, RESULT_INVALID_OBJECT, &[])
                        }
                        Some(_) => {
                            let pending = std::mem::take(&mut state.pending);
                            state.committed.extend_from_slice(&pending);
                            state.pending_created = None;
                            self.respond(OP_EXECUTE, RESULT_SUCCESS, &[]);
                        }
                    },
                },
                other => self.respond(other, 0x02, &[]),
            }
            Ok(())
        }

        async fn write_packet(&self, bytes: &[u8]) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            match state.current {
                Some(OBJECT_COMMAND) => state.init.extend_from_slice(bytes),
                _ => {
                    let index = state.data_packets;
                    state.data_packets += 1;
                    if self.drop_data_packet == Some(index) && !state.dropped {
                        state.dropped = true;
                    } else {
                        state.pending.extend_from_slice(bytes);
                    }
                }
            }
            if state.prn != 0 {
                state.prn_count += 1;
                if state.prn_count == state.prn {
                    state.prn_count = 0;
                    let payload = match state.current {
                        Some(OBJECT_COMMAND) => Self::crc_payload(state.init.len(), &state.init),
                        _ => {
                            let data = Self::data_stream(&state);
                            Self::crc_payload(data.len(), &data)
                        }
                    };
                    self.respond(OP_CALCULATE_CRC, RESULT_SUCCESS, &payload);
                }
            }
            Ok(())
        }

        async fn notifications(&self) -> Result<NotificationStream> {
            let receiver = self.notify_rx.lock().await.take().expect("one subscriber");
            Ok(Box::pin(stream::unfold(receiver, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            })))
        }

        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    fn sample_package() -> DfuPackage {
        let init: Vec<u8> = (0..142u32).map(|i| (i * 7 % 251) as u8).collect();
        let firmware: Vec<u8> = (0..1000u32).map(|i| (i * 13 % 256) as u8).collect();
        DfuPackage::from_parts(init, firmware).unwrap()
    }

    fn options() -> DfuOptions {
        DfuOptions {
            packet_size: 20,
            prn: 10,
            object_retries: 3,
            response_timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn uploads_a_package_object_by_object() {
        let bootloader = MockBootloader::new(256);
        let target = DfuTarget::new(bootloader.clone()).await.unwrap();
        let package = sample_package();
        let mut last = None;
        let report = target
            .upload(&package, &options(), |progress| last = Some(progress))
            .await
            .unwrap();

        let state = bootloader.state.lock().unwrap();
        assert!(state.init_executed);
        assert_eq!(state.init, package.init_packet);
        assert_eq!(state.committed, package.firmware);
        assert!(state.pending.is_empty());
        assert_eq!(
            state.creates,
            1 + 4,
            "1 个命令对象 + 4 个数据对象（1000/256）"
        );
        assert_eq!(report.objects, 4);
        assert_eq!(report.retries, 0);
        assert_eq!(report.resumed_from, 0);
        assert_eq!(report.firmware_bytes, 1000);
        let last = last.unwrap();
        assert_eq!(last.stage, DfuStage::Firmware);
        assert_eq!(last.percent(), 100);
    }

    #[tokio::test]
    async fn resumes_inside_a_partially_written_object() {
        let bootloader = MockBootloader::new(256);
        let package = sample_package();
        {
            let mut state = bootloader.state.lock().unwrap();
            state.init = package.init_packet.clone();
            state.init_created = state.init.len();
            state.init_executed = true;
            state.committed = package.firmware[..256].to_vec();
            state.pending = package.firmware[256..300].to_vec();
            state.pending_created = Some(256);
            state.current = Some(OBJECT_DATA);
        }
        let target = DfuTarget::new(bootloader.clone()).await.unwrap();
        let report = target.upload(&package, &options(), |_| {}).await.unwrap();

        let state = bootloader.state.lock().unwrap();
        assert_eq!(state.committed, package.firmware);
        assert_eq!(report.resumed_from, 300);
        // init 已在 bootloader 中：不重发；数据对象 1 续写不 Create，之后 2 个新对象
        assert_eq!(state.creates, 2);
        assert_eq!(report.objects, 3);
        assert_eq!(report.retries, 0);
    }

    #[tokio::test]
    async fn rebuilds_the_object_when_pending_bytes_differ() {
        let bootloader = MockBootloader::new(256);
        let package = sample_package();
        {
            let mut state = bootloader.state.lock().unwrap();
            state.init = package.init_packet.clone();
            state.init_created = state.init.len();
            state.init_executed = true;
            state.committed = package.firmware[..256].to_vec();
            state.pending = vec![0xEE; 44];
            state.pending_created = Some(256);
            state.current = Some(OBJECT_DATA);
        }
        let target = DfuTarget::new(bootloader.clone()).await.unwrap();
        let report = target.upload(&package, &options(), |_| {}).await.unwrap();

        let state = bootloader.state.lock().unwrap();
        assert_eq!(state.committed, package.firmware);
        assert_eq!(report.resumed_from, 0, "内容不符不算续传");
        assert_eq!(state.creates, 3, "对象 2 重建 + 对象 3、4");
    }

    #[tokio::test]
    async fn retries_an_object_after_a_dropped_packet() {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let bootloader = Arc::new(MockBootloader {
            notify_tx,
            notify_rx: Mutex::new(Some(notify_rx)),
            state: StdMutex::new(State::default()),
            max_data_object: 256,
            drop_data_packet: Some(5),
            init_execute_ext_error: None,
        });
        let target = DfuTarget::new(bootloader.clone()).await.unwrap();
        let package = sample_package();
        let report = target.upload(&package, &options(), |_| {}).await.unwrap();
        let state = bootloader.state.lock().unwrap();
        assert_eq!(state.committed, package.firmware);
        assert_eq!(report.retries, 1);
        assert_eq!(report.objects, 4);
    }

    #[tokio::test]
    async fn init_packet_rejection_surfaces_the_extended_error() {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let bootloader = Arc::new(MockBootloader {
            notify_tx,
            notify_rx: Mutex::new(Some(notify_rx)),
            state: StdMutex::new(State::default()),
            max_data_object: 256,
            drop_data_packet: None,
            init_execute_ext_error: Some(0x05),
        });
        let target = DfuTarget::new(bootloader.clone()).await.unwrap();
        let error = target
            .upload(&sample_package(), &options(), |_| {})
            .await
            .unwrap_err();
        match error {
            SdkError::Dfu {
                operation,
                result,
                extended,
                message,
            } => {
                assert_eq!(operation, "execute init packet");
                assert_eq!(result, RESULT_EXTENDED_ERROR);
                assert_eq!(extended, Some(0x05));
                assert!(message.contains("downgrade"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(bootloader.state.lock().unwrap().committed.is_empty());
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, stored).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn parses_nrfutil_application_packages() {
        let manifest = br#"{"manifest":{"application":{"bin_file":"nrf52840_xxaa.bin","dat_file":"nrf52840_xxaa.dat"}}}"#;
        let bytes = zip_with(&[
            ("nrf52840_xxaa.dat", &[1, 2, 3]),
            ("manifest.json", manifest),
            ("nrf52840_xxaa.bin", &[9; 300]),
        ]);
        let package = DfuPackage::from_zip_bytes(&bytes).unwrap();
        assert_eq!(package.init_packet, vec![1, 2, 3]);
        assert_eq!(package.firmware.len(), 300);
        assert_eq!(
            format!("{package:?}"),
            "DfuPackage { init_packet_bytes: 3, firmware_bytes: 300 }"
        );
    }

    #[test]
    fn rejects_packages_the_sdk_cannot_transfer() {
        let combo = br#"{"manifest":{"softdevice_bootloader":{"bin_file":"sd_bl.bin","dat_file":"sd_bl.dat"}}}"#;
        let bytes = zip_with(&[
            ("manifest.json", combo),
            ("sd_bl.bin", &[0; 4]),
            ("sd_bl.dat", &[0; 4]),
        ]);
        assert!(matches!(
            DfuPackage::from_zip_bytes(&bytes),
            Err(SdkError::DfuPackage(message)) if message.contains("SoftDevice")
        ));
        assert!(matches!(
            DfuPackage::from_zip_bytes(b"not a zip"),
            Err(SdkError::DfuPackage(_))
        ));
        assert!(matches!(
            DfuPackage::from_parts(vec![], vec![1]),
            Err(SdkError::DfuPackage(_))
        ));
    }
}
