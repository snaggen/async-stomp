use futures::prelude::*;
use async_stomp::client;
use async_stomp::FromServer;

// You can start a simple STOMP server with docker:
// `docker run -p 61613:61613 -p 8161:8161 rmohr/activemq:latest`
// activemq web interface starts at: http://localhost:8161

#[tokio::main]
async fn main() -> Result<(), std::io::Error> {
    let mut conn = client::connect(
        "127.0.0.1:61613",
        "/".to_string(),
        "guest".to_string().into(),
        "guest".to_string().into(),
    )
    .await
    .unwrap();

    conn.send(client::subscribe("queue.test", "custom-subscriber-id"))
        .await
        .unwrap();

    while let Some(item) = conn.next().await {
        if let FromServer::Message {
            message_id, body, ..
        } = item.unwrap().content
        {
            println!("{:?}", body);
            println!("{}", message_id);
        }
    }
    Ok(())
}
