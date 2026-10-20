use std::str;

use serde_json::Value;

use crate::{Result, SdkError};

pub(crate) const COMMAND_TERMINATOR: &[u8] = b"\r\n";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextFrame {
    pub command: String,
    pub payload: Value,
}

/// 解码器吐出的一帧。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Frame {
    /// 常规 `COMMAND[:json]\r\n` 行。
    Text(TextFrame),
    /// 无命令封装的裸 JSON 对象。固件电源管理（关机倒计时等）直接把
    /// `{"type":...}` 写到 BLE 上，既无命令名也无换行，只能按 JSON 边界切。
    Raw(Value),
}

#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
}

enum RawTake {
    Complete(Value),
    Incomplete,
    Malformed(String),
}

impl FrameDecoder {
    /// Feeds raw bytes and returns the decoded frames together with the first malformed-line
    /// error, if any. A malformed line is skipped without discarding the well-formed frames
    /// decoded before or after it in the same batch.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> (Vec<Frame>, Option<String>) {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut fault: Option<String> = None;

        loop {
            let blank = self
                .buffer
                .iter()
                .take_while(|byte| matches!(byte, b'\r' | b'\n'))
                .count();
            if blank > 0 {
                self.buffer.drain(..blank);
            }
            match self.buffer.first() {
                None => break,
                // 协议命令名只含大写字母和下划线，行首 `{` 一定是裸 JSON。
                Some(b'{') => match self.take_raw_object() {
                    RawTake::Complete(value) => frames.push(Frame::Raw(value)),
                    RawTake::Incomplete => break,
                    RawTake::Malformed(message) => {
                        fault.get_or_insert(message);
                    }
                },
                Some(_) => {
                    let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') else {
                        break;
                    };
                    let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                    while matches!(line.last(), Some(b'\n' | b'\r')) {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    match parse_text_frame(&line) {
                        Ok(frame) => frames.push(Frame::Text(frame)),
                        Err(error) => {
                            fault.get_or_insert(error.to_string());
                        }
                    }
                }
            }
        }

        (frames, fault)
    }

    /// 从缓冲区头部切出一个完整的 JSON 对象；裸 JSON 没有换行，只能靠 JSON 自身
    /// 的边界判断是否收全，收全前的字节一直留在缓冲区里等待。
    fn take_raw_object(&mut self) -> RawTake {
        let outcome = {
            let mut stream =
                serde_json::Deserializer::from_slice(&self.buffer).into_iter::<Value>();
            match stream.next() {
                Some(Ok(value)) => Ok((value, stream.byte_offset())),
                Some(Err(error)) => Err(Some(error)),
                None => Err(None),
            }
        };
        match outcome {
            Ok((value, used)) => {
                self.buffer.drain(..used);
                RawTake::Complete(value)
            }
            Err(None) => RawTake::Incomplete,
            Err(Some(error)) if error.is_eof() => RawTake::Incomplete,
            Err(Some(error)) => {
                // 坏掉的裸 JSON 无法确定边界：丢到下一个换行（没有就整段丢）重新同步。
                match self.buffer.iter().position(|byte| *byte == b'\n') {
                    Some(end) => {
                        self.buffer.drain(..=end);
                    }
                    None => self.buffer.clear(),
                }
                RawTake::Malformed(format!("malformed raw JSON: {error}"))
            }
        }
    }
}

pub(crate) fn encode_command(command: &str, payload: Option<&Value>) -> Result<Vec<u8>> {
    let command = command.trim();
    if command.is_empty()
        || !command
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    {
        return Err(SdkError::InvalidCommand(command.to_owned()));
    }

    let mut result = command.as_bytes().to_vec();
    if let Some(payload) = payload {
        result.push(b':');
        result.extend_from_slice(serde_json::to_string(payload)?.as_bytes());
    }
    result.extend_from_slice(COMMAND_TERMINATOR);
    Ok(result)
}

pub(crate) fn parse_text_frame(line: &[u8]) -> Result<TextFrame> {
    let text = str::from_utf8(line).map_err(|_| SdkError::InvalidUtf8)?;
    let (command, payload) = match text.split_once(':') {
        Some((command, json)) => (command.trim(), serde_json::from_str(json.trim())?),
        None => (text.trim(), Value::Object(Default::default())),
    };
    if command.is_empty() {
        return Err(SdkError::InvalidFrame(text.to_owned()));
    }
    Ok(TextFrame {
        command: command.to_ascii_uppercase(),
        payload,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Frame, FrameDecoder, TextFrame, encode_command};

    fn text(frame: &Frame) -> &TextFrame {
        match frame {
            Frame::Text(frame) => frame,
            Frame::Raw(value) => panic!("expected text frame, got raw {value}"),
        }
    }

    #[test]
    fn command_uses_crlf_and_json() {
        assert_eq!(
            encode_command("SYNC_TIME", Some(&json!({"timestamp": 1}))).unwrap(),
            b"SYNC_TIME:{\"timestamp\":1}\r\n"
        );
    }

    #[test]
    fn decoder_accepts_fragmented_and_joined_frames() {
        let mut decoder = FrameDecoder::default();
        let (frames, fault) = decoder.push(b"BASE_INFO:{\"version\":\"");
        assert!(frames.is_empty());
        assert!(fault.is_none());
        let (frames, fault) = decoder.push(b"1\"}\r\nBATTERY:{\"level\":80}\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(text(&frames[0]).command, "BASE_INFO");
        assert_eq!(text(&frames[1]).payload["level"], 80);
        assert!(fault.is_none());
    }

    #[test]
    fn decoder_skips_bad_lines_but_keeps_good_frames() {
        let mut decoder = FrameDecoder::default();
        let (frames, fault) =
            decoder.push(b"BATTERY:{\"level\":80}\nBASE_INFO:{oops}\nSTATE_INFO:{}\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(text(&frames[0]).command, "BATTERY");
        assert_eq!(text(&frames[1]).command, "STATE_INFO");
        assert!(fault.is_some());
    }

    #[test]
    fn unframed_json_is_split_from_the_frame_glued_behind_it() {
        // 固件 SHUTDOWN 之后的真实字节流：响应行 + 无换行的裸 JSON；
        // 若设备还没断开又来了一帧，裸 JSON 就和它粘在同一"行"里。
        let mut decoder = FrameDecoder::default();
        let (frames, fault) = decoder.push(
            b"SHUTDOWN:{\"code\":0}\r\n{\"type\":\"shutdown_imminent\",\"seconds\":5}BATTERY:{\"level\":80}\r\n",
        );
        assert!(fault.is_none(), "{fault:?}");
        assert_eq!(frames.len(), 3);
        assert_eq!(text(&frames[0]).command, "SHUTDOWN");
        assert_eq!(
            frames[1],
            Frame::Raw(json!({"type":"shutdown_imminent","seconds":5}))
        );
        assert_eq!(text(&frames[2]).command, "BATTERY");
    }

    #[test]
    fn unframed_json_is_emitted_as_soon_as_it_is_complete() {
        // 没有换行也没有后续帧（关机前最后一条），凑齐 JSON 边界就该吐出来。
        let mut decoder = FrameDecoder::default();
        let (frames, _) = decoder.push(b"{\"type\":\"shutdown_imm");
        assert!(frames.is_empty());
        let (frames, fault) = decoder.push(b"inent\",\"seconds\":5}");
        assert!(fault.is_none());
        assert_eq!(
            frames,
            [Frame::Raw(json!({"type":"shutdown_imminent","seconds":5}))]
        );
        // 缓冲区已清空，后续普通帧照常解析。
        let (frames, _) = decoder.push(b"\r\nSTATE_INFO:{}\r\n");
        assert_eq!(text(&frames[0]).command, "STATE_INFO");
    }

    #[test]
    fn malformed_unframed_json_resyncs_at_the_next_line() {
        let mut decoder = FrameDecoder::default();
        let (frames, fault) = decoder.push(b"{\"type\":oops}garbage\r\nSTATE_INFO:{}\r\n");
        assert!(fault.unwrap().contains("malformed raw JSON"));
        assert_eq!(frames.len(), 1);
        assert_eq!(text(&frames[0]).command, "STATE_INFO");
    }
}
