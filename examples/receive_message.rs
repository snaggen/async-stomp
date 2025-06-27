use async_stomp::FromServer;
use async_stomp::client::Connector;
use async_stomp::client::Subscriber;
use futures::prelude::*;

// You can start a simple STOMP server with docker:
// `docker run -p 61613:61613 -p 8161:8161 rmohr/activemq:latest`
// activemq web interface starts at: http://localhost:8161

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

    conn.send(subscribe).await.unwrap();

    while let Some(item) = conn.next().await {
        if let FromServer::Message {
            message_id, body, ..
        } = item.unwrap().content
        {
            println!("{body:?}");
            println!("{message_id}");
        }
    }
    Ok(())
}
