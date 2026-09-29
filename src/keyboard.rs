//! The keyboard: finding it, opening its tty in raw mode, and talking Focus.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long to wait for the keyboard's reply to a command.
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) fn find_keyboard(explicit: &Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.exists().then(|| p.clone());
    }
    let mut hits: Vec<PathBuf> = fs::read_dir("/dev/serial/by-id")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().contains("keyboardio"))
        .map(|e| e.path())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Open the tty in raw mode (no echo, no CR/LF translation) with a 100 ms read
/// timeout (VMIN=0, VTIME=1: read() returns 0 bytes on timeout).
pub(crate) fn open_keyboard(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let fd = file.as_raw_fd();
    // SAFETY: plain termios/fcntl calls on a valid, owned fd.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        t.c_cflag |= libc::CLOCAL | libc::CREAD;
        t.c_cflag &= !libc::HUPCL;
        libc::cfsetispeed(&mut t, libc::B115200);
        libc::cfsetospeed(&mut t, libc::B115200);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 1;
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// Send one Focus command line and read the reply through its terminating
/// "\r\n.\r\n". Returns whether the reply contained "ok".
pub(crate) fn exchange(port: &mut File, line: &str) -> io::Result<bool> {
    port.write_all(line.as_bytes())?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut reply = Vec::new();
    let mut tmp = [0u8; 64];
    loop {
        match port.read(&mut tmp) {
            Ok(n) if n > 0 => {
                reply.extend_from_slice(&tmp[..n]);
                if reply.ends_with(b"\r\n.\r\n") || reply.ends_with(b"\n.\n") {
                    return Ok(reply.windows(2).any(|w| w == b"ok"));
                }
            }
            Ok(_) => {} // read timeout
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply from keyboard"));
        }
    }
}

pub(crate) fn format_set(channel: u8, data: &[u8]) -> String {
    let mut s = format!("hostlink.set {} {}", channel, data.len());
    for b in data {
        s.push(' ');
        s.push_str(&b.to_string());
    }
    s.push('\n');
    s
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_format() {
        assert_eq!(format_set(0, &[17, 1, 0]), "hostlink.set 0 3 17 1 0\n");
    }}
