# `reai-card` Python 绑定（PyO3 + maturin）

`reai-card` 是 `reai-card-sdk` 的官方 Python 绑定（基于 PyO3 `abi3-py310` 与 maturin），面向 Windows 11 / macOS / Linux 的 Python 3.10+ 数据与 ASR 处理管线。

## 本地构建与安装

前置要求：Python >= 3.10、Rust 工具链、[`maturin`](https://www.maturin.rs/)。

```bash
cd python
pip install maturin
maturin develop --release
```

生成可分发的 wheel（如 Windows x64 / macOS arm64）：

```bash
cd python
maturin build --release
```

## 快速示例

```python
import reai_card

# 1. BLE 扫描与连接
devices = reai_card.scan(seconds=5.0)
print("discovered:", devices)

dev = reai_card.connect(device_id=devices[0]["id"])
print("info:", dev.device_info())
print("battery:", dev.battery())

# 2. 列文件与 1024s 回绕时间轴重建
raw_files = dev.list_files()
timeline = reai_card.reconstruct_session_timeline(raw_files)

# 3. 取回录音并转码 WAV（去直流 + 语音增益调理 + 可选历史采样率重采样）
saved = dev.take_oldest_file_to_dir("./recordings")
wav = reai_card.apm_to_wav(
    saved["path"],
    saved["path"].replace(".apm", ".wav"),
    condition_speech=True,
    source_sample_rate_hz=15578,  # 若 .txt/FileEntry 上报 16000 则可省略或传 16000
)
print("wav:", wav)

# 4. 可选设备隐私与防误录配置
dev.set_mic_gain("meeting")
dev.set_silence_stop_minutes(30)
dev.set_local_only(True)

dev.disconnect()
```

AP 快传模式（主机连上设备热点后）：

```python
ap = reai_card.AccessPointClient("192.168.4.1", 80)
page = ap.list_files(page=1, page_size=50)
for item in page["files"]:
    ap.download_to_dir(item["name"], "./recordings")
```
