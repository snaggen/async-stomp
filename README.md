# async-stomp

[![crates.io](https://img.shields.io/crates/v/async-stomp.svg)](https://crates.io/crates/async-stomp)
[![docs.rs](https://docs.rs/async-stomp/badge.svg)](https://docs.rs/async-stomp/latest/async_stomp/)
[![codecov](https://codecov.io/github/snaggen/async-stomp/graph/badge.svg?token=HEY5300T6X)](https://codecov.io/github/snaggen/async-stomp)

An async [STOMP](https://stomp.github.io/) library for Rust, using the Tokio stack.

This is a fork of [tokio-stomp-2](https://github.com/alexkunde/tokio-stomp-2), with the purpose of getting some basic maintenance going.

## Overview

This library provides a fully asynchronous Rust implementation of the STOMP (Simple/Streaming Text Oriented Messaging Protocol) 1.2 protocol. It allows Rust applications to communicate with message brokers like ActiveMQ, RabbitMQ, and others that support the STOMP protocol.

The library is built on top of Tokio, leveraging its async I/O capabilities to provide non-blocking message handling.

## Features

- Async STOMP client for Rust using Tokio
- Support for all STOMP operations:
  - Connection management (connect, disconnect)
  - Message publishing
  - Subscriptions
  - Acknowledgments (auto, client, client-individual modes)
  - Transactions
- TLS/SSL support for secure connections
- Custom headers support for advanced configurations
- Built-in heartbeat support
- Error handling with detailed error information

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
async-stomp = "0.6"
```

## Usage Guide

### Connecting to a STOMP Server

The primary entry point is the `Connector` builder, which allows you to configure and establish a connection:

```rust
use async_stomp::client::Connector;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Basic connection
    let connection = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;
        
    // Connection with custom headers
    let connection_with_headers = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .headers(vec![("client-id".to_string(), "my-client".to_string())])
        .connect()
        .await?;
        
    Ok(())
}
```

### Sending a message to a queue

```rust
use futures::prelude::*;
use async_stomp::client::Connector;
use async_stomp::ToServer;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;

    // Send a message with body
    conn.send(
        ToServer::Send {
            destination: "queue.test".into(),
            transaction: None,
            headers: None,
            body: Some(b"Hello there rustaceans!".to_vec()),
        }
        .into(),
    )
    .await?;
    
    // Send a message with custom headers
    conn.send(
        ToServer::Send {
            destination: "queue.test".into(),
            transaction: None,
            headers: Some(vec![
                ("content-type".to_string(), "text/plain".to_string()),
                ("priority".to_string(), "high".to_string())
            ]),
            body: Some(b"Important message!".to_vec()),
        }
        .into(),
    )
    .await?;
    
    Ok(())
}
```

### Subscribing to a queue and receiving messages

```rust
use futures::prelude::*;
use async_stomp::client::Connector;
use async_stomp::client::Subscriber;
use async_stomp::FromServer;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;

    // Create and send a subscription message
    let subscribe = Subscriber::builder()
        .destination("queue.test")
        .id("subscription-1")
        .subscribe();

    conn.send(subscribe).await?;

    // Process incoming messages
    while let Some(message) = conn.next().await {
        match message {
            Ok(msg) => {
                if let FromServer::Message { 
                    destination, 
                    message_id, 
                    body, 
                    ..
                } = msg.content {
                    println!("Received message from {}", destination);
                    println!("Message ID: {}", message_id);
                    
                    if let Some(body) = body {
                        println!("Body: {}", String::from_utf8_lossy(&body));
                    }
                    
                    // Process the message...
                }
            },
            Err(e) => {
                eprintln!("Error receiving message: {:?}", e);
                break;
            }
        }
    }
    
    Ok(())
}
```

### Using transactions

Transactions allow you to group multiple operations together and commit or abort them as a unit:

```rust
use futures::prelude::*;
use async_stomp::{client::Connector, ToServer};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("127.0.0.1:61613")
        .virtualhost("/")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .connect()
        .await?;

    // Start a transaction
    let transaction_id = "tx-1";
    conn.send(ToServer::Begin { 
        transaction: transaction_id.to_string() 
    }.into()).await?;
    
    // Send messages within the transaction
    conn.send(
        ToServer::Send {
            destination: "queue.test".into(),
            transaction: Some(transaction_id.to_string()),
            headers: None,
            body: Some(b"Message 1 in transaction".to_vec()),
        }
        .into(),
    ).await?;
    
    conn.send(
        ToServer::Send {
            destination: "queue.test".into(),
            transaction: Some(transaction_id.to_string()),
            headers: None,
            body: Some(b"Message 2 in transaction".to_vec()),
        }
        .into(),
    ).await?;
    
    // Commit the transaction
    conn.send(ToServer::Commit { 
        transaction: transaction_id.to_string() 
    }.into()).await?;
    
    // Example of starting another transaction and aborting it
    let transaction_id = "tx-2";
    conn.send(ToServer::Begin { 
        transaction: transaction_id.to_string() 
    }.into()).await?;
    
    conn.send(
        ToServer::Send {
            destination: "queue.test".into(),
            transaction: Some(transaction_id.to_string()),
            headers: None,
            body: Some(b"This message will be aborted".to_vec()),
        }
        .into(),
    ).await?;
    
    // Abort the transaction
    conn.send(ToServer::Abort { 
        transaction: transaction_id.to_string() 
    }.into()).await?;
    
    Ok(())
}
```

### Secure Connection with TLS/SSL

```rust
use futures::prelude::*;
use async_stomp::client::Connector;
use async_stomp::client::Subscriber;
use async_stomp::ToServer;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let server_address = "secure-stomp-server.example.com:61614";
    
    // Create a secure connection with TLS enabled
    let mut conn = Connector::builder()
        .server(server_address)
        .virtualhost("secure-stomp-server.example.com")
        .login("guest".to_string())
        .passcode("guest".to_string())
        .use_tls(true)                                    // Enable TLS
        .tls_server_name("secure-stomp-server.example.com") // Server name for certificate validation
        .connect()
        .await?;
        
    // Use the connection as normal for subscribing and sending messages
    let subscribe = Subscriber::builder()
        .destination("secure.topic")
        .id("secure-subscription")
        .subscribe();
        
    conn.send(subscribe).await?;
    
    // Send a message securely
    conn.send(
        ToServer::Send {
            destination: "secure.topic".into(),
            transaction: None,
            headers: None,
            body: Some(b"This is a secure message!".to_vec()),
        }
        .into(),
    ).await?;
    
    Ok(())
}
```

## Advanced Usage

### Acknowledgment Modes

STOMP offers different acknowledgment modes for message consumption:

```rust
use async_stomp::{AckMode, client::{Connector, Subscriber}, ToServer};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .connect()
        .await?;

    // Subscribe with client acknowledgment mode
    let subscribe_client_ack = Subscriber::builder()
        .destination("queue.important")
        .id("sub-with-ack")
        .headers(vec![("ack".to_string(), "client".to_string())])
        .subscribe();
        
    conn.send(subscribe_client_ack).await?;
    
    // When processing messages, acknowledge them manually
    // (assuming you've received a message with ID "message-123")
    conn.send(
        ToServer::Ack {
            id: "message-123".to_string(),
            transaction: None,
        }
        .into()
    ).await?;
    
    // Or negative-acknowledge if processing failed
    conn.send(
        ToServer::Nack {
            id: "message-456".to_string(),
            transaction: None,
        }
        .into()
    ).await?;
    
    Ok(())
}
```

STOMP supports three acknowledgment modes:

1. **Auto** (default if not specified)
   - Messages are automatically acknowledged by the client as soon as they are received
   - No explicit acknowledgment is required
   - Example: `vec![("ack".to_string(), "auto".to_string())]`

2. **Client**
   - The client must explicitly acknowledge messages
   - An ACK acknowledges all messages received so far on the connection
   - Example: `vec![("ack".to_string(), "client".to_string())]`

3. **Client-Individual**
   - The client must explicitly acknowledge each individual message
   - Each message must be acknowledged separately
   - Example: `vec![("ack".to_string(), "client-individual".to_string())]`

#### **Example with Auto Acknowledgment (Default)**

```rust
// Default subscription uses auto acknowledgment
let subscribe_auto = Subscriber::builder()
    .destination("queue.standard")
    .id("sub-auto")
    .subscribe();
    
conn.send(subscribe_auto).await?;

// No need to acknowledge messages - they're auto-acknowledged
```

#### **Example with Client-Individual Acknowledgment**

```rust
// Subscribe with client-individual acknowledgment mode
let subscribe_individual = Subscriber::builder()
    .destination("queue.critical")
    .id("sub-individual")
    .headers(vec![("ack".to_string(), "client-individual".to_string())])
    .subscribe();
    
conn.send(subscribe_individual).await?;

// Process messages in a loop
while let Some(message) = conn.next().await {
    if let Ok(msg) = message {
        if let FromServer::Message { message_id, body, .. } = msg.content {
            // Process the message...
            println!("Processing message: {}", message_id);
            
            // Individual acknowledgment after successful processing
            conn.send(
                ToServer::Ack {
                    id: message_id,
                    transaction: None,
                }
                .into()
            ).await?;
        }
    }
}
```

#### **Using Transactions with Acknowledgments**

You can also combine transactions with acknowledgments to ensure atomic processing:

```rust
// Start a transaction
let transaction_id = "tx-1";
conn.send(ToServer::Begin { 
    transaction: transaction_id.to_string() 
}.into()).await?;

// Acknowledge multiple messages within the transaction
conn.send(
    ToServer::Ack {
        id: "message-123".to_string(),
        transaction: Some(transaction_id.to_string()),
    }
    .into()
).await?;

conn.send(
    ToServer::Ack {
        id: "message-124".to_string(), 
        transaction: Some(transaction_id.to_string()),
    }
    .into()
).await?;

// Commit the transaction to finalize all acknowledgments
conn.send(ToServer::Commit { 
    transaction: transaction_id.to_string() 
}.into()).await?;
```

### Connection Lifecycle Management

```rust
use futures::prelude::*;
use async_stomp::{client::Connector, ToServer};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .connect()
        .await?;
        
    // Use the connection...
    
    // Gracefully disconnect when done
    conn.send(
        ToServer::Disconnect {
            receipt: Some("disconnect-receipt".to_string())
        }
        .into()
    ).await?;
    
    // Wait for the RECEIPT frame if requested
    if let Some(msg) = conn.next().await {
        // Check for receipt...
    }
    
    Ok(())
}
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

Licensed under the [EUPL](LICENSE.EUPL).
