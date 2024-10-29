# async-stomp
An async [STOMP](https://stomp.github.io/) client (and maybe eventually, server) for Rust, using the Tokio stack.

This is a fork of async-stomp-2, with the purpose of getting some basic maintenance going.

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

License: [EUPL](LICENSE)
