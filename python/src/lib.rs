use std::{path::Path, sync::OnceLock, time::Duration};

use pyo3::{
    exceptions::{PyIOError, PyRuntimeError, PyValueError},
    prelude::*,
    types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString},
};
use reai_card_sdk::{
    AP_DEFAULT_IP, AP_DEFAULT_PORT, AccessPointClient, AudioProcessOptions, DeviceManager,
    DyjDevice, FileEntry, SAMPLE_RATE_HZ, SavedRecording, SdkError,
    apm_to_wav_with_options as sdk_apm_to_wav_with_options,
    reconstruct_session_timeline as sdk_reconstruct_session_timeline,
};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::runtime::{Builder, Runtime};

fn runtime() -> PyResult<&'static Runtime> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    match RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(rt) => Ok(rt),
        Err(error) => Err(PyRuntimeError::new_err(format!(
            "failed to initialize Tokio runtime: {error}"
        ))),
    }
}

fn sdk_err(error: SdkError) -> PyErr {
    match error {
        SdkError::Io(io_err) => PyIOError::new_err(io_err.to_string()),
        SdkError::InvalidCommand(msg) => PyValueError::new_err(msg),
        other => PyRuntimeError::new_err(other.to_string()),
    }
}

fn json_to_py(py: Python<'_>, value: &Value) -> PyResult<PyObject> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(v) => PyBool::new(py, *v).to_owned().into_any().unbind(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.into_any().unbind()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py)?.into_any().unbind()
            } else {
                n.as_f64()
                    .unwrap_or(0.0)
                    .into_pyobject(py)?
                    .into_any()
                    .unbind()
            }
        }
        Value::String(s) => PyString::new(py, s).into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

fn py_to_json(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.downcast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if let Ok(i) = obj.downcast::<PyInt>() {
        return Ok(Value::Number(i.extract::<i64>()?.into()));
    }
    if let Ok(f) = obj.downcast::<PyFloat>() {
        let num = serde_json::Number::from_f64(f.value())
            .ok_or_else(|| PyValueError::new_err("non-finite float cannot be serialized"))?;
        return Ok(Value::Number(num));
    }
    if let Ok(s) = obj.downcast::<PyString>() {
        return Ok(Value::String(s.to_str()?.to_owned()));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut items = Vec::with_capacity(list.len());
        for item in list.iter() {
            items.push(py_to_json(&item)?);
        }
        return Ok(Value::Array(items));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = Map::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_json(&v)?);
        }
        return Ok(Value::Object(map));
    }
    Err(PyValueError::new_err(format!(
        "unsupported Python type for JSON conversion: {}",
        obj.get_type().name()?
    )))
}

fn serialize_to_py<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<PyObject> {
    let json = serde_json::to_value(value).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    json_to_py(py, &json)
}

fn saved_recording_to_py(py: Python<'_>, saved: &SavedRecording) -> PyResult<PyObject> {
    let json = serde_json::json!({
        "path": saved.path,
        "bytes_written": saved.bytes_written,
        "original_name": saved.original_name,
        "device_name": saved.device_name,
        "timestamp": saved.stamp.map(|s| s.timestamp),
        "synced": saved.stamp.map(|s| s.synced),
        "recorded_at": saved.recorded_at.map(|dt| dt.to_rfc3339()),
    });
    json_to_py(py, &json)
}

fn duration_from_secs(seconds: f64) -> PyResult<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(PyValueError::new_err(
            "duration seconds must be a finite non-negative number",
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// 扫描附近广播中的 DYJ 设备，返回字典列表（id / name / rssi）。
#[pyfunction]
#[pyo3(signature = (seconds = 5.0))]
fn scan(py: Python<'_>, seconds: f64) -> PyResult<PyObject> {
    let duration = duration_from_secs(seconds)?;
    let rt = runtime()?;
    let devices = py
        .allow_threads(|| {
            rt.block_on(async move {
                let manager = DeviceManager::new().await?;
                manager.scan(duration).await
            })
        })
        .map_err(sdk_err)?;
    serialize_to_py(py, &devices)
}

/// 扫描并连接 DYJ 设备（`device_id` 为 `None` 时连第一台发现的设备）。
#[pyfunction]
#[pyo3(signature = (device_id = None, scan_seconds = 5.0))]
fn connect(py: Python<'_>, device_id: Option<&str>, scan_seconds: f64) -> PyResult<PyCardDevice> {
    let duration = duration_from_secs(scan_seconds)?;
    let target_id = device_id.map(str::to_owned);
    let rt = runtime()?;
    let device = py
        .allow_threads(|| {
            rt.block_on(async move {
                let manager = DeviceManager::new().await?;
                let devices = manager.scan(duration).await?;
                let discovered = match target_id {
                    Some(ref id) => devices
                        .into_iter()
                        .find(|d| &d.id == id)
                        .ok_or_else(|| SdkError::DeviceNotFound(id.clone()))?,
                    None => devices
                        .into_iter()
                        .next()
                        .ok_or_else(|| SdkError::DeviceNotFound(String::new()))?,
                };
                manager.connect(&discovered).await
            })
        })
        .map_err(sdk_err)?;
    Ok(PyCardDevice { inner: device })
}

/// 将离线 `.apm`（IMA ADPCM）解码为 16kHz 单声道 WAV，可选开启语音调理与源采样率重采样。
#[pyfunction]
#[pyo3(signature = (apm_path, wav_path, condition_speech = false, source_sample_rate_hz = None))]
fn apm_to_wav(
    py: Python<'_>,
    apm_path: &str,
    wav_path: &str,
    condition_speech: bool,
    source_sample_rate_hz: Option<u32>,
) -> PyResult<PyObject> {
    let rt = runtime()?;
    let apm = apm_path.to_owned();
    let wav = wav_path.to_owned();
    let options = AudioProcessOptions {
        source_sample_rate_hz,
        remove_dc_and_normalize: condition_speech,
    };
    let wav_file = py
        .allow_threads(|| {
            rt.block_on(async move {
                sdk_apm_to_wav_with_options(Path::new(&apm), Path::new(&wav), &options).await
            })
        })
        .map_err(sdk_err)?;
    let json = serde_json::json!({
        "path": wav_file.path,
        "samples": wav_file.samples,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "duration_secs": wav_file.duration_secs(),
        "condition_speech": condition_speech,
        "source_sample_rate_hz": source_sample_rate_hz,
    });
    json_to_py(py, &json)
}

/// 按录音会话重建时间轴（修复旧固件 24-bit RTC 1024s 回绕导致的长会议时间戳折叠）。
#[pyfunction]
fn reconstruct_session_timeline(py: Python<'_>, entries: &Bound<'_, PyAny>) -> PyResult<PyObject> {
    let raw = py_to_json(entries)?;
    let parsed: Vec<FileEntry> = serde_json::from_value(raw)
        .map_err(|error| PyValueError::new_err(format!("invalid file entries: {error}")))?;
    let rebuilt = sdk_reconstruct_session_timeline(&parsed);
    serialize_to_py(py, &rebuilt)
}

#[pyclass(name = "CardDevice")]
pub struct PyCardDevice {
    inner: DyjDevice,
}

#[pymethods]
impl PyCardDevice {
    fn disconnect(&self, py: Python<'_>) -> PyResult<()> {
        let rt = runtime()?;
        let device = self.inner.clone();
        py.allow_threads(|| rt.block_on(async move { device.disconnect().await }))
            .map_err(sdk_err)
    }

    fn device_info(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let info = py
            .allow_threads(|| rt.block_on(async move { device.device_info().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &info)
    }

    fn state(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let state = py
            .allow_threads(|| rt.block_on(async move { device.state().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &state)
    }

    fn battery(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let battery = py
            .allow_threads(|| rt.block_on(async move { device.battery().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &battery)
    }

    fn storage(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let storage = py
            .allow_threads(|| rt.block_on(async move { device.storage().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &storage)
    }

    #[pyo3(signature = (timestamp = None))]
    fn sync_time(&self, py: Python<'_>, timestamp: Option<i64>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let resp = py
            .allow_threads(|| {
                rt.block_on(async move {
                    match timestamp {
                        Some(ts) => device.sync_time(ts).await,
                        None => device.sync_time_now().await,
                    }
                })
            })
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    #[pyo3(signature = (folder = None))]
    fn list_files(&self, py: Python<'_>, folder: Option<&str>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let folder = folder.map(str::to_owned);
        let files = py
            .allow_threads(|| {
                rt.block_on(async move { device.list_files(folder.as_deref()).await })
            })
            .map_err(sdk_err)?;
        serialize_to_py(py, &files)
    }

    fn take_file_to_dir(&self, py: Python<'_>, name: &str, dir_path: &str) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let name = name.to_owned();
        let dir = dir_path.to_owned();
        let saved = py
            .allow_threads(|| rt.block_on(async move { device.take_file_to_dir(&name, dir).await }))
            .map_err(sdk_err)?;
        saved_recording_to_py(py, &saved)
    }

    fn take_oldest_file_to_dir(&self, py: Python<'_>, dir_path: &str) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let dir = dir_path.to_owned();
        let saved = py
            .allow_threads(|| rt.block_on(async move { device.take_oldest_file_to_dir(dir).await }))
            .map_err(sdk_err)?;
        saved_recording_to_py(py, &saved)
    }

    fn mic_gain(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let rt = runtime()?;
        let device = self.inner.clone();
        py.allow_threads(|| rt.block_on(async move { device.mic_gain().await }))
            .map_err(sdk_err)
    }

    fn set_mic_gain(&self, py: Python<'_>, mode: &str) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let mode = mode.to_owned();
        let resp = py
            .allow_threads(|| rt.block_on(async move { device.set_mic_gain(&mode).await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    fn silence_stop_minutes(&self, py: Python<'_>) -> PyResult<Option<u32>> {
        let rt = runtime()?;
        let device = self.inner.clone();
        py.allow_threads(|| rt.block_on(async move { device.silence_stop_minutes().await }))
            .map_err(sdk_err)
    }

    fn set_silence_stop_minutes(&self, py: Python<'_>, minutes: u32) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let resp = py
            .allow_threads(|| {
                rt.block_on(async move { device.set_silence_stop_minutes(minutes).await })
            })
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    fn local_only(&self, py: Python<'_>) -> PyResult<Option<bool>> {
        let rt = runtime()?;
        let device = self.inner.clone();
        py.allow_threads(|| rt.block_on(async move { device.local_only().await }))
            .map_err(sdk_err)
    }

    fn set_local_only(&self, py: Python<'_>, enabled: bool) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let resp = py
            .allow_threads(|| rt.block_on(async move { device.set_local_only(enabled).await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    #[pyo3(signature = (ssid = None, password = None))]
    fn start_access_point(
        &self,
        py: Python<'_>,
        ssid: Option<&str>,
        password: Option<&str>,
    ) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let ssid = ssid.map(str::to_owned);
        let password = password.map(str::to_owned);
        let resp = py
            .allow_threads(|| {
                rt.block_on(async move {
                    device
                        .start_access_point(ssid.as_deref(), password.as_deref())
                        .await
                })
            })
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    fn stop_access_point(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let resp = py
            .allow_threads(|| rt.block_on(async move { device.stop_access_point().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &resp)
    }

    fn access_point_client(&self, py: Python<'_>) -> PyResult<PyAccessPointClient> {
        let rt = runtime()?;
        let device = self.inner.clone();
        let client = py
            .allow_threads(|| rt.block_on(async move { device.access_point_client().await }))
            .map_err(sdk_err)?;
        Ok(PyAccessPointClient { inner: client })
    }
}

#[pyclass(name = "AccessPointClient")]
pub struct PyAccessPointClient {
    inner: AccessPointClient,
}

#[pymethods]
impl PyAccessPointClient {
    #[new]
    #[pyo3(signature = (ip = AP_DEFAULT_IP, port = AP_DEFAULT_PORT))]
    fn new(ip: &str, port: u16) -> PyResult<Self> {
        let inner = AccessPointClient::new(ip, port).map_err(sdk_err)?;
        Ok(Self { inner })
    }

    fn device_info(&self, py: Python<'_>) -> PyResult<PyObject> {
        let rt = runtime()?;
        let client = self.inner.clone();
        let info = py
            .allow_threads(|| rt.block_on(async move { client.device_info().await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &info)
    }

    #[pyo3(signature = (page = 1, page_size = 50))]
    fn list_files(&self, py: Python<'_>, page: u32, page_size: u32) -> PyResult<PyObject> {
        let rt = runtime()?;
        let client = self.inner.clone();
        let files = py
            .allow_threads(|| rt.block_on(async move { client.list_files(page, page_size).await }))
            .map_err(sdk_err)?;
        serialize_to_py(py, &files)
    }

    fn download_to_dir(
        &self,
        py: Python<'_>,
        remote_path: &str,
        dir_path: &str,
    ) -> PyResult<PyObject> {
        let rt = runtime()?;
        let client = self.inner.clone();
        let path = remote_path.to_owned();
        let dir = dir_path.to_owned();
        let saved = py
            .allow_threads(|| rt.block_on(async move { client.download_to_dir(&path, dir).await }))
            .map_err(sdk_err)?;
        saved_recording_to_py(py, &saved)
    }

    fn delete_file(&self, py: Python<'_>, remote_path: &str) -> PyResult<PyObject> {
        let rt = runtime()?;
        let client = self.inner.clone();
        let path = remote_path.to_owned();
        let resp = py
            .allow_threads(|| rt.block_on(async move { client.delete_file(&path).await }))
            .map_err(sdk_err)?;
        json_to_py(py, &resp)
    }
}

#[pymodule]
fn reai_card(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add_function(wrap_pyfunction!(apm_to_wav, m)?)?;
    m.add_function(wrap_pyfunction!(reconstruct_session_timeline, m)?)?;
    m.add_class::<PyCardDevice>()?;
    m.add_class::<PyAccessPointClient>()?;
    Ok(())
}
