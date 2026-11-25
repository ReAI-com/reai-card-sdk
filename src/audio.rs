//! 实时音频：IMA ADPCM 解码与 PCM 数据块。
//!
//! 固件链路（`nrf52840dk/src/lib/record_lib.c` / `audio/adpcm.c`）：PDM 麦克风 16kHz
//! 16bit 单声道 → 标准 IMA ADPCM（4bit，4:1）。实时模式下 ADPCM 流以 `0x00` 前缀的
//! 二进制 BLE 通知持续发出（≈8KB/s），解码状态跨整段录音连续，仅在录音开始时复位。
//! 本模块在主机端做同样的解码，把字节流还原为 PCM 样本，也可用于离线转换取回的
//! `.apm` 文件。

/// 固件录音的固定采样率（Hz）。
pub const SAMPLE_RATE_HZ: u32 = 16_000;
/// 固件录音固定为单声道。
pub const CHANNELS: u8 = 1;

/// 一段解码后的 PCM 数据块。
#[derive(Debug, Clone, PartialEq)]
pub struct PcmChunk {
    /// 16bit 有符号单声道样本，`sample_rate` 为 16000。
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u8,
    /// true 表示此前检测到数据丢失（BLE 通知积压被丢弃），解码流出现过断裂，
    /// 断裂点附近会有短暂杂音直至解码状态重新收敛。
    pub discontinuous: bool,
}

impl PcmChunk {
    /// 时长（秒）。
    pub fn duration_secs(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate as f64
    }
}

/// 标准 IMA ADPCM 索引调整表（与固件 `adpcm.c` 的 `s_index_table` 一致）。
const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// 标准 IMA ADPCM 步长表（与固件 `adpcm.c` 的 `s_step_table` 一致）。
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// 连续 IMA ADPCM 流的解码器。
///
/// 状态（预测样本 + 步长索引）在 [`Self::decode_chunk`] 调用之间保持，对应固件在
/// 整段录音期间不复位的行为。一段录音对应一个解码器；新录音开始时重新构造。
#[derive(Debug, Clone)]
pub struct AdpcmDecoder {
    predictor: i16,
    index: i32,
}

impl Default for AdpcmDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AdpcmDecoder {
    /// 新解码器，状态与固件 `adpcm_reset()` 一致（predictor=0, index=0）。
    pub fn new() -> Self {
        Self {
            predictor: 0,
            index: 0,
        }
    }

    /// 解码一段 ADPCM 字节，样本追加到 `out`。
    ///
    /// 字节内低位 nibble 是先编码的样本（固件 `adpcm_decode_buffer` 的取法）。
    pub fn decode_chunk(&mut self, adpcm: &[u8], out: &mut Vec<i16>) {
        out.reserve(adpcm.len() * 2);
        for byte in adpcm {
            out.push(self.decode_nibble(byte & 0x0F));
            out.push(self.decode_nibble(byte >> 4));
        }
    }

    /// 解码一段 ADPCM 字节并返回样本。
    pub fn decode(&mut self, adpcm: &[u8]) -> Vec<i16> {
        let mut out = Vec::new();
        self.decode_chunk(adpcm, &mut out);
        out
    }

    fn decode_nibble(&mut self, code: u8) -> i16 {
        let step = STEP_TABLE[self.index as usize];
        let mut diff = step >> 3;
        if code & 0b0100 != 0 {
            diff += step;
        }
        if code & 0b0010 != 0 {
            diff += step >> 1;
        }
        if code & 0b0001 != 0 {
            diff += step >> 2;
        }
        // 固件用 int16 存预测样本并显式限幅，这里以 i32 计算后 clamp 等价。
        let mut predictor = self.predictor as i32;
        if code & 0b1000 != 0 {
            predictor -= diff;
        } else {
            predictor += diff;
        }
        self.predictor = predictor.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.index = (self.index + INDEX_TABLE[code as usize]).clamp(0, 88);
        self.predictor
    }
}

/// 音频预处理与重采样配置。
///
/// 默认值 [`AudioProcessOptions::default()`] 不做重采样也不做归一化，
/// 保持 [`crate::apm_to_wav`] 原有解码行为 100% 不变；
/// 面向 ASR / VAD 预处理时使用 [`AudioProcessOptions::conditioned_for_asr()`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioProcessOptions {
    /// 源采样率（Hz）；`None` 或等于 [`SAMPLE_RATE_HZ`] 时不做重采样。
    /// 例如历史固件（<= 0.3.19）实际采样率约 `15578` Hz，可在此传入以线性重采样到 16kHz。
    pub source_sample_rate_hz: Option<u32>,
    /// 是否执行一阶高通去直流、噪声门控语音增益与软限幅。
    pub remove_dc_and_normalize: bool,
}

impl AudioProcessOptions {
    /// 适用于 ASR / VAD 的默认预处理配置：开启去直流与噪声门控语音增益，不做重采样。
    pub const fn conditioned_for_asr() -> Self {
        Self {
            source_sample_rate_hz: None,
            remove_dc_and_normalize: true,
        }
    }
}

/// 50ms 帧长度（16kHz 下为 800 样本）。
const CONDITION_FRAME_LEN: usize = 800;
/// 一阶高通去直流极点系数（16kHz 下约 12.7 Hz 截止）。
const DC_BLOCK_POLE: f64 = 0.995;
/// 静音门限：P95 RMS 低于此值不放大，避免抬高底噪引发 ASR 幻觉。
const NOISE_GATE_RMS: f64 = 0.00085;
/// 软膝盖上限过渡区上界。
const SOFT_KNEE_HIGH_RMS: f64 = 0.005;
/// 目标 P95 RMS 响度。
const TARGET_P95_RMS: f64 = 0.065;
/// 软膝盖起点最大增益与常规最大增益。
const SOFT_KNEE_MIN_CEILING: f64 = 2.0;
const SOFT_KNEE_MAX_CEILING: f64 = 10.0;
/// 平滑软限幅拐点与余量。
const SOFT_LIMIT_THRESHOLD: f64 = 0.80;
const SOFT_LIMIT_HEADROOM: f64 = 0.18;

/// 对 16kHz PCM 样本执行语音预处理：去直流、噪声门控语音增益与平滑软限幅。
///
/// 1. 一阶高通去直流：`y = (x - prev_x) + 0.995 * y`；
/// 2. 按 50ms 帧（800 样本）计算 RMS 的 P95 分位值；
/// 3. P95 RMS `< 0.00085` 时保持 `1.0x` 不放大；`0.00085..0.005` 间渐进放开增益上限
///    `2.0..10.0` 倍，目标 P95 RMS 为 `0.065`；
/// 4. 幅度超过 `±0.80` 时应用平滑软限幅 `0.80 + 0.18 * (excess / (0.18 + excess))` 并夹回 `i16`。
pub fn condition_speech_samples(samples: &[i16]) -> Vec<i16> {
    if samples.is_empty() {
        return Vec::new();
    }

    const FULL_SCALE: f64 = 32_768.0;
    let mut hp = Vec::with_capacity(samples.len());
    let mut prev_x = f64::from(samples[0]) / FULL_SCALE;
    let mut prev_y = 0.0_f64;
    for &sample in samples {
        let x = f64::from(sample) / FULL_SCALE;
        let y = (x - prev_x) + DC_BLOCK_POLE * prev_y;
        prev_x = x;
        prev_y = y;
        hp.push(y);
    }

    let mut frame_rms: Vec<f64> = hp
        .chunks(CONDITION_FRAME_LEN)
        .map(|frame| {
            let sum_sq: f64 = frame.iter().map(|&v| v * v).sum();
            (sum_sq / frame.len() as f64).sqrt()
        })
        .collect();
    frame_rms.sort_by(f64::total_cmp);
    let p95_idx = (((frame_rms.len() - 1) as f64) * 0.95).round() as usize;
    let p95_rms = frame_rms[p95_idx.min(frame_rms.len() - 1)];

    let gain = if p95_rms < NOISE_GATE_RMS {
        1.0
    } else {
        let ceiling = if p95_rms < SOFT_KNEE_HIGH_RMS {
            let t = (p95_rms - NOISE_GATE_RMS) / (SOFT_KNEE_HIGH_RMS - NOISE_GATE_RMS);
            SOFT_KNEE_MIN_CEILING + t * (SOFT_KNEE_MAX_CEILING - SOFT_KNEE_MIN_CEILING)
        } else {
            SOFT_KNEE_MAX_CEILING
        };
        (TARGET_P95_RMS / p95_rms).clamp(1.0, ceiling)
    };

    hp.into_iter()
        .map(|sample| {
            let scaled = sample * gain;
            let abs = scaled.abs();
            let limited = if abs <= SOFT_LIMIT_THRESHOLD {
                scaled
            } else {
                let excess = abs - SOFT_LIMIT_THRESHOLD;
                let mag = SOFT_LIMIT_THRESHOLD
                    + SOFT_LIMIT_HEADROOM * (excess / (SOFT_LIMIT_HEADROOM + excess));
                scaled.signum() * mag
            };
            (limited * FULL_SCALE)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        })
        .collect()
}

/// 对 `i16` 单声道样本做线性插值重采样。
///
/// 当 `source_hz == 0 || target_hz == 0 || source_hz == target_hz` 时直接返回原样本克隆；
/// 否则按 `out_len = round(len * target_hz / source_hz)` 进行线性插值。
pub fn resample_linear_i16(samples: &[i16], source_hz: u32, target_hz: u32) -> Vec<i16> {
    if samples.is_empty() || source_hz == 0 || target_hz == 0 || source_hz == target_hz {
        return samples.to_vec();
    }

    let out_len =
        ((samples.len() as f64) * f64::from(target_hz) / f64::from(source_hz)).round() as usize;
    if out_len == 0 {
        return Vec::new();
    }
    if samples.len() == 1 {
        return vec![samples[0]; out_len];
    }

    let step = f64::from(source_hz) / f64::from(target_hz);
    let max_idx = samples.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = (i as f64 * step).min(max_idx as f64);
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let s0 = f64::from(samples[idx]);
        let s1 = f64::from(samples[(idx + 1).min(max_idx)]);
        let interpolated = s0 + (s1 - s0) * frac;
        out.push(
            interpolated
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
        );
    }
    out
}

/// 按 [`AudioProcessOptions`] 对 PCM 样本依次执行重采样（到 [`SAMPLE_RATE_HZ`]）与语音预处理。
pub fn process_pcm_samples(samples: &[i16], options: &AudioProcessOptions) -> Vec<i16> {
    let resampled = match options.source_sample_rate_hz {
        Some(source_hz) if source_hz != 0 && source_hz != SAMPLE_RATE_HZ => {
            resample_linear_i16(samples, source_hz, SAMPLE_RATE_HZ)
        }
        _ => samples.to_vec(),
    };
    if options.remove_dc_and_normalize {
        condition_speech_samples(&resampled)
    } else {
        resampled
    }
}

/// 将 PCM 样本封装为 16bit 单声道 WAV 文件字节（44 字节头 + 小端样本）。
///
/// 用于把实时流或离线 `.apm` 解码结果保存成可播放的文件。
pub fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes()); // fmt 块长度
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // 单声道
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // 字节率
    wav.extend_from_slice(&2_u16.to_le_bytes()); // 块对齐
    wav.extend_from_slice(&16_u16.to_le_bytes()); // 位深
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// 供本 crate 测试构造已知明文 ADPCM 流的辅助（device.rs 的 mock 测试也复用）。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// 测试内 IMA 编码器，算法与固件 `adpcm_encode` 逐条一致。
    pub(crate) struct AdpcmEncoder {
        prev: i16,
        index: i32,
    }

    impl AdpcmEncoder {
        pub(crate) fn new() -> Self {
            Self { prev: 0, index: 0 }
        }

        pub(crate) fn encode(&mut self, sample: i16) -> u8 {
            let step = STEP_TABLE[self.index as usize];
            let mut diff = sample as i32 - self.prev as i32;
            let mut code = 0_u8;
            if diff < 0 {
                code = 8;
                diff = -diff;
            }
            if diff >= step {
                code |= 4;
                diff -= step;
            }
            let half = step >> 1;
            if diff >= half {
                code |= 2;
                diff -= half;
            }
            let quarter = half >> 1;
            if diff >= quarter {
                code |= 1;
            }
            // 与固件一致：用旧索引下的步长重建预测样本，再更新索引。
            let mut recon = step >> 3;
            if code & 0b0100 != 0 {
                recon += step;
            }
            if code & 0b0010 != 0 {
                recon += step >> 1;
            }
            if code & 0b0001 != 0 {
                recon += step >> 2;
            }
            let mut predictor = self.prev as i32;
            if code & 0b1000 != 0 {
                predictor -= recon;
            } else {
                predictor += recon;
            }
            self.prev = predictor.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            self.index = (self.index + INDEX_TABLE[code as usize]).clamp(0, 88);
            code
        }

        /// 与固件 `adpcm_encode_buffer` 相同的低 nibble 在前打包。
        pub(crate) fn encode_buffer(&mut self, samples: &[i16]) -> Vec<u8> {
            samples
                .chunks(2)
                .map(|pair| match pair {
                    [low] => self.encode(*low),
                    [low, high] => self.encode(*low) | (self.encode(*high) << 4),
                    _ => unreachable!("chunks(2) never yields empty pairs"),
                })
                .collect()
        }
    }

    /// 16bit 正弦波，幅度不带满以贴近真实麦克风信号。
    pub(crate) fn sine(freq: f64, count: usize) -> Vec<i16> {
        (0..count)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / 16_000.0).sin() * 8_000.0)
            .map(|v| v as i16)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{AdpcmEncoder, sine};
    use super::*;

    #[test]
    fn round_trips_a_sine_within_adpcm_tolerance() {
        let pcm = sine(440.0, 4_000);
        let encoded = AdpcmEncoder::new().encode_buffer(&pcm);
        let decoded = AdpcmDecoder::new().decode(&encoded);
        assert_eq!(decoded.len(), pcm.len());
        // IMA ADPCM 步长索引从 0 冷启动，开头十来个样本追不上陡峭信号（固件解码器同样
        // 存在的固有瞬态），稳态误差才是编解码一致性的度量。
        const WARMUP: usize = 32;
        let max_error = decoded[WARMUP..]
            .iter()
            .zip(&pcm[WARMUP..])
            .map(|(out, reference)| (i32::from(*out) - i32::from(*reference)).abs())
            .max()
            .unwrap();
        assert!(max_error < 1_000, "max error {max_error}");
    }

    #[test]
    fn decodes_low_nibble_before_high_nibble() {
        let mut encoder = AdpcmEncoder::new();
        let pcm = [1_000_i16, -1_000, 2_000, -2_000];
        let encoded = encoder.encode_buffer(&pcm);
        let decoded = AdpcmDecoder::new().decode(&encoded);
        // 逐 nibble 顺序与编码一一对应，解码序列方向必须与输入一致。
        assert_eq!(decoded[0].signum(), 1);
        assert_eq!(decoded[1].signum(), -1);
        assert_eq!(decoded[2].signum(), 1);
        assert_eq!(decoded[3].signum(), -1);
    }

    #[test]
    fn decodes_silence_as_silence() {
        let encoded = AdpcmEncoder::new().encode_buffer(&vec![0_i16; 1_000]);
        let decoded = AdpcmDecoder::new().decode(&encoded);
        assert!(decoded.iter().all(|sample| sample.abs() <= 8));
    }

    #[test]
    fn state_persists_across_chunks_like_the_firmware_stream() {
        let pcm = sine(220.0, 2_000);
        let encoded = AdpcmEncoder::new().encode_buffer(&pcm);
        // 按 20ms 实时块（160 字节 ADPCM）切开分别喂给同一个解码器，
        // 结果必须与一次性解码完全一致——对应固件跨块不复位。
        let mut streamed = AdpcmDecoder::new();
        let mut streamed_out = Vec::new();
        for chunk in encoded.chunks(160) {
            streamed.decode_chunk(chunk, &mut streamed_out);
        }
        let whole = AdpcmDecoder::new().decode(&encoded);
        assert_eq!(streamed_out, whole);
    }

    #[test]
    fn clamps_predictor_on_loud_swing() {
        let mut encoder = AdpcmEncoder::new();
        let loud = sine(20.0, 2_000);
        let encoded = encoder.encode_buffer(&loud);
        let decoded = AdpcmDecoder::new().decode(&encoded);
        assert!(
            decoded
                .iter()
                .all(|sample| (-32_768..=32_767).contains(sample))
        );
    }

    #[test]
    fn wav_header_describes_the_payload() {
        let samples = [1_i16, -1, 2];
        let wav = wav_bytes(&samples, SAMPLE_RATE_HZ);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(&wav[44..46], &1_i16.to_le_bytes());
    }

    #[test]
    fn condition_speech_removes_dc_offset() {
        let input: Vec<i16> = (0..16_000)
            .map(|i| {
                let ac = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16_000.0).sin() * 2_000.0;
                (10_000.0 + ac) as i16
            })
            .collect();
        let out = condition_speech_samples(&input);
        let mean = out.iter().map(|&s| f64::from(s)).sum::<f64>() / out.len() as f64;
        assert!(
            mean.abs() < 20.0,
            "DC offset should be removed, mean={mean}"
        );
    }

    #[test]
    fn condition_speech_does_not_amplify_silence_below_noise_gate() {
        // P95 RMS ~ 5 / 32768 ≈ 0.00015 < 0.00085
        let silence: Vec<i16> = (0..8_000)
            .map(|i| if i % 2 == 0 { 5 } else { -5 })
            .collect();
        let out = condition_speech_samples(&silence);
        let max_abs = out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            max_abs <= 15,
            "silence should not be amplified, max_abs={max_abs}"
        );
    }

    #[test]
    fn condition_speech_lifts_quiet_speech_toward_target_rms() {
        // 幅度 200 的正弦波：RMS ≈ 141.4 / 32768 ≈ 0.00431，处于软膝盖区间内，应被明显抬升
        let quiet: Vec<i16> = (0..16_000)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 300.0 * i as f64 / 16_000.0).sin() * 200.0) as i16
            })
            .collect();
        let in_rms =
            (quiet.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / quiet.len() as f64).sqrt();
        let out = condition_speech_samples(&quiet);
        let out_rms =
            (out.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
        assert!(
            out_rms > in_rms * 5.0,
            "quiet speech should be lifted: in_rms={in_rms}, out_rms={out_rms}"
        );
    }

    #[test]
    fn condition_speech_soft_limits_peaks_without_hard_clipping() {
        // 混合中等持续语音与偶发强峰，触发 >1x 增益同时峰顶超过 0.80 触发软限幅
        let mut samples = vec![0_i16; 16_000];
        for (i, slot) in samples.iter_mut().enumerate() {
            let base = (2.0 * std::f64::consts::PI * 250.0 * i as f64 / 16_000.0).sin() * 800.0;
            *slot = if (8_000..8_100).contains(&i) {
                if i % 2 == 0 { 28_000 } else { -28_000 }
            } else {
                base as i16
            };
        }
        let out = condition_speech_samples(&samples);
        let peak = out.iter().map(|&s| s.unsigned_abs()).max().unwrap_or(0);
        // 软限幅渐近上限为 0.98 * 32768 ≈ 32112，绝不硬削波顶满 32767
        assert!(
            peak < 32_200 && peak > 26_200,
            "unexpected soft-limited peak: {peak}"
        );
    }

    #[test]
    fn resamples_historical_15578hz_to_16000hz_with_exact_duration_ratio() {
        // 10 秒真实时间在 15578 Hz 下有 155_780 样本，重采样到 16kHz 应精确为 160_000 样本
        let input: Vec<i16> = (0..155_780)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 440.0 * i as f64 / 15_578.0).sin() * 4_000.0) as i16
            })
            .collect();
        let out = resample_linear_i16(&input, 15_578, 16_000);
        assert_eq!(out.len(), 160_000);

        // 零采样率或同采样率直接返回克隆
        let small = vec![10_i16, -20, 30];
        assert_eq!(resample_linear_i16(&small, 16_000, 16_000), small);
        assert_eq!(resample_linear_i16(&small, 0, 16_000), small);
        assert_eq!(resample_linear_i16(&small, 16_000, 0), small);

        // process_pcm_samples 联合流程
        let opts = AudioProcessOptions {
            source_sample_rate_hz: Some(15_578),
            remove_dc_and_normalize: true,
        };
        let processed = process_pcm_samples(&input, &opts);
        assert_eq!(processed.len(), 160_000);
    }
}
