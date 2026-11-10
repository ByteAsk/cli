//! HFT domain pack: public exchange feed formats decoded into the normalized market
//! events the port_check harness replays, and reproducible fixtures that decode under the shared
//! replay library.
//!
//! - [`itch50`]: Nasdaq TotalView-ITCH 5.0 messages (decode / encode / JSON), written from the
//!   public specification.
//! - [`normalize`]: ITCH messages to harness events (`add`, `exec`, `cancel`, `delete`,
//!   `replace`, `trade`).
//! - [`synthetic`]: an invented, seeded ITCH session used for the checked-in fixture.
//! - [`read_itch_file`]: a length-prefixed capture file (2-byte big-endian length before every
//!   message, as Nasdaq's binary files are laid out) to messages and events.

pub mod itch50;
pub mod normalize;
pub mod synthetic;

use byteask_replay::record::Framing;
use byteask_replay::record::Frames;
use serde_json::Value;
use std::io::Read;

/// Every message of a capture, decoded, with any framing or decoding error naming the frame.
pub fn read_messages<R: Read>(reader: R) -> impl Iterator<Item = Result<itch50::Message, String>> {
    Frames::new(reader, Framing::U16Be).map(|f| match f {
        Err(e) => Err(e.to_string()),
        Ok(frame) => itch50::decode(&frame.bytes)
            .map_err(|e| format!("message {} (byte offset {}): {e}", frame.index, frame.offset)),
    })
}

/// A capture as harness events (messages that do not change the book are skipped).
pub fn read_events<R: Read>(reader: R) -> impl Iterator<Item = Result<Value, String>> {
    let mut norm = normalize::Normalizer::new();
    read_messages(reader).filter_map(move |m| match m {
        Err(e) => Some(Err(e)),
        Ok(msg) => norm.push(&msg).transpose(),
    })
}

#[cfg(test)]
mod tests;
