//! The program's own terminal for a `debug_session` (Linux).
//!
//! Without one, the program under debug shares gdb's stdio, which in MI mode
//! is the tool's command channel:
//!
//! * its stdin IS the pipe we write MI commands into — a program that reads
//!   stdin blocked the session for 30 s and then consumed the model's next
//!   command as its input (`print 1234` was read by `fgets`, never by gdb);
//! * its stdout is interleaved with MI records and was discarded as
//!   unrecognized lines, so the model never saw what the program printed — and
//!   a prompt printed without a newline glued itself onto the next MI record,
//!   corrupting it;
//! * its stderr went to gdb's stderr, which is `/dev/null` (assert messages,
//!   sanitizer reports).
//!
//! A pseudo-terminal given to the inferior with `-inferior-tty-set` separates
//! all three. It is put in raw mode with `VMIN = 0, VTIME = 0`, so every read of
//! stdin returns 0 at once: the program sees end-of-file, exactly as under
//! `cpp_sanitize` and the batch `debug` tool (`/dev/null`). Its output is
//! collected by a background reader — so a chatty program never blocks on a
//! full terminal buffer — and returned with the command that produced it.
//!
//! `posix_openpt` + `ptsname_r`, not `openpty`: glibc moved `openpty` into
//! libc only in 2.34, and linking it would stop the engine starting on the
//! older distributions it supports.

use std::ffi::CStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;

/// Program output kept between commands. The model sees the TAIL when more
/// than this arrived: the lines just before a crash matter most.
const KEEP: usize = 64 * 1024;

/// Bytes the program wrote that have not yet been returned to the model.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// Bytes discarded from the front because more than [`KEEP`] arrived.
    dropped: usize,
}

impl Captured {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > KEEP {
            let excess = self.bytes.len() - KEEP;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }
}

pub(crate) struct InferiorTty {
    master: Arc<AsyncFd<OwnedFd>>,
    /// Held open for the session's life: with no slave fd open, reading the
    /// master fails with EIO between runs, and the reader would stop.
    _slave: OwnedFd,
    path: String,
    captured: Arc<Mutex<Captured>>,
    reader: JoinHandle<()>,
}

impl InferiorTty {
    /// Open a raw, EOF-on-read terminal and start collecting what the program
    /// writes to it. Must be called inside the tokio runtime.
    pub(crate) fn open() -> io::Result<Self> {
        // SAFETY: plain libc calls on fds we own; every return is checked and
        // each fd is wrapped in an OwnedFd as soon as it exists.
        let (master, slave, path) = unsafe {
            let raw = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let master = OwnedFd::from_raw_fd(raw);
            if libc::grantpt(raw) != 0 || libc::unlockpt(raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut name = [0 as libc::c_char; 128];
            let rc = libc::ptsname_r(raw, name.as_mut_ptr(), name.len());
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            let path = CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if slave < 0 {
                return Err(io::Error::last_os_error());
            }
            let slave = OwnedFd::from_raw_fd(slave);
            make_eof_on_read(&slave)?;
            let flags = libc::fcntl(raw, libc::F_GETFL);
            if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
            (master, slave, path)
        };
        let master = Arc::new(AsyncFd::new(master)?);
        let captured = Arc::new(Mutex::new(Captured::default()));
        let reader = tokio::spawn(pump(master.clone(), captured.clone()));
        Ok(Self {
            master,
            _slave: slave,
            path,
            captured,
            reader,
        })
    }

    /// Put the terminal back to raw, EOF-on-read before a program starts: a
    /// previous run may have switched it to line mode (a password prompt, a
    /// REPL), and a new run would then block reading stdin instead of seeing
    /// EOF. Measured: after one such program, every later `run` hung 30 s.
    pub(crate) fn reset(&self) {
        let _ = make_eof_on_read(&self._slave);
    }

    /// The terminal's device path, for `-inferior-tty-set`.
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// Everything the program wrote since the last call, lossily decoded.
    /// Reads what the kernel already holds first, so output written just
    /// before a stop is never left behind for the next command.
    pub(crate) fn take(&self) -> Option<String> {
        drain(&self.master, &self.captured);
        let mut captured = self.captured.lock().ok()?;
        if captured.bytes.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut captured.bytes);
        let dropped = std::mem::take(&mut captured.dropped);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Some(if dropped > 0 {
            format!("[{dropped} earlier bytes of program output not shown]\n{text}")
        } else {
            text
        })
    }
}

impl Drop for InferiorTty {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Raw mode with `VMIN = 0, VTIME = 0`: a read returns what is there, or 0
/// (end-of-file) at once.
fn make_eof_on_read(slave: &OwnedFd) -> io::Result<()> {
    // SAFETY: termios calls on an fd we own, with a zeroed struct that
    // tcgetattr fills before we read it.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut termios) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut termios);
        termios.c_cc[libc::VMIN] = 0;
        termios.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Keep the terminal drained while the program runs, so it never blocks on a
/// full buffer.
async fn pump(master: Arc<AsyncFd<OwnedFd>>, captured: Arc<Mutex<Captured>>) {
    loop {
        let Ok(mut guard) = master.readable().await else {
            return;
        };
        match guard.try_io(|fd| read_some(fd.get_ref())) {
            Ok(Ok(Some(data))) => {
                if let Ok(mut captured) = captured.lock() {
                    captured.push(&data);
                }
            }
            // EOF / EIO: the terminal is gone.
            Ok(Ok(None)) | Ok(Err(_)) => return,
            // Spurious readiness; try_io already cleared it.
            Err(_would_block) => {}
        }
    }
}

/// Move whatever the kernel holds right now into `captured`, without waiting.
fn drain(master: &AsyncFd<OwnedFd>, captured: &Mutex<Captured>) {
    while let Ok(Some(data)) = read_some(master.get_ref()) {
        if let Ok(mut captured) = captured.lock() {
            captured.push(&data);
        }
    }
}

/// One non-blocking read: `Some(bytes)`, `None` at EOF/EIO, `WouldBlock` when
/// nothing is waiting.
fn read_some(fd: &OwnedFd) -> io::Result<Option<Vec<u8>>> {
    let mut buf = [0u8; 8192];
    // SAFETY: reading into a stack buffer of the length we pass.
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        return Ok(Some(buf[..n as usize].to_vec()));
    }
    if n == 0 {
        return Ok(None);
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        return Err(err);
    }
    // EIO: no slave side open. Nothing more will arrive.
    Ok(None)
}
