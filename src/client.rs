use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;

use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};
use typed_builder::TypedBuilder;
use winnow::error::ErrMode;
use winnow::stream::Offset;
use winnow::Partial;

pub type ClientTransport = Framed<TcpStream, ClientCodec>;

use crate::frame;
use crate::{FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};

/// Create a connection to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
///
/// `virtualhost` If no specific virtualhost is desired, it is recommended
/// to set this to the same as the host name that the socket
/// was established against (i.e, the same as the server address).
///
/// # Examples
///
/// ```rust,no_run
/// use async_stomp::client::Connector;
///
///#[tokio::main]
/// async fn main() {
///   let connection = Connector::builder()
///     .server("stomp.example.com")
///     .virtualhost("stomp.example.com")
///     .login("guest".to_string())
///     .passcode("guest".to_string())
///     .connect()
///     .await;
///}
/// ```
#[derive(TypedBuilder)]
#[builder(build_method(vis="", name=__build))]
pub struct Connector<S: tokio::net::ToSocketAddrs, V: Into<String>> {
    /// The address to the stomp server
    server: S,
    /// Virtualhost, if no specific virtualhost is desired, it is recommended
    /// to set this to the same as the host name that the socket
    virtualhost: V,
    /// Username to use for optional authentication to the server
    #[builder(default, setter(strip_option))]
    login: Option<String>,
    /// Passcode to use for optional authentication to the server
    #[builder(default, setter(strip_option))]
    passcode: Option<String>,
    /// Custom headers to be sent to the server
    #[builder(default)]
    headers: Vec<(String, String)>,
}

#[allow(non_camel_case_types)]
impl<
        S: tokio::net::ToSocketAddrs,
        V: Into<String>,
        __login: ::typed_builder::Optional<Option<String>>,
        __passcode: ::typed_builder::Optional<Option<String>>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
    > ConnectorBuilder<S, V, ((S,), (V,), __login, __passcode, __headers)>
{
    pub async fn connect(self) -> Result<ClientTransport> {
        let connector = self.__build();
        connector.connect().await
    }

    pub fn msg(self) -> Message<ToServer> {
        let connector = self.__build();
        connector.msg()
    }
}

impl<S: tokio::net::ToSocketAddrs, V: Into<String>> Connector<S, V> {
    pub async fn connect(self) -> Result<ClientTransport> {
        let tcp = TcpStream::connect(self.server).await?;
        let mut transport = ClientCodec.framed(tcp);
        client_handshake(
            &mut transport,
            self.virtualhost.into(),
            self.login,
            self.passcode,
            self.headers,
        )
        .await?;
        Ok(transport)
    }

    pub fn msg(self) -> Message<ToServer> {
        let extra_headers = self
            .headers
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        Message {
            content: ToServer::Connect {
                accept_version: "1.2".into(),
                host: self.virtualhost.into(),
                login: self.login,
                passcode: self.passcode,
                heartbeat: None,
            },
            extra_headers,
        }
    }
}

async fn client_handshake(
    transport: &mut ClientTransport,
    virtualhost: String,
    login: Option<String>,
    passcode: Option<String>,
    headers: Vec<(String, String)>,
) -> Result<()> {
    let extra_headers = headers
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    let connect = Message {
        content: ToServer::Connect {
            accept_version: "1.2".into(),
            host: virtualhost,
            login,
            passcode,
            heartbeat: None,
        },
        extra_headers,
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

/// Builder to create a Subscribe message with optional custom headers
///
/// # Examples
///
/// ```rust,no_run
/// use futures::prelude::*;
/// use async_stomp::client::Connector;
/// use async_stomp::client::Subscriber;
///
///
/// #[tokio::main]
/// async fn main() -> Result<(), anyhow::Error> {
///   let mut connection = Connector::builder()
///     .server("stomp.example.com")
///     .virtualhost("stomp.example.com")
///     .login("guest".to_string())
///     .passcode("guest".to_string())
///     .headers(vec![("client-id".to_string(), "ClientTest".to_string())])
///     .connect()
///     .await.expect("Client connection");
///   
///   let subscribe_msg = Subscriber::builder()
///     .destination("queue.test")
///     .id("custom-subscriber-id")
///     .subscribe();
///
///   connection.send(subscribe_msg).await?;
///   Ok(())
/// }
/// ```
#[derive(TypedBuilder)]
#[builder(build_method(vis="", name=__build))]
pub struct Subscriber<S: Into<String>, I: Into<String>> {
    destination: S,
    id: I,
    #[builder(default)]
    headers: Vec<(String, String)>,
}

#[allow(non_camel_case_types)]
impl<
        S: Into<String>,
        I: Into<String>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
    > SubscriberBuilder<S, I, ((S,), (I,), __headers)>
{
    pub fn subscribe(self) -> Message<ToServer> {
        let subscriber = self.__build();
        subscriber.subscribe()
    }
}

impl<S: Into<String>, I: Into<String>> Subscriber<S, I> {
    pub fn subscribe(self) -> Message<ToServer> {
        let mut msg: Message<ToServer> = ToServer::Subscribe {
            destination: self.destination.into(),
            id: self.id.into(),
            ack: None,
        }
        .into();
        msg.extra_headers = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        msg
    }
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

    use bytes::BytesMut;
    use futures::{future::ok, SinkExt, StreamExt, TryStreamExt};

    use crate::{
        client::{Connector, Subscriber},
        FromServer, Message, ToServer,
    };

    #[test]
    fn subscription_message() {
        let headers = vec![(
            "activemq.subscriptionName".to_string(),
            "ClientTest".to_string(),
        )];
        let subscribe_msg = Subscriber::builder()
            .destination("queue.test")
            .id("custom-subscriber-id")
            .headers(headers.clone())
            .subscribe();
        let mut expected: Message<ToServer> = ToServer::Subscribe {
            destination: "queue.test".to_string(),
            id: "custom-subscriber-id".to_string(),
            ack: None,
        }
        .into();
        expected.extra_headers = headers
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();

        let mut expected_buffer = BytesMut::new();
        expected.to_frame().serialize(&mut expected_buffer);
        let mut actual_buffer = BytesMut::new();
        subscribe_msg.to_frame().serialize(&mut actual_buffer);

        assert_eq!(expected_buffer, actual_buffer);
    }

    #[test]
    fn connection_message() {
        let headers = vec![("client-id".to_string(), "ClientTest".to_string())];
        let connect_msg = Connector::builder()
            .server("stomp.example.com")
            .virtualhost("virtual.stomp.example.com")
            .login("guest_login".to_string())
            .passcode("guest_passcode".to_string())
            .headers(headers.clone())
            .msg();

        let mut expected: Message<ToServer> = ToServer::Connect {
            accept_version: "1.2".into(),
            host: "virtual.stomp.example.com".into(),
            login: Some("guest_login".to_string()),
            passcode: Some("guest_passcode".to_string()),
            heartbeat: None,
        }
        .into();
        expected.extra_headers = headers
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();

        let mut expected_buffer = BytesMut::new();
        expected.to_frame().serialize(&mut expected_buffer);
        let mut actual_buffer = BytesMut::new();
        connect_msg.to_frame().serialize(&mut actual_buffer);

        assert_eq!(expected_buffer, actual_buffer);
    }

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
                    String::from_utf8_lossy(&body.unwrap())
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
}
