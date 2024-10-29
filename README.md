# async-stomp
An async [STOMP](https://stomp.github.io/) client (and maybe eventually, server) for Rust, using the Tokio stack.

This is a fork of [tokio-stomp-2](https://github.com/alexkunde/tokio-stomp-2), with the purpose of getting some basic maintenance going.

## Examples

Sending a message to a queue.

```rust
use futures::prelude::*;
use async_stomp::client::Connector;
use async_stomp::ToServer;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
  let mut conn = Connector::builder()
    .server("127.0.0.1:61613")
    .virtualhost("/")
    .login("guest".to_string())
    .passcode("guest".to_string())
    .connect()
    .await
    .unwrap();

    conn.send(
      ToServer::Send {
        destination: "queue.test".into(),
        transaction: None,
        headers: None,
        body: Some(b"Hello there rustaceans!".to_vec()),
      }
      .into(),
    )
    .await
    .expect("sending message to server");
    Ok(())
}
```

Receiving a message from a queue.
```rust
use futures::prelude::*;
use async_stomp::client::Connector;
use async_stomp::client::Subscriber;
use async_stomp::FromServer;

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
  let mut conn = Connector::builder()
    .server("127.0.0.1:61613")
    .virtualhost("/")
    .login("guest".to_string())
    .passcode("guest".to_string())
    .connect()
    .await
    .unwrap();

  let subscribe = Subscriber::builder()
    .destination("queue.test")
    .id("custom-subscriber-id")
    .subscribe();

  conn.send(subscribe)
    .await
    .unwrap();

  while let Some(item) = conn.next().await {
    if let FromServer::Message { message_id, body, .. } = item.unwrap().content {
        println!("{:?}", body);
        println!("{}", message_id);
    }
  }
  Ok(())
}
```

For full examples, see the examples directory.

License: [EUPL](LICENSE)
