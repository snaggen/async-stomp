use async_stomp::FromServer;
use async_stomp::client::{Connector, Subscriber};
use futures::prelude::*;

// This example demonstrates receiving messages from a STOMP server using TLS

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let server = "secure-stomp-server.example.com:61614";

    println!("Connecting to secure STOMP server with TLS...");

    // Create a connection with TLS enabled
    let mut conn = Connector::builder()
        .server(server)
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .use_tls(true) // Enable TLS
        .tls_server_name("secure-stomp-server.example.com".to_string()) // Server name for certificate validation
        .connect()
        .await?;

    println!("Connected to server");

    // Subscribe to a topic
    let subscribe = Subscriber::builder()
        .destination("secure.topic")
        .id("tls-subscriber-id")
        .subscribe();

    conn.send(subscribe).await?;
    println!("Subscribed to secure.topic");

    // Receive and process messages
    println!("Waiting for messages...");
    while let Some(message_result) = conn.next().await {
        match message_result {
            Ok(message) => {
                if let FromServer::Message {
                    message_id,
                    body,
                    destination,
                    ..
                } = message.content
                {
                    if let Some(content) = body {
                        println!(
                            "Received message (id: {}) from {}: {}",
                            message_id,
                            destination,
                            String::from_utf8_lossy(&content)
                        );
                    } else {
                        println!("Received empty message (id: {message_id}) from {destination}");
                    }
                } else {
                    println!("Received non-message frame: {message:?}");
                }
            }
            Err(e) => println!("Error receiving message: {e}"),
        }
    }

    // The connection was closed
    println!("Connection closed");
    Ok(())
}
