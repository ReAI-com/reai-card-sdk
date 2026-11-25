//! 诊断用：不做服务过滤的原始 BLE 扫描，列出所有有名字的外设。
//! 用法：cargo run --example dyj_raw_scan -- [secs]

use std::time::Duration;

use btleplug::api::{Central, Manager, Peripheral, ScanFilter};

#[tokio::main]
async fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let manager = btleplug::platform::Manager::new().await.unwrap();
    let adapters = manager.adapters().await.unwrap();
    println!("adapters: {}", adapters.len());
    let adapter = adapters.into_iter().next().unwrap();
    adapter.start_scan(ScanFilter::default()).await.unwrap();
    println!("raw scanning {secs}s ...");
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let mut named = 0;
    for p in adapter.peripherals().await.unwrap() {
        if let Ok(Some(props)) = p.properties().await
            && let Some(name) = props.local_name.or(props.advertisement_name)
        {
            println!(
                "{}  name={name:?}  rssi={:?}  tx={:?}",
                props.address, props.rssi, props.tx_power_level
            );
            named += 1;
        }
    }
    println!("total named peripherals: {named}");
}
