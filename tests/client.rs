use async_stomp::{
    client::{Connector, Subscriber},
    FromServer, Message, ToServer,
};
use futures::{future::ok, prelude::*};
use std::time::Duration;

// Test to send a message
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

// Test to receive a message
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

// Test to send and receive message
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
