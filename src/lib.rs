#![doc(html_root_url = "https://docs.rs/tokio-pipe/0.3.0")]
#![feature(io_safety)]
//! Asynchronous pipe(2) library using tokio.
//!
//! # Example
//!
//! ```
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let (mut r, mut w) = tokio_pipe::pipe()?;
//!
//!     w.write_all(b"HELLO, WORLD!").await?;
//!
//!     let mut buf = [0; 16];
//!     let len = r.read(&mut buf[..]).await?;
//!
//!     assert_eq!(&buf[..len], &b"HELLO, WORLD!"[..]);
//!     Ok(())
//! }
//! ```
use std::cmp;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(target_os = "macos")]
const MAX_LEN: usize = <libc::c_int>::MAX as usize - 1;

#[cfg(not(target_os = "macos"))]
const MAX_LEN: usize = <libc::ssize_t>::MAX as usize;

unsafe fn set_nonblocking(fd: RawFd) {
    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
}

#[cfg(not(any(target_os = "linux", target_os = "solaris")))]
macro_rules! try_libc {
    ($e: expr) => {{
        let ret = $e;
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        ret
    }};
}

macro_rules! cvt {
    ($e:expr) => {{
        let ret = $e;
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }};
}

macro_rules! ready {
    ($e:expr) => {
        match $e {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(e) => e,
        }
    };
}

fn is_wouldblock(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

/// Pipe read
pub struct PipeRead(AsyncFd<OwnedFd>);

impl AsyncRead for PipeRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let fd = self.0.as_raw_fd();

        loop {
            let pinned = Pin::new(&mut self.0);
            let mut ready = ready!(pinned.poll_read_ready(cx))?;
            let ret = unsafe {
                libc::read(
                    fd,
                    buf.unfilled_mut() as *mut _ as *mut c_void,
                    cmp::min(buf.remaining(), MAX_LEN),
                )
            };
            match cvt!(ret) {
                Err(e) if is_wouldblock(&e) => {
                    ready.clear_ready();
                }
                Err(e) => return Poll::Ready(Err(e)),
                Ok(ret) => {
                    let ret = ret as usize;
                    unsafe {
                        buf.assume_init(ret);
                    };
                    buf.advance(ret);
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl AsRawFd for PipeRead {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for PipeRead {
    fn into_raw_fd(self) -> RawFd {
        let inner = self.0.into_inner();
        inner.into_raw_fd()
    }
}

impl FromRawFd for PipeRead {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        Self(AsyncFd::new(OwnedFd::from_raw_fd(fd)).unwrap())
    }
}

impl AsFd for PipeRead {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.get_ref().as_fd()
    }
}

impl fmt::Debug for PipeRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipeRead({})", self.as_raw_fd())
    }
}

/// Pipe write
pub struct PipeWrite(AsyncFd<OwnedFd>);

impl AsRawFd for PipeWrite {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for PipeWrite {
    fn into_raw_fd(self) -> RawFd {
        let inner = self.0.into_inner();
        inner.into_raw_fd()
    }
}

impl FromRawFd for PipeWrite {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        Self(AsyncFd::new(OwnedFd::from_raw_fd(fd)).unwrap())
    }
}

impl AsyncWrite for PipeWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let fd = self.0.as_raw_fd();

        loop {
            let pinned = Pin::new(&mut self.0);
            let mut ready = ready!(pinned.poll_write_ready(cx))?;
            let ret = unsafe {
                libc::write(
                    fd,
                    buf.as_ptr() as *mut c_void,
                    cmp::min(buf.len(), MAX_LEN),
                )
            };
            match cvt!(ret) {
                Err(e) if is_wouldblock(&e) => {
                    ready.clear_ready();
                }
                Err(e) => return Poll::Ready(Err(e)),
                Ok(ret) => return Poll::Ready(Ok(ret as usize)),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsFd for PipeWrite {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.get_ref().as_fd()
    }
}

impl fmt::Debug for PipeWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipeRead({})", self.as_raw_fd())
    }
}

#[cfg(any(target_os = "linux", target_os = "solaris"))]
fn sys_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut pipefd = [0; 2];
    let ret = unsafe { libc::pipe2(pipefd.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((pipefd[0], pipefd[1]))
}

#[cfg(not(any(target_os = "linux", target_os = "solaris")))]
fn sys_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut pipefd = [0; 2];
    try_libc!(unsafe { libc::pipe(pipefd.as_mut_ptr()) });
    for fd in &pipefd {
        let ret = try_libc!(unsafe { libc::fcntl(*fd, libc::F_GETFD) });
        try_libc!(unsafe { libc::fcntl(*fd, libc::F_SETFD, ret | libc::FD_CLOEXEC) });
        let ret = try_libc!(unsafe { libc::fcntl(*fd, libc::F_GETFL) });
        try_libc!(unsafe { libc::fcntl(*fd, libc::F_SETFL, ret | libc::O_NONBLOCK) });
    }
    Ok((pipefd[0], pipefd[1]))
}

/// Open pipe
pub fn pipe() -> io::Result<(PipeRead, PipeWrite)> {
    let (r, w) = sys_pipe()?;
    unsafe {
        Ok((
            PipeRead(AsyncFd::new(OwnedFd::from_raw_fd(r))?),
            PipeWrite(AsyncFd::new(OwnedFd::from_raw_fd(w))?),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test() {
        let (mut r, mut w) = pipe().unwrap();

        let w_task = tokio::spawn(async move {
            for n in 0..=65535 {
                w.write_u32(n).await.unwrap();
            }
            //w.shutdown().await.unwrap();
        });

        let r_task = tokio::spawn(async move {
            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 65535 {
                r.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
            }
        });
        tokio::try_join!(w_task, r_task).unwrap();
    }

    #[tokio::test]
    async fn test_write_after_shutdown() {
        let (r, mut w) = pipe().unwrap();
        w.shutdown().await.unwrap();
        let result = w.write(b"ok").await;
        assert!(result.is_ok());

        drop(r)
    }

    #[tokio::test]
    async fn test_read_to_end() -> io::Result<()> {
        let (mut r, mut w) = pipe()?;
        let t = tokio::spawn(async move {
            w.write_all(&b"Hello, World!"[..]).await?;
            io::Result::Ok(())
        });

        let mut buf = vec![];
        r.read_to_end(&mut buf).await?;
        assert_eq!(&b"Hello, World!"[..], &buf[..]);

        t.await?
    }

    #[tokio::test]
    async fn test_from_child_stdio() -> io::Result<()> {
        use std::process::Stdio;
        use tokio::process::Command;

        let (mut r, w) = pipe()?;

        let script = r#"#!/usr/bin/env python3
import os
with os.fdopen(1, 'wb') as w:
    w.write(b"Hello, World!")
"#;

        let mut command = Command::new("python");
        command
            .args(&["-c", script])
            .stdout(unsafe { Stdio::from_raw_fd(w.as_raw_fd()) });
        unsafe {
            // suppress posix_spawn
            command.pre_exec(|| Ok(()));
        }
        let mut child = command.spawn()?;
        drop(w);

        let mut buf = vec![];
        r.read_to_end(&mut buf).await?;
        assert_eq!(&b"Hello, World!"[..], &buf[..]);

        child.wait().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_from_child_no_stdio() -> io::Result<()> {
        use tokio::process::Command;

        let (mut r, w) = pipe()?;

        let script = r#"#!/usr/bin/env python3
import os
with os.fdopen(3, 'wb') as w:
    w.write(b"Hello, World!")
"#;

        let mut command = Command::new("python");
        command.args(&["-c", script]);
        unsafe {
            let w = w.as_raw_fd();
            command.pre_exec(move || {
                if w == 3 {
                    // drop CLOEXEC
                    let flags = libc::fcntl(w, libc::F_SETFD);
                    if flags == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if flags & libc::FD_CLOEXEC != 0
                        && libc::fcntl(w, libc::F_SETFD, flags ^ libc::FD_CLOEXEC) == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                } else {
                    let r = libc::dup2(w, 3);
                    if r == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(w);

        let mut buf = vec![];
        r.read_to_end(&mut buf).await?;
        assert_eq!(&b"Hello, World!"[..], &buf[..]);

        child.wait().await?;
        Ok(())
    }
}
