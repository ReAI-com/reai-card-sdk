use std::str;

use serde_json::Value;

use crate::{Result, SdkError};

pub(crate) const COMMAND_TERMINATOR: &[u8] = b"\r\n";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TextFrame {
    pub command: String,
    pub payload: Value,
}

#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// Feeds raw bytes and returns the decoded frames together with the first malformed-line
    /// error, if any. A malformed line is skipped without discarding the well-formed frames
    /// decoded before or after it in the same batch.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> (Vec<TextFrame>, Option<String>) {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut fault = None;

        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            match parse_text_frame(&line) {
                Ok(frame) => frames.push(frame),
                Err(error) => {
                    if fault.is_none() {
                        fault = Some(error.to_string());
                    }
                }
            }
        }

        (frames, fault)
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

    use super::{FrameDecoder, encode_command};

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
        assert_eq!(frames[0].command, "BASE_INFO");
        assert_eq!(frames[1].payload["level"], 80);
        assert!(fault.is_none());
    }

    #[test]
    fn decoder_skips_bad_lines_but_keeps_good_frames() {
        let mut decoder = FrameDecoder::default();
        let (frames, fault) =
            decoder.push(b"BATTERY:{\"level\":80}\nBASE_INFO:{oops}\nSTATE_INFO:{}\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].command, "BATTERY");
        assert_eq!(frames[1].command, "STATE_INFO");
        assert!(fault.is_some());
    }
}
