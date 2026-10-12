use std::io::{BufRead, Read, Write};

use serde_json::Value;

use crate::protocol::SupervisorError;

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Any synchronous byte stream with a bounded frame reader can back this
/// client. Production currently supplies a Unix domain socket.
pub trait SupervisorTransport: Read + Write {}
impl<T: Read + Write> SupervisorTransport for T {}

pub(crate) fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), SupervisorError> {
    let mut frame = serde_json::to_vec(value)?;
    if frame.len() + 1 > MAX_FRAME_BYTES {
        return Err(SupervisorError::InvalidFrame(
            "request exceeds 1 MiB".into(),
        ));
    }
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame(reader: &mut impl BufRead) -> Result<Option<String>, SupervisorError> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(SupervisorError::InvalidFrame(
                    "connection ended inside a frame".into(),
                ))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if frame.len() + take > MAX_FRAME_BYTES {
            return Err(SupervisorError::InvalidFrame(
                "response exceeds 1 MiB".into(),
            ));
        }
        let complete = available.get(take.wrapping_sub(1)) == Some(&b'\n');
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if complete {
            frame.pop();
            return String::from_utf8(frame)
                .map(Some)
                .map_err(|error| SupervisorError::InvalidFrame(error.to_string()));
        }
    }
}
