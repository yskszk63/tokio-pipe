// https://github.com/yskszk63/tokio-pipe/issues/29
use std::os::unix::io::IntoRawFd;
use std::sync::mpsc;
use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime;

fn main() -> anyhow::Result<()> {
    thread::scope(|s| {
        let (fdx, fdr) = mpsc::channel();

        let h1 = s.spawn(move || {
            println!("{:?}", thread::current().id());

            let rt = runtime::Builder::new_current_thread().enable_io().build()?;
            rt.block_on(async {
                let (rx, mut tx) = tokio_pipe::pipe()?;

                let fd = rx.into_raw_fd();
                fdx.send(fd)?;

                tx.write_all(b"Hello, World!").await?;

                Ok::<_, anyhow::Error>(())
            })
        });

        let h2 = s.spawn(move || {
            println!("{:?}", thread::current().id());

            let rt = runtime::Builder::new_current_thread().enable_io().build()?;
            rt.block_on(async {
                let fd = fdr.recv()?;
                let mut rx = tokio_pipe::PipeRead::from_raw_fd_checked(fd)?;

                let mut buf = String::new();
                rx.read_to_string(&mut buf).await?;
                println!("{}", buf);

                Ok::<_, anyhow::Error>(())
            })
        });

        h1.join().unwrap()?;
        h2.join().unwrap()?;
        Ok::<_, anyhow::Error>(())
    })?;

    thread::scope(|s| {
        let (fdx, fdr) = mpsc::channel();

        let h1 = s.spawn(move || {
            println!("{:?}", thread::current().id());

            let rt = runtime::Builder::new_current_thread().enable_io().build()?;
            rt.block_on(async {
                let (mut rx, tx) = tokio_pipe::pipe()?;

                let fd = tx.into_raw_fd();
                fdx.send(fd)?;

                let mut buf = String::new();
                rx.read_to_string(&mut buf).await?;
                println!("{}", buf);

                Ok::<_, anyhow::Error>(())
            })
        });

        let h2 = s.spawn(move || {
            println!("{:?}", thread::current().id());

            let rt = runtime::Builder::new_current_thread().enable_io().build()?;
            rt.block_on(async {
                let fd = fdr.recv()?;
                let mut tx = tokio_pipe::PipeWrite::from_raw_fd_checked(fd)?;

                tx.write_all(b"Hello, World!").await?;
                Ok::<_, anyhow::Error>(())
            })
        });

        h1.join().unwrap()?;
        h2.join().unwrap()?;
        Ok::<_, anyhow::Error>(())
    })?;

    let rt = runtime::Builder::new_multi_thread().enable_io().build()?;
    rt.block_on(async {
        let (rx, tx) = tokio_pipe::pipe()?;
        let rx = rx.into_raw_fd();
        let tx = tx.into_raw_fd();

        let h1 = tokio::spawn(async move {
            println!("{:?}", thread::current().id());

            let mut rx = tokio_pipe::PipeRead::from_raw_fd_checked(rx)?;

            let mut buf = String::new();
            rx.read_to_string(&mut buf).await?;
            println!("{}", buf);

            Ok::<_, anyhow::Error>(())
        });
        let h2 = tokio::spawn(async move {
            println!("{:?}", thread::current().id());

            let mut tx = tokio_pipe::PipeWrite::from_raw_fd_checked(tx)?;
            tx.write_all(b"Hello, World!").await?;

            Ok::<_, anyhow::Error>(())
        });

        h1.await??;
        h2.await??;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
