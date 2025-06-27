use std::time::Duration;

use async_stomp::*;
use client::{Connector, Subscriber};
use futures::future::ok;
use futures::prelude::*;

// The example connects to a local server, then sends the following messages -
// subscribe to a destination, send a message to the destination, unsubscribe and disconnect
// It simultaneously receives and prints messages sent from the server, which in fact means
// it ends up printing it's own message.

// You can start a simple STOMP server with docker:
// `docker run -p 61613:61613 rmohr/activemq:latest`

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (mut sink, stream) = conn.split();

    let fut1 = async move {
        let subscribe = Subscriber::builder()
            .destination("rusty")
            .id("myid")
            .subscribe();
        sink.send(subscribe).await?;
        println!("Subscribe sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        sink.send(
            ToServer::Send {
                destination: "rusty".into(),
                transaction: None,
                headers: None,
                body: Some(b"Hello there rustaceans!".to_vec()),
            }
            .into(),
        )
        .await?;
        println!("Message sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        sink.send(ToServer::Unsubscribe { id: "myid".into() }.into())
            .await?;
        println!("Unsubscribe sent");

        tokio::time::sleep(Duration::from_millis(200)).await;

        tokio::time::sleep(Duration::from_secs(1)).await;
        sink.send(ToServer::Disconnect { receipt: None }.into())
            .await?;
        println!("Disconnect sent");

        Ok(())
    };

    // Listen from the main thread. Once the Disconnect message is sent from
    // the sender thread, the server will disconnect the client and the future
    // will resolve, ending the program
    let fut2 = stream.try_for_each(|item| {
        if let FromServer::Message { body, .. } = item.content {
            println!(
                "Message received: {:?}",
                String::from_utf8_lossy(&body.unwrap())
            );
        } else {
            println!("{item:?}");
        }
        ok(())
    });

    futures::future::select(Box::pin(fut1), Box::pin(fut2))
        .await
        .factor_first()
        .0
}
