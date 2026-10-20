//! OTA 版本信息接口。
//!
//! 产品 OTA 服务（`https://dyj2.dev.reai.com`，nginx 静态托管）用
//! `{api_base}/ota/info?mod=main|wifi` 发布两个模块的最新固件，`api_base` 就是设备
//! KV 里的 `wifi_api_base_url`（所有固件版本内置默认即 [`OTA_DEFAULT_API_BASE_URL`]，
//! 拼上 [`OTA_DEFAULT_INFO_PATH`] 即设备实际请求的地址）。
//!
//! 主控（nRF52840）的版本比较由**上位机**负责——设备端不检查，拿到 `.zip` 后用
//! [`crate::dfu`] 推送；WiFi 模块的比较由 ESP32 自己完成
//! （`DyjDevice::check_wifi_ota`），上位机调这里只是为了展示服务器侧版本。
//! 本模块只提供类型与两次 HTTP 调用，"要不要升级、何时提示"由应用决定。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Result, SdkError};

/// 所有 WiFi 固件版本内置的 `wifi_api_base_url` 默认值（dyj2 OTA 服务，nginx 静态托管）。
pub const OTA_DEFAULT_API_BASE_URL: &str = "https://dyj2.dev.reai.com/api";
/// 固件内置的 `wifi_ota_info_path` 默认值，拼在 API base 之后。
pub const OTA_DEFAULT_INFO_PATH: &str = "/ota/info?mod=wifi";

/// `mod` 查询参数：主控或 WiFi 模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtaModule {
    /// nRF52840 主控（`BASE_INFO.versionCode`）。
    Main,
    /// ESP32-C3 WiFi 模块（`BASE_INFO.wifiVersionCode`）。
    Wifi,
}

impl OtaModule {
    pub const fn query_value(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Wifi => "wifi",
        }
    }
}

impl std::fmt::Display for OtaModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.query_value())
    }
}

/// 服务器发布的最新固件信息。只用 `version_code` 判断新旧；`version` 仅供展示，
/// `url` 仅用于下载（文件名可能与 `version` 不一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtaInfo {
    pub version: String,
    pub version_code: u32,
    pub url: String,
}

impl OtaInfo {
    /// 拉取 `{api_base_url}{info_path}`（默认 `/ota/info?mod=<module>`，与固件
    /// `buildUrl` 的拼接方式一致；接口无鉴权）。
    pub async fn fetch_with_path(api_base_url: &str, info_path: &str) -> Result<Self> {
        let url = format!(
            "{}{}",
            api_base_url.trim_end_matches('/'),
            if info_path.starts_with('/') {
                info_path.to_owned()
            } else {
                format!("/{info_path}")
            }
        );
        reqwest::Url::parse(&url)
            .map_err(|error| SdkError::InvalidCommand(format!("invalid API base URL: {error}")))?;
        tracing::info!(target: "dyj.ota", "🌐 查询 OTA 信息 {url}");
        let info: Self = http_client(Duration::from_secs(15))?
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        tracing::info!(target: "dyj.ota", "🌐 服务器最新 {} (code {}) {}", info.version, info.version_code, info.url);
        Ok(info)
    }

    /// 按固件默认路径拉取 `{api_base_url}/ota/info?mod=<module>`。
    pub async fn fetch(api_base_url: &str, module: OtaModule) -> Result<Self> {
        Self::fetch_with_path(
            api_base_url,
            &format!("/ota/info?mod={}", module.query_value()),
        )
        .await
    }

    /// 服务器版本是否比设备上装的新（只比较 `versionCode`）。
    pub fn is_newer_than(&self, installed_version_code: u32) -> bool {
        self.version_code > installed_version_code
    }

    /// 下载 `url` 指向的固件（主控为 nrfutil `.zip`，WiFi 为 `.bin`）。
    pub async fn download(&self) -> Result<Vec<u8>> {
        tracing::info!(target: "dyj.ota", "⬇️ 下载固件 {}", self.url);
        let bytes = http_client(Duration::from_secs(300))?
            .get(&self.url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        tracing::info!(target: "dyj.ota", "⬇️ 下载完成 {} 字节", bytes.len());
        Ok(bytes.to_vec())
    }
}

fn http_client(total_timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(total_timeout)
        .build()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use super::*;

    fn serve_once(expected_prefix: &'static str, content_type: &'static str, body: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let size = socket.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..size]).unwrap();
            assert!(request.starts_with(expected_prefix), "{request}");
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        port
    }

    #[tokio::test]
    async fn fetches_and_compares_version_codes() {
        let port = serve_once(
            "GET /api/ota/info?mod=main HTTP/1.1",
            "application/json",
            br#"{"version":"0.3.7","versionCode":307,"url":"https://cdn.example.com/app_update_v307.zip"}"#.to_vec(),
        );
        let info = OtaInfo::fetch(&format!("http://127.0.0.1:{port}/api"), OtaModule::Main)
            .await
            .unwrap();
        assert_eq!(info.version, "0.3.7");
        assert!(info.is_newer_than(306));
        assert!(!info.is_newer_than(307));
        assert!(!info.is_newer_than(400));
    }

    #[tokio::test]
    async fn downloads_firmware_bytes() {
        let port = serve_once(
            "GET /fw/wifi_0.0.9.bin HTTP/1.1",
            "application/octet-stream",
            vec![0xE9, 0x01, 0x02, 0x03],
        );
        let info = OtaInfo {
            version: "0.0.9".into(),
            version_code: 9,
            url: format!("http://127.0.0.1:{port}/fw/wifi_0.0.9.bin"),
        };
        assert_eq!(info.download().await.unwrap(), vec![0xE9, 1, 2, 3]);
    }

    #[tokio::test]
    async fn custom_info_path_follows_the_device_config() {
        let port = serve_once(
            "GET /api/v2/ota/wifi.json HTTP/1.1",
            "application/json",
            br#"{"version":"0.0.9","versionCode":9,"url":"https://dyj2.dev.reai.com/ota/fw_wifi/esp32c3wifi.ino.bin"}"#.to_vec(),
        );
        let info =
            OtaInfo::fetch_with_path(&format!("http://127.0.0.1:{port}/api"), "/v2/ota/wifi.json")
                .await
                .unwrap();
        assert_eq!(info.version_code, 9);
        assert_eq!(OTA_DEFAULT_API_BASE_URL, "https://dyj2.dev.reai.com/api");
        assert_eq!(OTA_DEFAULT_INFO_PATH, "/ota/info?mod=wifi");
    }

    #[test]
    fn module_names_match_the_query_parameter() {
        assert_eq!(OtaModule::Main.query_value(), "main");
        assert_eq!(OtaModule::Wifi.to_string(), "wifi");
        assert_eq!(serde_json::to_string(&OtaModule::Wifi).unwrap(), "\"wifi\"");
    }
}
