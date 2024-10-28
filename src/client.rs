use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;

use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};
use winnow::error::ErrMode;
use winnow::stream::Offset;
use winnow::Partial;

pub type ClientTransport = Framed<TcpStream, ClientCodec>;

use crate::frame;
use crate::{FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};

/// Connect to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
///
/// `virtualhost` If no specific virtualhost is desired, it is recommended
/// to set this to the same as the host name that the socket
/// was established against (i.e, the same as the server address).
pub async fn connect(
    server: impl tokio::net::ToSocketAddrs,
    virtualhost: impl Into<String>,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<ClientTransport> {
    let tcp = TcpStream::connect(server).await?;
    let mut transport = ClientCodec.framed(tcp);
    client_handshake(&mut transport, virtualhost.into(), login, passcode).await?;
    Ok(transport)
}

async fn client_handshake(
    transport: &mut ClientTransport,
    virtualhost: String,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<()> {
    let connect = Message {
        content: ToServer::Connect {
            accept_version: "1.2".into(),
            host: virtualhost,
            login,
            passcode,
            heartbeat: None,
        },
        extra_headers: vec![],
    };
    // Send the message
    transport.send(connect).await?;
    // Receive reply
    let msg = transport.next().await.transpose()?;
    if let Some(FromServer::Connected { .. }) = msg.as_ref().map(|m| &m.content) {
        Ok(())
    } else {
        Err(anyhow!("unexpected reply: {:?}", msg))
    }
}

/// Convenience function to build a Subscribe message
pub fn subscribe(dest: impl Into<String>, id: impl Into<String>) -> Message<ToServer> {
    ToServer::Subscribe {
        destination: dest.into(),
        id: id.into(),
        ack: None,
    }
    .into()
}

pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Message<FromServer>;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        let buf = &mut Partial::new(src.chunk());
        let item = match frame::parse_frame(buf) {
            Ok(frame) => Message::<FromServer>::from_frame(frame),
            Err(ErrMode::Incomplete(_)) => return Ok(None),
            Err(e) => bail!("Parse failed: {:?}", e),
        };
        let len = buf.offset_from(&Partial::new(src.chunk()));
        src.advance(len);
        item.map(Some)
    }
}

impl Encoder<Message<ToServer>> for ClientCodec {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        item: Message<ToServer>,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        item.to_frame().serialize(dst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{future::ok, SinkExt, StreamExt, TryStreamExt};

    use crate::{client, FromServer, ToServer};

    // Test to send a message
    #[tokio::test]
    #[ignore]
    async fn test_session() {
        let mut conn = crate::client::connect(
            "localhost:61613",
            "/",
            //None,None
            Some("artemis".to_string()),
            Some("artemis".to_string()),
        )
        .await
        .expect("Default connection to localhost");
        let msg = crate::Message {
            content: ToServer::Send {
                destination: "/test/a".to_string(),
                transaction: None,
                headers: Some(vec![("header-a".to_string(), "value-a".to_string())]),
                body: Some("This is a test message".as_bytes().to_vec()),
            },
            extra_headers: vec![],
        };
        conn.send(msg).await.expect("Send a");
        let msg = crate::Message {
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

    // Test to recieve a message
    #[tokio::test]
    #[ignore]
    async fn test_subscribe() {
        let sub_msg = crate::client::subscribe("/test/a", "tjo");
        let mut conn = crate::client::connect(
            "localhost:61613",
            "/",
            //None,None
            Some("artemis".to_string()),
            Some("artemis".to_string()),
        )
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
                        String::from_utf8_lossy(&body.unwrap())
                    );
                } else {
                    println!("{:?}", item);
                }
                ok(())
            })
            .await;
    }

    // Test to send and recieve message
    #[tokio::test]
    #[ignore]
    async fn test_send_subscribe() {
        let conn = client::connect(
            "127.0.0.1:61613",
            "/".to_string(),
            "artemis".to_string().into(),
            "artemis".to_string().into(),
        )
        .await
        .expect("Connect");

        tokio::time::sleep(Duration::from_millis(200)).await;

        let (mut sink, stream) = conn.split();

        let fut1 = async move {
            sink.send(client::subscribe("rusty", "myid")).await?;
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
                //    println!("{:?}", item);
            }
            ok(())
        });

        futures::future::select(Box::pin(fut1), Box::pin(fut2))
            .await
            .factor_first()
            .0
            .expect("Select");
    }
}
