#![doc(html_root_url = "https://docs.rs/tokio-pipe/0.2.5")]
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
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
#[cfg(target_os = "linux")]
use std::ptr;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(target_os = "linux")]
pub use libc::off64_t;

pub use libc::PIPE_BUF;

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

// needs impl AsRawFd for RawFd (^v1.48)
#[derive(Debug)]
struct PipeFd(RawFd);

impl AsRawFd for PipeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for PipeFd {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.0) };
    }
}

/// A buffer that can be written atomically
#[derive(Copy, Clone, Debug)]
pub struct AtomicWriteBuffer<'a>(&'a [u8]);
impl<'a> AtomicWriteBuffer<'a> {
    /// If buffer is more than PIPE_BUF, then return None.
    pub fn new(buffer: &'a [u8]) -> Option<Self> {
        if buffer.len() <= PIPE_BUF {
            Some(Self(buffer))
        } else {
            None
        }
    }

    pub fn into_inner(self) -> &'a [u8] {
        self.0
    }
}

/// `IoSlice`s that can be written atomically
#[derive(Copy, Clone, Debug)]
pub struct AtomicWriteIoSlices<'a, 'b>(&'a [io::IoSlice<'b>]);
impl<'a, 'b> AtomicWriteIoSlices<'a, 'b> {
    /// If total length is more than PIPE_BUF, then return None.
    pub fn new(buffers: &'a [io::IoSlice<'b>]) -> Option<Self> {
        let len: usize = buffers.iter().map(|slice| slice.len()).sum();
        if len <= PIPE_BUF {
            Some(Self(buffers))
        } else {
            None
        }
    }

    pub fn into_inner(self) -> &'a [io::IoSlice<'b>] {
        self.0
    }
}

#[cfg(target_os = "linux")]
async fn tee_impl(pipe_in: &PipeRead, pipe_out: &PipeWrite, len: usize) -> io::Result<usize> {
    let fd_in = pipe_in.0.as_raw_fd();
    let fd_out = pipe_out.0.as_raw_fd();

    // There is only one reader and one writer, so it only needs to polled once.
    let _read_ready = pipe_in.0.readable().await?;
    let _write_ready = pipe_out.0.writable().await?;

    loop {
        let ret = unsafe { libc::tee(fd_in, fd_out, len, libc::SPLICE_F_NONBLOCK) };
        match cvt!(ret) {
            Err(e) if is_wouldblock(&e) => (),
            Err(e) => break Err(e),
            Ok(ret) => break Ok(ret as usize),
        }
    }
}

/// Duplicates up to len bytes of data from pipe_in to pipe_out.
///
/// It does not consume the data that is duplicated from pipe_in; therefore, that data
/// can be copied by a subsequent splice.
#[cfg(target_os = "linux")]
pub async fn tee(
    pipe_in: &mut PipeRead,
    pipe_out: &mut PipeWrite,
    len: usize,
) -> io::Result<usize> {
    tee_impl(pipe_in, pipe_out, len).await
}

#[cfg(target_os = "linux")]
fn as_ptr<T>(option: Option<&mut T>) -> *mut T {
    match option {
        Some(some) => some,
        None => ptr::null_mut(),
    }
}

#[cfg(target_os = "linux")]
async fn splice_impl(
    asyncfd_in: &mut AsyncFd<impl AsRawFd>,
    off_in: Option<&mut off64_t>,
    asyncfd_out: &AsyncFd<impl AsRawFd>,
    off_out: Option<&mut off64_t>,
    len: usize,
    has_more_data: bool,
) -> io::Result<usize> {
    let fd_in = asyncfd_in.as_raw_fd();
    let fd_out = asyncfd_out.as_raw_fd();

    let off_in = as_ptr(off_in);
    let off_out = as_ptr(off_out);

    let flags = libc::SPLICE_F_NONBLOCK
        | if has_more_data {
            libc::SPLICE_F_MORE
        } else {
            0
        };

    // There is only one reader and one writer, so it only needs to polled once.
    let _read_ready = asyncfd_in.readable().await?;
    let _write_ready = asyncfd_out.writable().await?;

    loop {
        let ret = unsafe { libc::splice(fd_in, off_in, fd_out, off_out, len, flags) };
        match cvt!(ret) {
            Err(e) if is_wouldblock(&e) => (),
            Err(e) => break Err(e),
            Ok(ret) => break Ok(ret as usize),
        }
    }
}

/// Moves data between pipes without copying between kernel address space and
/// user address space.
///
/// It transfers up to len bytes of data from pipe_in to pipe_out.
#[cfg(target_os = "linux")]
pub async fn splice(
    pipe_in: &mut PipeRead,
    pipe_out: &mut PipeWrite,
    len: usize,
) -> io::Result<usize> {
    splice_impl(&mut pipe_in.0, None, &pipe_out.0, None, len, false).await
}

/// Pipe read
pub struct PipeRead(AsyncFd<PipeFd>);
impl PipeRead {
    /// Moves data between pipe and fd without copying between kernel address space and
    /// user address space.
    ///
    /// It transfers up to len bytes of data from self to asyncfd_out.
    ///
    ///  * `asyncfd_out` - must be have O_NONBLOCK set,
    ///    otherwise this function might block.
    ///  * `off_out` - If it is not None, then it would be updated on success.
    ///  * `has_more_data` - If there is more data to be sent to off_out.
    ///    This is a helpful hint for socket (see also the description of MSG_MORE
    ///    in send(2), and the description of TCP_CORK in tcp(7)).
    #[cfg(target_os = "linux")]
    pub async fn splice_to(
        &mut self,
        asyncfd_out: &AsyncFd<impl AsRawFd>,
        off_out: Option<&mut off64_t>,
        len: usize,
        has_more_data: bool,
    ) -> io::Result<usize> {
        splice_impl(&mut self.0, None, asyncfd_out, off_out, len, has_more_data).await
    }
}

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
        let fd = inner.0;
        mem::forget(inner);
        fd
    }
}

impl FromRawFd for PipeRead {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        Self(AsyncFd::new(PipeFd(fd)).unwrap())
    }
}

impl fmt::Debug for PipeRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipeRead({})", self.as_raw_fd())
    }
}

/// Pipe write
pub struct PipeWrite(AsyncFd<PipeFd>);

impl AsRawFd for PipeWrite {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for PipeWrite {
    fn into_raw_fd(self) -> RawFd {
        let inner = self.0.into_inner();
        let fd = inner.0;
        mem::forget(inner);
        fd
    }
}

impl FromRawFd for PipeWrite {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        set_nonblocking(fd);
        Self(AsyncFd::new(PipeFd(fd)).unwrap())
    }
}

impl PipeWrite {
    fn poll_write_impl(
        self: Pin<&Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let fd = self.0.as_raw_fd();

        loop {
            let pinned = Pin::new(&self.0);
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

    /// Write buf atomically to the pipe, using guarantees provided in POSIX.1
    pub fn poll_write_atomic(
        self: Pin<&Self>,
        cx: &mut Context<'_>,
        buf: AtomicWriteBuffer,
    ) -> Poll<Result<usize, io::Error>> {
        self.poll_write_impl(cx, buf.0)
    }

    fn poll_write_vectored_impl(
        self: Pin<&Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let fd = self.0.as_raw_fd();

        loop {
            let pinned = Pin::new(&self.0);
            let mut ready = ready!(pinned.poll_write_ready(cx))?;
            let ret =
                unsafe { libc::writev(fd, bufs.as_ptr() as *const libc::iovec, bufs.len() as i32) };
            match cvt!(ret) {
                Err(e) if is_wouldblock(&e) => {
                    ready.clear_ready();
                }
                Err(e) => return Poll::Ready(Err(e)),
                Ok(ret) => return Poll::Ready(Ok(ret as usize)),
            }
        }
    }

    pub fn poll_write_vectored_atomic(
        self: Pin<&Self>,
        cx: &mut Context<'_>,
        bufs: AtomicWriteIoSlices<'_, '_>,
    ) -> Poll<Result<usize, io::Error>> {
        self.poll_write_vectored_impl(cx, bufs.0)
    }

    /// Moves data between fd and pipe without copying between kernel address space and
    /// user address space.
    ///
    /// It transfers up to len bytes of data from asyncfd_in to self.
    ///
    ///  * `asyncfd_in` - must be have O_NONBLOCK set,
    ///    otherwise this function might block.
    ///    There must not be other reader for that fd (or its duplicates).
    ///  * `off_in` - If it is not None, then it would be updated on success.
    #[cfg(target_os = "linux")]
    pub async fn splice_from(
        &mut self,
        asyncfd_in: &mut AsyncFd<impl AsRawFd>,
        off_in: Option<&mut off64_t>,
        len: usize,
    ) -> io::Result<usize> {
        splice_impl(asyncfd_in, off_in, &self.0, None, len, false).await
    }
}

impl AsyncWrite for PipeWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.as_ref().poll_write_impl(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        self.as_ref().poll_write_vectored_impl(cx, bufs)
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
    Ok((
        PipeRead(AsyncFd::new(PipeFd(r))?),
        PipeWrite(AsyncFd::new(PipeFd(w))?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(target_os = "linux")]
    use tokio::time::{sleep, Duration};

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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_tee() {
        let (mut r1, mut w1) = pipe().unwrap();
        let (mut r2, mut w2) = pipe().unwrap();

        for n in 0..1024 {
            w1.write_u32(n).await.unwrap();
        }

        tee(&mut r1, &mut w2, 4096).await.unwrap();

        let r2_task = tokio::spawn(async move {
            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 1024 {
                r2.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
            }
        });

        let r1_task = tokio::spawn(async move {
            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 1024 {
                r1.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
            }
        });

        tokio::try_join!(r1_task, r2_task).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_tee_no_inf_loop() {
        let (mut r1, mut w1) = pipe().unwrap();
        let (mut r2, mut w2) = pipe().unwrap();

        let w1_task = tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;

            for n in 0..1024 {
                w1.write_u32(n).await.unwrap();
            }
        });

        for n in 0..1024 {
            w2.write_u32(n).await.unwrap();
        }

        let r2_task = tokio::spawn(async move {
            sleep(Duration::from_millis(200)).await;

            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 1024 {
                r2.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
            }
        });

        tee(&mut r1, &mut w2, 4096).await.unwrap();

        tokio::try_join!(w1_task, r2_task).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_splice() {
        let (mut r1, mut w1) = pipe().unwrap();
        let (mut r2, mut w2) = pipe().unwrap();

        for n in 0..1024 {
            w1.write_u32(n).await.unwrap();
        }

        splice(&mut r1, &mut w2, 4096).await.unwrap();

        let mut n = 0u32;
        let mut buf = [0; 4 * 128];
        while n < 1024 {
            r2.read_exact(&mut buf).await.unwrap();
            for x in buf.chunks(4) {
                assert_eq!(x, n.to_be_bytes());
                n += 1;
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_splice_no_inf_loop() {
        let (mut r1, mut w1) = pipe().unwrap();
        let (mut r2, mut w2) = pipe().unwrap();

        let w1_task = tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;

            for n in 0..1024 {
                w1.write_u32(n).await.unwrap();
            }
        });

        for n in 0..1024 {
            w2.write_u32(n).await.unwrap();
        }

        let r2_task = tokio::spawn(async move {
            sleep(Duration::from_millis(200)).await;

            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 1024 {
                r2.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_be_bytes());
                    n += 1;
                }
            }
        });

        splice(&mut r1, &mut w2, 4096).await.unwrap();

        tokio::try_join!(w1_task, r2_task).unwrap();
    }

    fn as_ioslice<T>(v: &[T]) -> io::IoSlice<'_> {
        io::IoSlice::new(unsafe {
            std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * std::mem::size_of::<T>())
        })
    }

    #[tokio::test]
    async fn test_writev() {
        let (mut r, mut w) = pipe().unwrap();

        let w_task = tokio::spawn(async move {
            let buffer1: Vec<u32> = (0..512).collect();
            let buffer2: Vec<u32> = (512..1024).collect();

            w.write_vectored(&[as_ioslice(&buffer1), as_ioslice(&buffer2)])
                .await
                .unwrap();
        });

        let r_task = tokio::spawn(async move {
            let mut n = 0u32;
            let mut buf = [0; 4 * 128];
            while n < 1024 {
                r.read_exact(&mut buf).await.unwrap();
                for x in buf.chunks(4) {
                    assert_eq!(x, n.to_ne_bytes());
                    n += 1;
                }
            }
        });
        tokio::try_join!(w_task, r_task).unwrap();
    }
}
