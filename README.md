# Convinience Release
* Use with care
* If you don't know what you are doing, use the [original tokio-stomp](https://crates.io/crates/tokio-stomp)
* Added sending and receiving headers

# tokio-stomp-2
[![crates.io](https://img.shields.io/crates/v/tokio-stomp-2.svg)](https://crates.io/crates/tokio-stomp-2)
[![codecov](https://codecov.io/gh/alexkunde/tokio-stomp-2/branch/main/graph/badge.svg?token=L3BUYHJIQN)](https://codecov.io/gh/alexkunde/tokio-stomp-2)  
An async [STOMP](https://stomp.github.io/) client (and maybe eventually, server) for Rust, using the Tokio stack.

It aims to be fast and fully-featured with a simple streaming interface.

## Examples

Sending a message to a queue.

```rust
use futures::prelude::*;
use async_stomp::client;
use async_stomp::ToServer;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
  let mut conn = client::connect("127.0.0.1:61613", None, None).await.unwrap();
  
  conn.send(
    ToServer::Send {
        destination: "queue.test".into(),
        transaction: None,
        headers: vec!(),
        body: Some(b"Hello there rustaceans!".to_vec()),
    }
    .into(),
  )
  .await.expect("sending message to server");
  Ok(())
}
```

Receiving a message from a queue.

```rust
use futures::prelude::*;
use async_stomp::client;
use async_stomp::FromServer;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
  let mut conn = client::connect("127.0.0.1:61613", None, None).await.unwrap();
  conn.send(client::subscribe("queue.test", "custom-subscriber-id")).await.unwrap();

  while let Some(item) = conn.next().await {
    if let FromServer::Message { message_id,body, .. } = item.unwrap().content {
      println!("{:?}", body);
      println!("{}", message_id);
    }
  }
  Ok(())
}
```

For full examples, see the examples directory.

License: [MIT](LICENSE)
