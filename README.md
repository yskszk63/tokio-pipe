# tokio-pipe

[![tokio-pipe](https://docs.rs/tokio-pipe/badge.svg)](https://docs.rs/tokio-pipe)
[![dependency status](https://deps.rs/repo/github/yskszk63/tokio-pipe/status.svg)](https://deps.rs/repo/github/yskszk63/tokio-pipe)

Asynchronous pipe(2) library using tokio.

## Example

```rust
use tokio::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut r, mut w) = tokio_pipe::pipe()?;

    w.write_all(b"HELLO, WORLD!").await?;

    let mut buf = [0; 16];
    let len = r.read(&mut buf[..]).await?;

    assert_eq!(&buf[..len], &b"HELLO, WORLD!"[..]);
    Ok(())
}
```

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
