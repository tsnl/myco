//! Minimal pty plumbing for interactive sessions: `openpty`, a child wired to
//! the slave as its controlling terminal, and nonblocking async master I/O.
//!
//! Deliberately libc-only (no pty crate): the whole need is one `openpty`,
//! two fcntls, and read/write through [`tokio::io::unix::AsyncFd`].

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::unix::AsyncFd;

/// Read half of a pty master (a dup of the writer's fd, so each side owns its
/// own [`AsyncFd`] registration).
pub struct PtyReader {
    io: AsyncFd<OwnedFd>,
}

/// Write half of a pty master.
pub struct PtyWriter {
    io: AsyncFd<OwnedFd>,
}

/// Open a pty of `cols`×`rows`. Returns the master split into read/write
/// halves plus the slave, which the caller passes to the child as
/// stdin/stdout/stderr and then drops — the parent must hold no slave fd, or
/// the master would never see EOF when the child exits.
pub fn open(cols: u16, rows: u16) -> Result<(PtyReader, PtyWriter, OwnedFd), String> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(format!("openpty: {}", std::io::Error::last_os_error()));
    }
    // Safety: openpty returned two fresh fds we now own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };

    // The master stays out of children and must not block the runtime.
    set_flags(&master).map_err(|e| format!("pty master fcntl: {e}"))?;
    let read_half = master
        .try_clone()
        .map_err(|e| format!("pty master dup: {e}"))?;

    Ok((
        PtyReader {
            io: AsyncFd::new(read_half).map_err(|e| format!("pty reader register: {e}"))?,
        },
        PtyWriter {
            io: AsyncFd::new(master).map_err(|e| format!("pty writer register: {e}"))?,
        },
        slave,
    ))
}

fn set_flags(fd: &OwnedFd) -> std::io::Result<()> {
    let raw = fd.as_raw_fd();
    // Safety: plain fcntl on an fd we own.
    unsafe {
        if libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags == -1 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Make the calling child a session leader with fd 0 (the pty slave) as its
/// controlling terminal. For use in `pre_exec`; replaces `process_group(0)` —
/// `setsid` already puts the child in a fresh process group of its own pid,
/// so `kill(-pid)` keeps working.
///
/// # Safety
/// Async-signal-safe calls only (`setsid`, `ioctl`); safe to run post-fork.
pub unsafe fn make_controlling_tty() -> std::io::Result<()> {
    // Safety: both calls are async-signal-safe.
    unsafe {
        if libc::setsid() == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

impl tokio::io::AsyncRead for PtyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            let mut guard = ready!(self.io.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|inner| {
                // Safety: reading into an initialized buffer we own.
                let n = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        unfilled.as_mut_ptr().cast(),
                        unfilled.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                // EIO from a pty master means every slave fd is closed — the
                // child is gone. That is this device's spelling of EOF.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl PtyWriter {
    /// Resize the terminal window (TIOCSWINSZ on the master): the kernel
    /// delivers SIGWINCH to the foreground process group, so a full-screen
    /// program redraws at the new size.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Safety: plain ioctl on an fd we own.
        if unsafe { libc::ioctl(self.io.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &ws) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Write all of `data` to the master (delivered to the child as terminal
    /// input).
    pub async fn write_all(&mut self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = self.io.writable().await?;
            match guard.try_io(|inner| {
                // Safety: writing from a buffer we hold for the whole call.
                let n = unsafe {
                    libc::write(
                        inner.get_ref().as_raw_fd(),
                        data.as_ptr().cast(),
                        data.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(Ok(0)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "pty master wrote zero bytes",
                    ));
                }
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }
}
