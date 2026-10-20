use tokio::io::{AsyncWrite, AsyncWriteExt};

use futures::StreamExt;
use reqwest::{
    Client, Url,
    header::{CONTENT_DISPOSITION, CONTENT_TYPE},
};
use serde_json::Value;

use std::path::Path;

use crate::{
    Result, SdkError, recording,
    recording::SavedRecording,
    types::{
        AccessPointDeviceInfo, AccessPointDownloadedFile, AccessPointFilePage, AccessPointInfo,
    },
};

/// HTTP client for the ESP32 file service reachable while the device AP is active.
///
/// This client is intentionally separate from `DyjDevice`: it is used only after the host has
/// joined the device's WiFi access point and BLE has reported an active AP/web server.
#[derive(Clone)]
pub struct AccessPointClient {
    client: Client,
    base_url: Url,
}

impl AccessPointClient {
    pub fn new(ip: &str, port: u16) -> Result<Self> {
        let base_url = Url::parse(&format!("http://{ip}:{port}/"))
            .map_err(|error| SdkError::InvalidFrame(format!("invalid AP endpoint: {error}")))?;
        // 电脑没加入设备热点时 192.168.4.1 不可达，TCP 连接默认要挂到系统超时（macOS ~75s）；
        // 只限制建连阶段，下载本身可以很长。
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        tracing::info!(target: "dyj.ap", "🌐 AP快传客户端就绪 base={base_url}");
        Ok(Self { client, base_url })
    }

    pub fn from_info(info: &AccessPointInfo) -> Result<Self> {
        let ip = info
            .ip
            .as_deref()
            .ok_or_else(|| SdkError::InvalidFrame("AP status omitted its IP address".into()))?;
        Self::new(ip, info.web_port.unwrap_or(80))
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn device_info(&self) -> Result<AccessPointDeviceInfo> {
        self.client
            .get(self.endpoint("api/device/info")?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(Into::into)
    }

    pub async fn list_files(&self, page: u32, page_size: u32) -> Result<AccessPointFilePage> {
        self.client
            .get(self.endpoint("api/files")?)
            .query(&[("page", page), ("pageSize", page_size)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(Into::into)
    }

    /// Streams an AP-hosted file to `writer`; this HTTP path does not delete the source file.
    ///
    /// 固件下载接口带目录遍历防护：`path` 以 `/` 开头（或含 `..`）会被 403 拒绝，
    /// 服务端自行拼接 SD 卡根路径，因此这里发送前剥掉前导斜杠。
    pub async fn download_file_to<W: AsyncWrite + Unpin>(
        &self,
        path: &str,
        writer: &mut W,
    ) -> Result<AccessPointDownloadedFile> {
        let remote = normalize_remote_path(path);
        tracing::info!(target: "dyj.ap", "🌐 下载文件 path={remote}");
        let response = self
            .client
            .get(self.endpoint("api/files/download")?)
            .query(&[("path", remote)])
            .send()
            .await?
            .error_for_status()?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let suggested_name = response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(disposition_filename);

        let mut written = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            writer.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        writer.flush().await?;
        tracing::info!(target: "dyj.ap", "✅ 下载完成 path={remote} 共{written}字节");

        Ok(AccessPointDownloadedFile {
            requested_path: path.to_owned(),
            bytes_written: written,
            content_type,
            suggested_name,
        })
    }

    /// 下载到目录：先写 `<dir>/<原名>.part`，成功后按固件时间戳名改成
    /// `YYYYMMDD-HHMMSS_<原编号>.apm`（见 [`crate::recording_file_name`]），失败清掉半截文件。
    /// 返回的 [`SavedRecording::to_wav`] 可直接转成可播放的 WAV。
    pub async fn download_to_dir(
        &self,
        path: &str,
        dir: impl AsRef<Path>,
    ) -> Result<SavedRecording> {
        let dir = dir.as_ref();
        tokio::fs::create_dir_all(dir).await?;
        let original = recording::base_name(path).to_owned();
        let part = recording::part_path(dir, &original);
        let mut file = tokio::fs::File::create(&part).await?;
        let result = match self.download_file_to(path, &mut file).await {
            Ok(result) => result,
            Err(error) => {
                drop(file);
                recording::discard_part(&part).await;
                return Err(error);
            }
        };
        drop(file);
        recording::finalize(
            dir,
            &part,
            &original,
            result.suggested_name.as_deref(),
            result.bytes_written,
        )
        .await
    }

    /// Deletes an AP-hosted file; `path` 接受带或不带前导 `/`（见
    /// [`Self::download_file_to`] 的固件防护说明）。
    pub async fn delete_file(&self, path: &str) -> Result<Value> {
        let remote = normalize_remote_path(path);
        tracing::info!(target: "dyj.ap", "🌐 删除文件 path={remote}");
        self.client
            .delete(self.endpoint("api/files/delete")?)
            .query(&[("path", remote)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(Into::into)
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path)
            .map_err(|error| SdkError::InvalidFrame(format!("invalid AP API path: {error}")))
    }
}

/// 固件下载/删除接口拒绝前导 `/` 与 `..`（目录遍历防护，服务端自行拼接根路径）。
fn normalize_remote_path(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// 从 `attachment; filename="xxx"` 里取出建议文件名。
fn disposition_filename(header: &str) -> Option<String> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("filename="))
        .map(|name| name.trim_matches('"').to_owned())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use super::AccessPointClient;
    use crate::types::AccessPointInfo;

    #[test]
    fn creates_local_ap_endpoint_from_status() {
        let client = AccessPointClient::from_info(&AccessPointInfo {
            ip: Some("192.168.4.1".into()),
            web_port: Some(80),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(client.base_url().as_str(), "http://192.168.4.1/");
    }

    #[tokio::test]
    async fn lists_files_using_the_firmware_route_and_pagination() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let size = socket.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..size]).unwrap();
            assert!(request.starts_with("GET /api/files?page=2&pageSize=5 HTTP/1.1"));
            let body = r#"{"total":1,"page":2,"pageSize":5,"totalPages":1,"files":[{"name":"A.APM","size":3}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });

        let page = AccessPointClient::new("127.0.0.1", port)
            .unwrap()
            .list_files(2, 5)
            .await
            .unwrap();
        assert_eq!(page.files[0].name, "A.APM");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn download_to_dir_names_by_recording_time_and_converts_to_wav() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"synced_1792392529.apm\"\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd").unwrap();
        });
        let dir = std::env::temp_dir().join(format!(
            "reai-card-sdk-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let saved = AccessPointClient::new("127.0.0.1", port)
            .unwrap()
            .download_to_dir("/059F19DF.APM", &dir)
            .await
            .unwrap();
        server.join().unwrap();

        let file_name = saved.path.file_name().unwrap().to_str().unwrap();
        assert!(file_name.ends_with("_059F19DF.apm"), "{file_name}");
        assert_eq!(saved.original_name, "059F19DF.APM");
        assert_eq!(saved.stamp.unwrap().timestamp, 1792392529);
        assert!(saved.recorded_at.is_some());
        assert_eq!(std::fs::read(&saved.path).unwrap(), b"abcd");
        assert!(!dir.join("059F19DF.APM.part").exists(), ".part 应已改名");

        let wav = saved.to_wav().await.unwrap();
        assert_eq!(wav.samples, 8);
        assert!(
            wav.path
                .ends_with(saved.path.with_extension("wav").file_name().unwrap())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_strips_the_leading_slash_the_firmware_rejects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let size = socket.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..size]).unwrap();
            // 固件对带前导 `/` 的 path 一律 403（目录遍历防护）
            assert!(request.starts_with("GET /api/files/download?path=A.APM HTTP/1.1"));
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"synced_1792392529.apm\"\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").unwrap();
        });

        let mut buffer = Vec::new();
        let result = AccessPointClient::new("127.0.0.1", port)
            .unwrap()
            .download_file_to("/A.APM", &mut buffer)
            .await
            .unwrap();
        assert_eq!(buffer, b"abc");
        assert_eq!(result.requested_path, "/A.APM");
        assert_eq!(
            result.suggested_name.as_deref(),
            Some("synced_1792392529.apm")
        );
        server.join().unwrap();
    }
}
