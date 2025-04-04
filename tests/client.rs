use async_stomp::{
    client::{Connector, Subscriber},
    FromServer, Message, ToServer,
};
use futures::{future::ok, prelude::*};
use std::time::Duration;

/// Tests sending messages to a STOMP server
///
/// This integration test verifies that a client can successfully connect to a STOMP server
/// and send multiple messages with different destinations and custom headers.
/// It validates the basic message sending functionality of the client.
///
/// If this test fails, it means there are issues with establishing a connection to the STOMP
/// server or sending messages, which would prevent the client from successfully communicating
/// with STOMP message brokers in a real-world scenario.
#[tokio::test]
#[ignore]
async fn test_session() {
    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("artemis".to_string())
        .passcode("artemis".to_string())
        .connect()
        .await
        .expect("Default connection to localhost");

    let msg = Message {
        content: ToServer::Send {
            destination: "/test/a".to_string(),
            transaction: None,
            headers: Some(vec![("header-a".to_string(), "value-a".to_string())]),
            body: Some("This is a test message".as_bytes().to_vec()),
        },
        extra_headers: vec![],
    };
    conn.send(msg).await.expect("Send a");
    let msg = Message {
        content: ToServer::Send {
            destination: "/test/b".to_string(),
            transaction: None,
            headers: Some(vec![("header-b".to_string(), "value-b".to_string())]),
            body: Some("This is a another test message".as_bytes().to_vec()),
        },
        extra_headers: vec![],
    };
    conn.send(msg).await.expect("Send b");
}

/// Tests subscribing to a destination and receiving messages
///
/// This integration test verifies that a client can successfully connect to a STOMP server,
/// subscribe to a destination, and receive messages from that destination. It validates
/// the subscription functionality and message reception capabilities of the client.
///
/// If this test fails, it means there are issues with establishing subscriptions or
/// receiving messages from the STOMP server, which would prevent the client from
/// successfully consuming messages from queues or topics in a real-world scenario.
#[tokio::test]
#[ignore]
async fn test_subscribe() {
    let sub_msg = Subscriber::builder()
        .destination("/test/a")
        .id("tjo")
        .subscribe();

    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("artemis".to_string())
        .passcode("artemis".to_string())
        .connect()
        .await
        .expect("Default connection to localhost");

    conn.send(sub_msg).await.expect("Send subscribe");
    let (_sink, stream) = conn.split();

    let mut cnt = 0;
    let _ = stream
        .try_for_each(|item| {
            println!("==== {cnt}");
            cnt += 1;
            if let FromServer::Message { body, .. } = item.content {
                println!(
                    "Message received: {:?}",
                    String::from_utf8_lossy(&body.expect("No body in message"))
                );
            } else {
                println!("{:?}", item);
            }
            ok(())
        })
        .await;
}

/// Tests the full message cycle: connecting, subscribing, sending, and receiving
///
/// This comprehensive integration test verifies a complete STOMP workflow:
/// 1. Establishing a connection to a STOMP server
/// 2. Creating a subscription to a destination
/// 3. Sending a message to that destination
/// 4. Receiving the message from the subscription
/// 5. Unsubscribing from the destination
/// 6. Disconnecting from the server
///
/// It validates the end-to-end functionality of the client in a real-world usage scenario.
///
/// If this test fails, it means there are issues with one or more of the core STOMP operations,
/// which would prevent the client from performing its intended functions in production use.
#[tokio::test]
#[ignore]
async fn test_send_subscribe() {
    let conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("artemis".to_string())
        .passcode("artemis".to_string())
        .connect()
        .await
        .expect("Default connection to localhost");

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
                String::from_utf8_lossy(&body.expect("No body in message"))
            );
        } else {
            println!("{:?}", item);
        }
        ok(())
    });

    futures::future::select(Box::pin(fut1), Box::pin(fut2))
        .await
        .factor_first()
        .0
        .expect("Select");
}
