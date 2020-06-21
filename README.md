# tokio-pipe

Asynchronous pipe(2) library using tokio.

## Example

```rust
use tokio::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut r, mut w) = tokio_pipe::pipe()?;

    w.write_all(b"HELLO, WORLD!").await?;

    let mut buf = bytes::BytesMut::with_capacity(100);
    r.read_buf(&mut buf).await?;
    assert_eq!(&buf, &b"HELLO, WORLD!"[..]);

    Ok(())
}
```

