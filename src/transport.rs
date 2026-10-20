use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures::Stream;

use crate::Result;

pub type NotificationStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>;

/// A narrow BLE transport seam. It permits deterministic protocol tests without Bluetooth hardware.
#[async_trait]
pub trait BleTransport: Send + Sync {
    async fn write(&self, bytes: &[u8]) -> Result<()>;
    async fn notifications(&self) -> Result<NotificationStream>;
    async fn disconnect(&self) -> Result<()>;
}

pub type SharedTransport = Arc<dyn BleTransport>;
