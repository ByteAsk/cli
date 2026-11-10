//! Reading recorded inputs and program outputs.
//!
//! Three shapes cover what the callers need:
//! - [`JsonLines`]: one JSON value per line (the normal output format of a replay subject and of
//!   a normalized recording). Blank lines are skipped. The first malformed line ends the stream
//!   with an error that names the line; nothing after it is guessed at.
//! - [`TextLines`]: plain text, one record per line, for programs whose output is not JSON. Each
//!   line becomes a JSON string (without its trailing newline; a `\r` is kept, because "exact"
//!   means exact).
//! - [`Frames`]: length-prefixed binary messages, the layout of exchange feed capture files
//!   (for example Nasdaq ITCH files use a 2-byte big-endian length before every message). A file
//!   that ends inside a frame is an error, never a silently shorter recording.
//!
//! Every reader caps the size of one record so a corrupt or hostile file cannot make the caller
//! allocate without bound.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::fmt;
use std::io;
use std::io::BufRead;
use std::io::Read;
use std::path::Path;

/// A record that could not be read, with where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordError {
    /// 0-based index of the record that failed (the number of good records before it).
    pub index: u64,
    /// "line 17" or "byte offset 1234".
    pub location: String,
    pub message: String,
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "record {} ({}): {}",
            self.index, self.location, self.message
        )
    }
}

impl std::error::Error for RecordError {}

pub type RecordResult = Result<Value, RecordError>;

/// Longest single line or frame accepted by default (16 MiB).
pub const DEFAULT_MAX_RECORD: usize = 16 << 20;

/// Read one line of at most `max` bytes (not counting the newline). Returns the number of bytes
/// read (0 at end of input) and whether the line was longer than `max`.
fn read_capped_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<(usize, bool)> {
    buf.clear();
    let limit = (max as u64).saturating_add(1);
    let n = reader.by_ref().take(limit).read_until(b'\n', buf)?;
    let too_long = buf.len() > max && buf.last() != Some(&b'\n');
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok((n, too_long))
}

/// JSON Lines reader.
pub struct JsonLines<R> {
    reader: R,
    buf: Vec<u8>,
    line: u64,
    index: u64,
    max_line: usize,
    done: bool,
}

impl<R: BufRead> JsonLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            line: 0,
            index: 0,
            max_line: DEFAULT_MAX_RECORD,
            done: false,
        }
    }

    pub fn with_max_line(mut self, max_line: usize) -> Self {
        self.max_line = max_line;
        self
    }

    fn fail(&mut self, message: String) -> Option<RecordResult> {
        self.done = true;
        Some(Err(RecordError {
            index: self.index,
            location: format!("line {}", self.line),
            message,
        }))
    }
}

impl<R: BufRead> Iterator for JsonLines<R> {
    type Item = RecordResult;

    fn next(&mut self) -> Option<RecordResult> {
        if self.done {
            return None;
        }
        loop {
            let (n, too_long) =
                match read_capped_line(&mut self.reader, &mut self.buf, self.max_line) {
                    Ok(r) => r,
                    Err(e) => {
                        self.line += 1;
                        return self.fail(format!("read failed: {e}"));
                    }
                };
            if n == 0 {
                self.done = true;
                return None;
            }
            self.line += 1;
            if too_long {
                return self.fail(format!("line is longer than {} bytes", self.max_line));
            }
            if self.buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return match serde_json::from_slice::<Value>(&self.buf) {
                Ok(v) => {
                    self.index += 1;
                    Some(Ok(v))
                }
                Err(e) => self.fail(format!("not valid JSON: {e}")),
            };
        }
    }
}

/// Plain text reader: one JSON string per line.
pub struct TextLines<R> {
    reader: R,
    buf: Vec<u8>,
    line: u64,
    max_line: usize,
    done: bool,
}

impl<R: BufRead> TextLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            line: 0,
            max_line: DEFAULT_MAX_RECORD,
            done: false,
        }
    }
}

impl<R: BufRead> Iterator for TextLines<R> {
    type Item = RecordResult;

    fn next(&mut self) -> Option<RecordResult> {
        if self.done {
            return None;
        }
        let index = self.line;
        let result = read_capped_line(&mut self.reader, &mut self.buf, self.max_line);
        let fail = |done: &mut bool, message: String| {
            *done = true;
            Some(Err(RecordError {
                index,
                location: format!("line {}", index + 1),
                message,
            }))
        };
        match result {
            Err(e) => fail(&mut self.done, format!("read failed: {e}")),
            Ok((0, _)) => {
                self.done = true;
                None
            }
            Ok((_, true)) => fail(
                &mut self.done,
                format!("line is longer than {} bytes", self.max_line),
            ),
            Ok(_) => {
                self.line += 1;
                match std::str::from_utf8(&self.buf) {
                    Ok(s) => Some(Ok(Value::String(s.to_string()))),
                    Err(e) => fail(&mut self.done, format!("not UTF-8 text: {e}")),
                }
            }
        }
    }
}

/// How binary messages are framed in a capture file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Framing {
    /// 2-byte big-endian length, then the message (Nasdaq ITCH/OUCH capture files).
    U16Be,
    /// 4-byte big-endian length, then the message.
    U32Be,
    /// 4-byte little-endian length, then the message.
    U32Le,
}

impl Framing {
    fn header_len(self) -> usize {
        match self {
            Framing::U16Be => 2,
            Framing::U32Be | Framing::U32Le => 4,
        }
    }

    fn decode_len(self, header: &[u8]) -> u64 {
        match self {
            Framing::U16Be => u64::from(u16::from_be_bytes([header[0], header[1]])),
            Framing::U32Be => u64::from(u32::from_be_bytes([
                header[0], header[1], header[2], header[3],
            ])),
            Framing::U32Le => u64::from(u32::from_le_bytes([
                header[0], header[1], header[2], header[3],
            ])),
        }
    }
}

/// One binary frame and where it started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub index: u64,
    /// Byte offset of the frame's length header in the file.
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// Length-prefixed frame reader.
pub struct Frames<R> {
    reader: R,
    framing: Framing,
    offset: u64,
    index: u64,
    max_frame: usize,
    done: bool,
}

impl<R: Read> Frames<R> {
    pub fn new(reader: R, framing: Framing) -> Self {
        Self {
            reader,
            framing,
            offset: 0,
            index: 0,
            max_frame: DEFAULT_MAX_RECORD,
            done: false,
        }
    }

    fn fail(&mut self, at: u64, message: String) -> Option<Result<Frame, RecordError>> {
        self.done = true;
        Some(Err(RecordError {
            index: self.index,
            location: format!("byte offset {at}"),
            message,
        }))
    }
}

/// Fill `buf` completely; returns how many bytes were read before end of input.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match reader.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

impl<R: Read> Iterator for Frames<R> {
    type Item = Result<Frame, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let start = self.offset;
        let mut header = [0u8; 4];
        let hlen = self.framing.header_len();
        let got = match read_full(&mut self.reader, &mut header[..hlen]) {
            Ok(n) => n,
            Err(e) => return self.fail(start, format!("read failed: {e}")),
        };
        if got == 0 {
            self.done = true;
            return None;
        }
        if got < hlen {
            return self.fail(
                start,
                format!(
                    "file ends inside a frame length ({got} of {hlen} bytes): the capture is truncated"
                ),
            );
        }
        let len = self.framing.decode_len(&header[..hlen]);
        if len > self.max_frame as u64 {
            return self.fail(
                start,
                format!(
                    "frame length {len} is larger than the {} byte limit",
                    self.max_frame
                ),
            );
        }
        let mut bytes = vec![0u8; len as usize];
        let got = match read_full(&mut self.reader, &mut bytes) {
            Ok(n) => n,
            Err(e) => return self.fail(start, format!("read failed: {e}")),
        };
        if got < bytes.len() {
            return self.fail(
                start,
                format!(
                    "file ends inside a frame ({got} of {len} bytes): the capture is truncated"
                ),
            );
        }
        self.offset = start + hlen as u64 + len;
        let index = self.index;
        self.index += 1;
        Some(Ok(Frame {
            index,
            offset: start,
            bytes,
        }))
    }
}

/// Lowercase hex SHA-256 of a byte string.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Lowercase hex SHA-256 of a file's contents, streamed.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn jsonl(text: &str) -> Vec<RecordResult> {
        JsonLines::new(text.as_bytes()).collect()
    }

    #[test]
    fn json_lines_skip_blank_lines_and_tolerate_crlf_and_no_final_newline() {
        let got = jsonl("{\"a\":1}\r\n\n   \n[2]\n3");
        assert_eq!(got, vec![Ok(json!({"a":1})), Ok(json!([2])), Ok(json!(3))]);
    }

    #[test]
    fn first_bad_json_line_ends_the_stream_and_names_its_line() {
        let got = jsonl("{\"a\":1}\n\n{not json}\n{\"b\":2}\n");
        assert_eq!(got.len(), 2, "nothing after the bad line is read: {got:?}");
        let err = got[1].clone().expect_err("second item is the error");
        assert_eq!(err.index, 1, "one good record before it");
        assert_eq!(
            err.location, "line 3",
            "blank lines still count for the line number"
        );
        assert!(err.message.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn floats_are_read_back_as_exactly_the_double_that_was_written() {
        // 44806/45 = 995.6888888888889 (shortest form). serde_json's default best-effort parser
        // reads this text one representable value low, which once made a 1-ulp difference look
        // like 2 ulps; the replay library needs the exact double.
        let exact = 44806.0f64 / 45.0;
        let got: Vec<_> = JsonLines::new("995.6888888888889\n".as_bytes()).collect();
        assert_eq!(got, vec![Ok(json!(exact))]);
        assert_eq!(
            got[0]
                .clone()
                .ok()
                .and_then(|v| v.as_f64())
                .map(f64::to_bits),
            Some(exact.to_bits())
        );
        // And every double survives a write and a read unchanged.
        let mut x = 0x3ff0_0000_0000_0001u64;
        for _ in 0..20_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let f = f64::from_bits((x >> 2) | 0x3000_0000_0000_0000);
            let text = json!(f).to_string();
            let back: Value = serde_json::from_str(&text).expect("parse");
            assert_eq!(back.as_f64().map(f64::to_bits), Some(f.to_bits()), "{text}");
        }
    }

    #[test]
    fn python_style_nan_is_refused_not_read_as_a_number() {
        // json.dumps(float('nan')) prints NaN, which is not JSON. It must be an error, because a
        // NaN that silently became null or 0 could make two different outputs compare equal.
        let got = jsonl("{\"x\": NaN}\n");
        assert!(got[0].is_err(), "{got:?}");
    }

    #[test]
    fn an_overlong_line_is_an_error_not_a_silent_cut() {
        let got: Vec<_> = JsonLines::new("[1,2,3,4,5,6,7,8,9]\n[1]\n".as_bytes())
            .with_max_line(8)
            .collect();
        assert_eq!(got.len(), 1);
        assert!(
            got[0]
                .clone()
                .expect_err("too long")
                .message
                .contains("longer than 8")
        );
        // Exactly at the limit is fine (the newline does not count).
        let ok: Vec<_> = JsonLines::new("[1,2,33]\n".as_bytes())
            .with_max_line(8)
            .collect();
        assert_eq!(ok, vec![Ok(json!([1, 2, 33]))]);
    }

    #[test]
    fn text_lines_keep_carriage_returns_because_exact_means_exact() {
        let got: Vec<_> = TextLines::new("a\r\nb\n\nc".as_bytes()).collect();
        assert_eq!(
            got,
            vec![
                Ok(json!("a\r")),
                Ok(json!("b")),
                Ok(json!("")),
                Ok(json!("c"))
            ]
        );
        let bad: Vec<_> = TextLines::new(&b"ok\n\xff\xfe\n"[..]).collect();
        assert_eq!(bad.len(), 2);
        let err = bad[1].clone().expect_err("bad utf-8");
        assert_eq!((err.index, err.location.as_str()), (1, "line 2"));
    }

    #[test]
    fn frames_split_on_every_supported_length_prefix() {
        let mut be16 = Vec::new();
        for msg in [&b"AB"[..], &b""[..], &b"XYZ"[..]] {
            be16.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            be16.extend_from_slice(msg);
        }
        let got: Vec<_> = Frames::new(&be16[..], Framing::U16Be)
            .map(|f| f.expect("frame"))
            .map(|f| (f.index, f.offset, f.bytes))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0, b"AB".to_vec()),
                (1, 4, Vec::new()),
                (2, 6, b"XYZ".to_vec())
            ]
        );
        let mut le32 = 3u32.to_le_bytes().to_vec();
        le32.extend_from_slice(b"abc");
        let one: Vec<_> = Frames::new(&le32[..], Framing::U32Le).collect();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].clone().expect("frame").bytes, b"abc".to_vec());
        let mut be32 = 2u32.to_be_bytes().to_vec();
        be32.extend_from_slice(b"hi");
        let one: Vec<_> = Frames::new(&be32[..], Framing::U32Be).collect();
        assert_eq!(one[0].clone().expect("frame").bytes, b"hi".to_vec());
    }

    #[test]
    fn a_capture_that_ends_inside_a_frame_is_reported_as_truncated() {
        let mut data = 2u16.to_be_bytes().to_vec();
        data.extend_from_slice(b"ok");
        data.extend_from_slice(&5u16.to_be_bytes());
        data.extend_from_slice(b"abc"); // 3 of 5 bytes
        let got: Vec<_> = Frames::new(&data[..], Framing::U16Be).collect();
        assert_eq!(got.len(), 2);
        let err = got[1].clone().expect_err("truncated");
        assert_eq!(err.location, "byte offset 4");
        assert!(err.message.contains("3 of 5 bytes"), "{err}");
        // A half length header is truncation too.
        let got: Vec<_> = Frames::new(&[0u8][..], Framing::U16Be).collect();
        assert!(
            got[0]
                .clone()
                .expect_err("half header")
                .message
                .contains("truncated")
        );
    }

    #[test]
    fn sha256_matches_the_published_test_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").expect("write");
        assert_eq!(sha256_file(&p).expect("hash"), sha256_hex(b"abc"));
    }
}
