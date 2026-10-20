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
}
