use crate::frame;
use crate::{FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};
use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tokio_util::codec::{Decoder, Encoder, Framed};
use typed_builder::TypedBuilder;
use winnow::error::ErrMode;
use winnow::stream::Offset;
use winnow::Partial;

/// The primary transport type used by STOMP clients
///
/// This is a `Framed` instance that handles encoding and decoding of STOMP frames
/// over either a plain TCP connection or a TLS connection.
pub type ClientTransport = Framed<TransportStream, ClientCodec>;

/// Enum representing the transport stream, which can be either a plain TCP connection or a TLS connection
///
/// This type abstracts over the two possible connection types to provide a uniform interface
/// for the rest of the library. It implements AsyncRead and AsyncWrite to handle all IO operations.
pub enum TransportStream {
    /// A plain, unencrypted TCP connection
    Plain(TcpStream),
    /// A secure TLS connection over TCP
    Tls(TlsStream<TcpStream>),
}

// Implement AsyncRead for TransportStream to allow reading data from either connection type
impl tokio::io::AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Delegate to the appropriate inner stream type
        match self.get_mut() {
            TransportStream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            TransportStream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

// Implement AsyncWrite for TransportStream to allow writing data to either connection type
impl tokio::io::AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // Delegate to the appropriate inner stream type
        match self.get_mut() {
            TransportStream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            TransportStream::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Delegate to the appropriate inner stream type
        match self.get_mut() {
            TransportStream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            TransportStream::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Delegate to the appropriate inner stream type
        match self.get_mut() {
            TransportStream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            TransportStream::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

// Debug implementation for TransportStream that provides a human-readable representation
impl fmt::Debug for TransportStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportStream::Plain(_) => write!(f, "Plain TCP connection"),
            TransportStream::Tls(_) => write!(f, "TLS connection"),
        }
    }
}

/// A builder for creating and establishing STOMP connections to a server
///
/// This struct provides a builder pattern for configuring the connection
/// parameters and then connecting to a STOMP server.
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
pub struct Connector<S: tokio::net::ToSocketAddrs + Clone, V: Into<String> + Clone> {
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
    /// Whether to use TLS for this connection
    #[builder(default = false)]
    use_tls: bool,
    /// Optional server name to verify in TLS certificate (defaults to hostname from server if not specified)
    #[builder(default, setter(strip_option))]
    tls_server_name: Option<String>,
}

/// Implementation of the builder connect method to allow the builder to directly connect
#[allow(non_camel_case_types)]
impl<
        S: tokio::net::ToSocketAddrs + Clone,
        V: Into<String> + Clone,
        __login: ::typed_builder::Optional<Option<String>>,
        __passcode: ::typed_builder::Optional<Option<String>>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
        __use_tls: ::typed_builder::Optional<bool>,
        __tls_server_name: ::typed_builder::Optional<Option<String>>,
    >
    ConnectorBuilder<
        S,
        V,
        (
            (S,),
            (V,),
            __login,
            __passcode,
            __headers,
            __use_tls,
            __tls_server_name,
        ),
    >
{
    /// Connect to the STOMP server using the configured parameters
    ///
    /// This method finalizes the builder and attempts to establish a connection
    /// to the STOMP server. If successful, it returns a ClientTransport that can
    /// be used to send and receive messages.
    pub async fn connect(self) -> Result<ClientTransport> {
        let connector = self.__build();
        connector.connect().await
    }

    /// Create a Message for connection without actually connecting
    ///
    /// This can be used when you want to handle the connection process manually
    /// or need access to the raw connection message.
    pub fn msg(self) -> Message<ToServer> {
        let connector = self.__build();
        connector.msg()
    }
}

impl<S: tokio::net::ToSocketAddrs + Clone, V: Into<String> + Clone> Connector<S, V> {
    /// Creates a TLS connector with default trust anchors
    ///
    /// This method configures a TLS connector with the system's default trust anchors
    /// for certificate verification.
    async fn create_tls_connector(&self) -> Result<TlsConnector> {
        // Create an empty root certificate store
        let mut root_cert_store = RootCertStore::empty();

        // Add webpki default trust anchors (CA certificates)
        root_cert_store.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));

        // Create a TLS client configuration with the root certificates
        let config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_cert_store)
            .with_no_client_auth();

        Ok(TlsConnector::from(Arc::new(config)))
    }

    /// Connect to the STOMP server using the configured parameters
    ///
    /// This method establishes a connection to the STOMP server and performs
    /// the STOMP protocol handshake. If successful, it returns a ClientTransport
    /// that can be used to send and receive STOMP messages.
    pub async fn connect(self) -> Result<ClientTransport> {
        // First establish a TCP connection to the server
        let tcp = TcpStream::connect(self.server.clone()).await?;

        // Determine whether to use plain TCP or wrap with TLS
        let transport_stream = if self.use_tls {
            // Extract server name for TLS verification
            let server_name = if let Some(name) = &self.tls_server_name {
                name.clone()
            } else {
                // Extract the hostname from the server address
                let server_addr = tcp.peer_addr()?;
                let hostname = server_addr.ip().to_string();
                if hostname.is_empty() {
                    return Err(anyhow!(
                        "Could not determine server hostname for TLS verification"
                    ));
                }
                hostname
            };

            // Create TLS connector
            let tls_connector = self.create_tls_connector().await?;

            // Convert string to DNS name for TLS verification
            let dns_name = rustls::ServerName::try_from(server_name.as_str())
                .map_err(|_| anyhow!("Invalid DNS name: {}", server_name))?;

            // Connect with TLS
            let tls_stream = tls_connector.connect(dns_name, tcp).await?;
            TransportStream::Tls(tls_stream)
        } else {
            // Use plain TCP
            TransportStream::Plain(tcp)
        };

        // Create a framed transport with the STOMP codec
        let mut transport = ClientCodec.framed(transport_stream);

        // Perform the STOMP protocol handshake
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

    /// Create a CONNECT message without actually connecting
    ///
    /// This method creates a STOMP CONNECT message using the configured parameters
    /// which can be used to establish a connection manually.
    pub fn msg(self) -> Message<ToServer> {
        // Convert custom headers to the binary format expected by the protocol
        let extra_headers = self
            .headers
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();

        // Create the CONNECT message
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

/// Performs the STOMP protocol handshake with the server
///
/// This function sends a CONNECT frame to the server and waits for
/// a CONNECTED response. If the server responds with anything else,
/// the handshake is considered failed.
async fn client_handshake(
    transport: &mut ClientTransport,
    virtualhost: String,
    login: Option<String>,
    passcode: Option<String>,
    headers: Vec<(String, String)>,
) -> Result<()> {
    // Convert custom headers to the binary format expected by the protocol
    let extra_headers = headers
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();

    // Create the CONNECT message
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

    // Send the message to the server
    transport.send(connect).await?;

    // Receive and process the server's reply
    let msg = transport.next().await.transpose()?;

    // Check if the reply is a CONNECTED frame
    if let Some(FromServer::Connected { .. }) = msg.as_ref().map(|m| &m.content) {
        Ok(())
    } else {
        Err(anyhow!("unexpected reply: {:?}", msg))
    }
}

/// Builder to create a Subscribe message with optional custom headers
///
/// This struct provides a builder pattern for configuring subscription parameters
/// and creating a SUBSCRIBE message to send to a STOMP server.
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
    /// The destination to subscribe to (e.g., queue or topic name)
    destination: S,
    /// The subscription ID used to identify this subscription
    id: I,
    /// Custom headers to be included in the SUBSCRIBE frame
    #[builder(default)]
    headers: Vec<(String, String)>,
}

/// Implementation of the builder subscribe method to allow direct subscription creation
#[allow(non_camel_case_types)]
impl<
        S: Into<String>,
        I: Into<String>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
    > SubscriberBuilder<S, I, ((S,), (I,), __headers)>
{
    /// Creates a SUBSCRIBE message using the configured parameters
    ///
    /// This method finalizes the builder and returns a STOMP SUBSCRIBE message
    /// that can be sent to a server to create a subscription.
    pub fn subscribe(self) -> Message<ToServer> {
        let subscriber = self.__build();
        subscriber.subscribe()
    }
}

impl<S: Into<String>, I: Into<String>> Subscriber<S, I> {
    /// Creates a SUBSCRIBE message using the configured parameters
    ///
    /// This method returns a STOMP SUBSCRIBE message that can be sent to a server
    /// to create a subscription with the configured destination, ID, and headers.
    pub fn subscribe(self) -> Message<ToServer> {
        // Create the basic Subscribe message
        let mut msg: Message<ToServer> = ToServer::Subscribe {
            destination: self.destination.into(),
            id: self.id.into(),
            ack: None,
        }
        .into();

        // Add any custom headers
        msg.extra_headers = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();

        msg
    }
}

/// Codec for encoding/decoding STOMP protocol frames for client usage
///
/// This codec handles the conversion between STOMP protocol frames and Rust types,
/// implementing the tokio_util::codec::Encoder and Decoder traits.
pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Message<FromServer>;
    type Error = anyhow::Error;

    /// Decodes bytes from the server into STOMP messages
    ///
    /// This method attempts to parse a complete STOMP frame from the input buffer.
    /// If a complete frame is available, it returns the parsed Message.
    /// If more data is needed, it returns None.
    /// If parsing fails, it returns an error.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        // Create a partial view of the buffer for parsing
        let buf = &mut Partial::new(src.chunk());

        // Attempt to parse a frame from the buffer
        let item = match frame::parse_frame(buf) {
            Ok(frame) => Message::<FromServer>::from_frame(frame),
            Err(ErrMode::Incomplete(_)) => return Ok(None), // Need more data
            Err(e) => bail!("Parse failed: {:?}", e),       // Parsing error
        };

        // Calculate how many bytes were consumed
        let len = buf.offset_from(&Partial::new(src.chunk()));

        // Advance the buffer past the consumed bytes
        src.advance(len);

        // Return the parsed message (or error)
        item.map(Some)
    }
}

impl Encoder<Message<ToServer>> for ClientCodec {
    type Error = anyhow::Error;

    /// Encodes STOMP messages for sending to the server
    ///
    /// This method serializes a STOMP message into bytes to be sent over the network.
    fn encode(
        &mut self,
        item: Message<ToServer>,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        // Convert the message to a frame and serialize it into the buffer
        item.to_frame().serialize(dst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        client::{Connector, Subscriber},
        Message, ToServer,
    };
    use bytes::BytesMut;

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
}
