use async_stomp::client::{Connector, Subscriber};
use async_stomp::{FromServer, ToServer};
use futures::prelude::*;
use std::time::Duration;

// This example subscribes to a destination and then sits idle, relying on
// heart-beats to keep the connection open. Without them a broker with an idle
// timeout, such as Apache Artemis with its 60 second default connection TTL,
// would drop the session long before a message arrives.
//
// You can start a simple STOMP server with docker:
// `docker run -p 61613:61613 rmohr/activemq:latest`

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        // We can beat at most every 5 seconds, and would like the server to do
        // the same so that we notice if it goes away
        .heartbeat(5_000, 5_000)
        .connect()
        .await?;

    // The handshake may well have settled on something slower than we asked
    // for, or on nothing at all if the server declined
    let (outgoing, incoming) = conn.heartbeat();
    println!("Negotiated heart-beats: outgoing {outgoing:?}, incoming {incoming:?}");

    let subscribe = Subscriber::builder()
        .destination("rusty")
        .id("myid")
        .subscribe();
    conn.send(subscribe).await?;

    // Nothing else is sent from here on. The background task keeps the session
    // alive, and a server that stops beating makes the stream yield an error.
    loop {
        match tokio::time::timeout(Duration::from_secs(30), conn.next()).await {
            Ok(Some(Ok(msg))) => {
                if let FromServer::Message { body, .. } = msg.content {
                    println!(
                        "Received: {}",
                        String::from_utf8_lossy(&body.unwrap_or_default())
                    );
                }
            }
            Ok(Some(Err(e))) => {
                eprintln!("Connection lost: {e}");
                break;
            }
            Ok(None) => {
                eprintln!("Server closed the connection");
                break;
            }
            Err(_) => println!("Still connected, nothing received in the last 30s"),
        }
    }

    conn.send(ToServer::Disconnect { receipt: None }.into())
        .await?;
    Ok(())
}
