use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use btleplug::{
    api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Adapter, Manager, Peripheral},
};
use futures::StreamExt;
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    DyjDevice, NUS_NOTIFY_UUID, NUS_SERVICE_UUID, NUS_WRITE_UUID, Result, SdkError,
    transport::{BleTransport, NotificationStream, SharedTransport},
    types::DiscoveredDevice,
};

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
