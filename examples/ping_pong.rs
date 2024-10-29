use std::time::Duration;

use async_stomp::*;
use client::{Connector, Subscriber};
use futures::prelude::*;

// This examples consists of two futures, each of which connects to a local server,
// and then sends either PING or PONG messages to the server while listening
// for replies. This continues indefinitely (ctrl-c to exit)

// You can start a simple STOMP server with docker:
// `docker run -p 61613:61613 rmohr/activemq:latest`

async fn client(listens: &str, sends: &str, msg: &[u8]) -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;

    let subscribe = Subscriber::builder()
        .destination(listens)
        .id("myid")
        .subscribe();
    conn.send(subscribe).await?;

    loop {
        conn.send(
            ToServer::Send {
                destination: sends.into(),
                transaction: None,
                headers: None,
                body: Some(msg.to_vec()),
            }
            .into(),
        )
        .await?;
        let msg = conn.next().await.transpose()?;
        if let Some(FromServer::Message { body, .. }) = msg.as_ref().map(|m| &m.content) {
            println!("{}", String::from_utf8_lossy(body.as_ref().unwrap()));
        } else {
            anyhow::bail!("Unexpected: {:?}", msg)
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let fut1 = Box::pin(client("ping", "pong", b"PONG!"));
    let fut2 = Box::pin(client("pong", "ping", b"PING!"));

    let (res, _) = futures::future::select(fut1, fut2).await.factor_first();
    res
}
