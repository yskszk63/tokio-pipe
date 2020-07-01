#![doc(html_root_url = "https://docs.rs/tokio-pipe/0.1.3")]
//! Asynchronous pipe(2) library using tokio.
//!
//! # Example
//!
//! ```
//! use tokio::prelude::*;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let (mut r, mut w) = tokio_pipe::pipe()?;
//!
//!     w.write_all(b"HELLO, WORLD!").await?;
//!
//!     let mut buf = bytes::BytesMut::with_capacity(100);
//!     r.read_buf(&mut buf).await?;
//!
//!     assert_eq!(&buf, &b"HELLO, WORLD!"[..]);
//!     Ok(())
//! }
//! ```
use std::cmp;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::event::Evented;
use mio::unix::EventedFd;
use mio::{Poll as MioPoll, PollOpt, Ready, Token};
use tokio::io::{AsyncRead, AsyncWrite, PollEvented};

#[cfg(target_os = "macos")]
const MAX_LEN: usize = <libc::c_int>::MAX as usize - 1;

#[cfg(not(target_os = "macos"))]
const MAX_LEN: usize = <libc::ssize_t>::MAX as usize;

unsafe fn set_nonblocking(fd: RawFd) {
    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
}

struct PipeFd(RawFd);

impl Evented for PipeFd {
    fn register(
        &self,
        poll: &MioPoll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &MioPoll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.0).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &MioPoll) -> io::Result<()> {
        EventedFd(&self.0).deregister(poll)
    }
}

impl io::Read for PipeFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::read(
                self.0,
                buf.as_mut_ptr() as *mut c_void,
                cmp::min(buf.len(), MAX_LEN),
            )
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }
}

impl io::Write for PipeFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::write(
                self.0,
                buf.as_ptr() as *mut c_void,
                cmp::min(buf.len(), MAX_LEN),
            )
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(ret as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PipeFd {
    fn close(&mut self) -> io::Result<()> {
        let ret = unsafe { libc::close(self.0) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for PipeFd {
    fn drop(&mut self) {
        self.close().ok();
    }
}

/// Pipe read
pub struct PipeRead(PollEvented<PipeFd>);

impl AsyncRead for PipeRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsRawFd for PipeRead {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().0
    }
}

impl IntoRawFd for PipeRead {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.0.get_ref().0;
        mem::forget(self);
        fd
    }
}

impl FromRawFd for PipeRead {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        PipeRead(PollEvented::new(PipeFd(fd)).unwrap())
    }
}

impl fmt::Debug for PipeRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipeRead({})", self.as_raw_fd())
    }
}

/// Pipe write
pub struct PipeWrite(PollEvented<PipeFd>);

impl AsRawFd for PipeWrite {
    fn as_raw_fd(&self) -> RawFd {
        self.0.get_ref().0
    }
}

impl IntoRawFd for PipeWrite {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.0.get_ref().0;
        mem::forget(self);
        fd
    }
}

impl FromRawFd for PipeWrite {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        PipeWrite(PollEvented::new(PipeFd(fd)).unwrap())
    }
}

impl AsyncWrite for PipeWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl fmt::Debug for PipeWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipeRead({})", self.as_raw_fd())
    }
}

fn sys_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut pipefd = [0; 2];
    let ret = unsafe { libc::pipe2(pipefd.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((pipefd[0], pipefd[1]))
}

/// Open pipe
pub fn pipe() -> io::Result<(PipeRead, PipeWrite)> {
    let (r, w) = sys_pipe()?;
    Ok((
        PipeRead(PollEvented::new(PipeFd(r))?),
        PipeWrite(PollEvented::new(PipeFd(w))?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::prelude::*;

    #[tokio::test]
    async fn test() {
        let (mut r, mut w) = pipe().unwrap();

        let w_task = tokio::spawn(async move {
            for n in 0..65535 {
                w.write_u32(n).await.unwrap();
            }
            //w.shutdown().await.unwrap();
        });

        let r_task = tokio::spawn(async move {
            let mut n = 0u32;
            let mut buf = bytes::BytesMut::with_capacity(4 * 100);
            while r.read_buf(&mut buf).await.unwrap() != 0 {
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
                buf.clear()
            }
        });
        tokio::try_join!(w_task, r_task).unwrap();
    }

    #[tokio::test]
    async fn test_buf() {
        let (mut r, mut w) = pipe().unwrap();

        let w_task = tokio::spawn(async move {
            for _ in 0..16384 {
                w.write_buf(&mut &[0u8; 8 * 1024][..]).await.unwrap();
            }
            //w.shutdown().await.unwrap();
        });

        let r_task = tokio::spawn(async move {
            let mut buf = bytes::BytesMut::with_capacity(8 * 1024);
            while r.read_buf(&mut buf).await.unwrap() != 0 {
                assert!(buf.iter().all(|n| *n == 0));
                buf.clear()
            }
        });
        tokio::try_join!(w_task, r_task).unwrap();
    }

    #[tokio::test]
    async fn test_non_buf() {
        let (mut r, mut w) = pipe().unwrap();

        let w_task = tokio::spawn(async move {
            for _ in 0..16384 {
                w.write(&[0u8; 8 * 1024][..]).await.unwrap();
            }
            //w.shutdown().await.unwrap();
        });

        let r_task = tokio::spawn(async move {
            let mut buf = [1u8; 8 * 1024];
            while r.read(&mut buf).await.unwrap() != 0 {
                assert!(buf.iter().all(|n| *n == 0));
                buf = [1u8; 8 * 1024];
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
}
