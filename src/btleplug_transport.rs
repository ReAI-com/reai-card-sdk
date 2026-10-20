use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
        WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures::StreamExt;
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    DyjDevice, NUS_NOTIFY_UUID, NUS_SERVICE_UUID, NUS_WRITE_UUID, Result, SdkError,
    dfu::{
        DFU_ADVERTISED_NAME, DFU_CONTROL_POINT_UUID, DFU_PACKET_UUID, DFU_SERVICE_UUID, DfuOptions,
        DfuPackage, DfuProgress, DfuReport, DfuTarget, DfuTransport, SharedDfuTransport,
    },
    transport::{BleTransport, NotificationStream, SharedTransport},
    types::DiscoveredDevice,
};

/// `OTA` 命令后等待 bootloader 开始广播的总时长（设备复位 + 广播通常 2–5s）。
const DFU_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);
/// 每轮扫描 bootloader 的窗口。
const DFU_SCAN_WINDOW: Duration = Duration::from_secs(2);

/// Discovers DYJ devices through the host Bluetooth adapter.
#[derive(Clone)]
pub struct DeviceManager {
    adapter: Adapter,
}

impl DeviceManager {
    pub async fn new() -> Result<Self> {
        let manager = Manager::new().await.map_err(transport_error)?;
        let adapter = manager
            .adapters()
            .await
            .map_err(transport_error)?
            .into_iter()
            .next()
            .ok_or(SdkError::NoAdapter)?;
        Ok(Self { adapter })
    }

    /// Scans the NUS service for `duration` and returns devices whose local name begins with `DYJ`.
    pub async fn scan(&self, duration: Duration) -> Result<Vec<DiscoveredDevice>> {
        let service = parse_uuid(NUS_SERVICE_UUID)?;
        tracing::info!(target: "dyj.ble", "🔍 开始扫描 duration={}ms service={service}", duration.as_millis());
        self.adapter
            .start_scan(ScanFilter {
                services: vec![service],
            })
            .await
            .map_err(transport_error)?;
        sleep(duration).await;
        self.adapter.stop_scan().await.map_err(transport_error)?;

        let mut devices = Vec::new();
        for peripheral in self.adapter.peripherals().await.map_err(transport_error)? {
            let Some(properties) = peripheral.properties().await.map_err(transport_error)? else {
                continue;
            };
            let Some(name) = properties.local_name else {
                continue;
            };
            if !name.to_ascii_uppercase().starts_with("DYJ") {
                continue;
            }
            tracing::debug!(target: "dyj.ble", "🔍 发现设备 name={name} id={} rssi={:?}", peripheral.id(), properties.rssi);
            devices.push(DiscoveredDevice {
                id: peripheral.id().to_string(),
                name,
                rssi: properties.rssi,
            });
        }
        tracing::info!(target: "dyj.ble", "🔍 扫描完成 共{}台DYJ设备", devices.len());
        Ok(devices)
    }

    /// Connects, validates the NUS characteristic pair, subscribes to notifications, and syncs time.
    pub async fn connect(&self, discovered: &DiscoveredDevice) -> Result<DyjDevice> {
        tracing::info!(target: "dyj.ble", "🔗 开始连接 name={} id={}", discovered.name, discovered.id);
        let peripheral = self
            .adapter
            .peripherals()
            .await
            .map_err(transport_error)?
            .into_iter()
            .find(|candidate| candidate.id().to_string() == discovered.id)
            .ok_or_else(|| SdkError::DeviceNotFound(discovered.id.clone()))?;

        peripheral.connect().await.map_err(transport_error)?;
        tracing::debug!(target: "dyj.ble", "🔗 链路已建立，开始服务发现");
        if let Err(error) = peripheral.discover_services().await {
            let _ = peripheral.disconnect().await;
            tracing::error!(target: "dyj.ble", "❌ 服务发现失败: {error}");
            return Err(transport_error(error));
        }

        let write_uuid = parse_uuid(NUS_WRITE_UUID)?;
        let notify_uuid = parse_uuid(NUS_NOTIFY_UUID)?;
        let characteristics = peripheral.characteristics();
        let pair = (|| {
            let write = characteristics
                .iter()
                .find(|characteristic| {
                    characteristic.uuid == write_uuid
                        && (characteristic.properties.contains(CharPropFlags::WRITE)
                            || characteristic
                                .properties
                                .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE))
                })
                .cloned()
                .ok_or(SdkError::MissingCharacteristic("write"))?;
            let notify = characteristics
                .iter()
                .find(|characteristic| {
                    characteristic.uuid == notify_uuid
                        && characteristic.properties.contains(CharPropFlags::NOTIFY)
                })
                .cloned()
                .ok_or(SdkError::MissingCharacteristic("notify"))?;
            Ok::<_, SdkError>((write, notify))
        })();
        let (write, notify) = match pair {
            Ok(pair) => pair,
            Err(error) => {
                let _ = peripheral.disconnect().await;
                tracing::error!(target: "dyj.ble", "❌ NUS特征校验失败: {error}，已断开");
                return Err(error);
            }
        };
        tracing::debug!(target: "dyj.ble", "✅ NUS特征校验通过 write=6e400002 notify=6e400003");

        if let Err(error) = peripheral.subscribe(&notify).await {
            let _ = peripheral.disconnect().await;
            tracing::error!(target: "dyj.ble", "❌ 订阅通知失败: {error}，已断开");
            return Err(transport_error(error));
        }
        tracing::info!(target: "dyj.ble", "✅ 已订阅通知，开始时间同步握手");
        let transport: SharedTransport = Arc::new(BtleplugTransport { peripheral, write });
        match DyjDevice::from_transport(transport.clone()).await {
            Ok(device) => {
                tracing::info!(target: "dyj.ble", "✅ 连接就绪 name={}", discovered.name);
                Ok(device)
            }
            // 连接建立但握手（SYNC_TIME 等）失败：显式断开，否则链路会悬挂到
            // supervision timeout 才释放，占用设备的唯一 BLE 连接槽。
            Err(error) => {
                tracing::error!(target: "dyj.ble", "❌ 握手失败: {error}，主动断开");
                let _ = transport.disconnect().await;
                Err(error)
            }
        }
    }
}

impl DeviceManager {
    /// 扫描处于 DFU 模式的主控：bootloader 广播 Secure DFU 服务（`0xFE59`），
    /// 名为 [`DFU_ADVERTISED_NAME`]。按服务 UUID 过滤，名字只作辅助。
    pub async fn scan_dfu_targets(&self, duration: Duration) -> Result<Vec<DiscoveredDevice>> {
        let service = parse_uuid(DFU_SERVICE_UUID)?;
        tracing::info!(target: "dyj.dfu", "🔍 扫描 DFU 目标 duration={}ms", duration.as_millis());
        self.adapter
            .start_scan(ScanFilter {
                services: vec![service],
            })
            .await
            .map_err(transport_error)?;
        sleep(duration).await;
        self.adapter.stop_scan().await.map_err(transport_error)?;

        let mut targets = Vec::new();
        for peripheral in self.adapter.peripherals().await.map_err(transport_error)? {
            let Some(properties) = peripheral.properties().await.map_err(transport_error)? else {
                continue;
            };
            let advertises_dfu = properties.services.contains(&service);
            let name = properties.local_name.unwrap_or_default();
            if !advertises_dfu && !name.to_ascii_uppercase().starts_with("DFU") {
                continue;
            }
            tracing::debug!(target: "dyj.dfu", "🔍 发现 DFU 目标 name={name} id={} rssi={:?}", peripheral.id(), properties.rssi);
            targets.push(DiscoveredDevice {
                id: peripheral.id().to_string(),
                name: if name.is_empty() {
                    DFU_ADVERTISED_NAME.to_owned()
                } else {
                    name
                },
                rssi: properties.rssi,
            });
        }
        tracing::info!(target: "dyj.dfu", "🔍 扫描完成 共{}个 DFU 目标", targets.len());
        Ok(targets)
    }

    /// 连接 bootloader 并校验 Control Point / Packet 两个特征。
    ///
    /// 经 `OTA`（GPREGRET）进 bootloader 时蓝牙地址不变，CoreBluetooth 可能沿用上次
    /// 连接缓存的 NUS 服务表而发现不到 DFU 特征；此时断开、停 1s 再重连一次。
    pub async fn connect_dfu(&self, discovered: &DiscoveredDevice) -> Result<DfuTarget> {
        tracing::info!(target: "dyj.dfu", "🔗 连接 DFU 目标 name={} id={}", discovered.name, discovered.id);
        let peripheral = self
            .adapter
            .peripherals()
            .await
            .map_err(transport_error)?
            .into_iter()
            .find(|candidate| candidate.id().to_string() == discovered.id)
            .ok_or_else(|| SdkError::DeviceNotFound(discovered.id.clone()))?;

        let mut attempt = 0;
        let (control, packet) = loop {
            peripheral.connect().await.map_err(transport_error)?;
            if let Err(error) = peripheral.discover_services().await {
                let _ = peripheral.disconnect().await;
                return Err(transport_error(error));
            }
            match find_dfu_characteristics(&peripheral.characteristics()) {
                Ok(pair) => break pair,
                Err(error) if attempt == 0 => {
                    attempt += 1;
                    tracing::warn!(target: "dyj.dfu", "⚠️ 未发现 DFU 特征（{error}），可能是 GATT 缓存，断开后重试一次");
                    let _ = peripheral.disconnect().await;
                    sleep(Duration::from_secs(1)).await;
                }
                Err(error) => {
                    let _ = peripheral.disconnect().await;
                    tracing::error!(target: "dyj.dfu", "❌ DFU 特征校验失败: {error}");
                    return Err(error);
                }
            }
        };
        if let Err(error) = peripheral.subscribe(&control).await {
            let _ = peripheral.disconnect().await;
            return Err(transport_error(error));
        }
        tracing::info!(target: "dyj.dfu", "✅ DFU 目标就绪（Control Point 已订阅）");
        let transport: SharedDfuTransport = Arc::new(BtleplugDfuTransport {
            peripheral,
            control,
            packet,
        });
        DfuTarget::new(transport).await
    }

    /// 一条龙升级主控：`OTA` 进 DFU → 等 bootloader 广播 → 连接 → 推送整个包。
    ///
    /// 成功返回后 bootloader 校验镜像并重启进新应用，`device` 已失效，调用方重新扫描
    /// 连接即可。失败时设备可能停在 DFU 模式（继续广播 `DFU_DYJ`）：用
    /// [`Self::scan_dfu_targets`] + [`Self::connect_dfu`] 可直接续传，不必再发 `OTA`。
    pub async fn update_main_firmware(
        &self,
        device: &DyjDevice,
        package: &DfuPackage,
        options: &DfuOptions,
        progress: impl FnMut(DfuProgress) + Send,
    ) -> Result<DfuReport> {
        device.enter_dfu_mode().await?;
        // 设备随即复位，链路本就会掉；主动断开只是让本端立刻释放。
        let _ = device.disconnect().await;
        tracing::info!(target: "dyj.dfu", "🔄 设备正在重启进 bootloader，等待 {} 广播", DFU_ADVERTISED_NAME);

        let deadline = Instant::now() + DFU_DISCOVERY_TIMEOUT;
        let target = loop {
            if let Some(found) = self
                .scan_dfu_targets(DFU_SCAN_WINDOW)
                .await?
                .into_iter()
                .next()
            {
                break found;
            }
            if Instant::now() >= deadline {
                tracing::error!(target: "dyj.dfu", "❌ {}s 内没有发现 {}", DFU_DISCOVERY_TIMEOUT.as_secs(), DFU_ADVERTISED_NAME);
                return Err(SdkError::DeviceNotFound(format!(
                    "{DFU_ADVERTISED_NAME} (bootloader did not start advertising within {}s)",
                    DFU_DISCOVERY_TIMEOUT.as_secs()
                )));
            }
        };
        let dfu = self.connect_dfu(&target).await?;
        let report = dfu.upload(package, options, progress).await;
        // bootloader 校验后会自己复位断链；这里只是尽力释放本端。
        let _ = dfu.disconnect().await;
        report
    }
}

fn find_dfu_characteristics(
    characteristics: &std::collections::BTreeSet<Characteristic>,
) -> Result<(Characteristic, Characteristic)> {
    let control_uuid = parse_uuid(DFU_CONTROL_POINT_UUID)?;
    let packet_uuid = parse_uuid(DFU_PACKET_UUID)?;
    let control = characteristics
        .iter()
        .find(|characteristic| {
            characteristic.uuid == control_uuid
                && characteristic.properties.contains(CharPropFlags::NOTIFY)
        })
        .cloned()
        .ok_or(SdkError::MissingCharacteristic("DFU control point"))?;
    let packet = characteristics
        .iter()
        .find(|characteristic| {
            characteristic.uuid == packet_uuid
                && characteristic
                    .properties
                    .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        })
        .cloned()
        .ok_or(SdkError::MissingCharacteristic("DFU packet"))?;
    Ok((control, packet))
}

struct BtleplugDfuTransport {
    peripheral: Peripheral,
    control: Characteristic,
    packet: Characteristic,
}

#[async_trait]
impl DfuTransport for BtleplugDfuTransport {
    async fn write_control(&self, bytes: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.control, bytes, WriteType::WithResponse)
            .await
            .map_err(transport_error)
    }

    async fn write_packet(&self, bytes: &[u8]) -> Result<()> {
        self.peripheral
            .write(&self.packet, bytes, WriteType::WithoutResponse)
            .await
            .map_err(transport_error)
    }

    async fn notifications(&self) -> Result<NotificationStream> {
        let control_uuid = self.control.uuid;
        let stream = self
            .peripheral
            .notifications()
            .await
            .map_err(transport_error)?;
        Ok(Box::pin(
            stream
                .filter(move |notification| {
                    futures::future::ready(notification.uuid == control_uuid)
                })
                .map(|notification| Ok(notification.value)),
        ))
    }

    async fn disconnect(&self) -> Result<()> {
        if self
            .peripheral
            .is_connected()
            .await
            .map_err(transport_error)?
        {
            self.peripheral
                .disconnect()
                .await
                .map_err(transport_error)?;
            tracing::info!(target: "dyj.dfu", "🔌 已断开 DFU 目标");
        }
        Ok(())
    }
}

struct BtleplugTransport {
    peripheral: Peripheral,
    write: btleplug::api::Characteristic,
}

#[async_trait]
impl BleTransport for BtleplugTransport {
    async fn write(&self, bytes: &[u8]) -> Result<()> {
        tracing::trace!(target: "dyj.ble", "🔬 写入 {}字节: {:02x?}", bytes.len(), bytes);
        self.peripheral
            .write(&self.write, bytes, WriteType::WithResponse)
            .await
            .map_err(transport_error)
    }

    async fn notifications(&self) -> Result<NotificationStream> {
        let stream = self
            .peripheral
            .notifications()
            .await
            .map_err(transport_error)?;
        Ok(Box::pin(stream.map(|notification| Ok(notification.value))))
    }

    async fn disconnect(&self) -> Result<()> {
        if self
            .peripheral
            .is_connected()
            .await
            .map_err(transport_error)?
        {
            self.peripheral
                .disconnect()
                .await
                .map_err(transport_error)?;
            tracing::info!(target: "dyj.ble", "🔌 已主动断开连接");
        }
        Ok(())
    }
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .map_err(|error| SdkError::Transport(format!("invalid SDK UUID `{value}`: {error}")))
}

fn transport_error(error: impl std::fmt::Display) -> SdkError {
    SdkError::Transport(error.to_string())
}
