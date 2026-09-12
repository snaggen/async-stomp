use crate::frame;
use crate::{AckMode, FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};
use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;
use rustls::pki_types::ServerName;
use std::fmt;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::codec::{Decoder, Encoder, Framed};
use winnow::Partial;
use winnow::error::ErrMode;
use winnow::stream::Offset;

/// The primary transport type used by STOMP clients
///
/// This is a `Framed` instance that handles encoding and decoding of STOMP frames
/// over either a plain TCP connection or a TLS connection.
pub type ClientTransport = Framed<TransportStream, ClientCodec>;

/// Enum representing the transport stream, which can be either a plain TCP connection or a TLS connection
///
/// This type abstracts over the two possible connection types to provide a uniform interface
/// for the rest of the library. It implements AsyncRead and AsyncWrite to handle all IO operations.
#[allow(clippy::large_enum_variant)]
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
pub struct Connector<S = Unset, V = Unset> {
    server: S,
    virtualhost: V,
    login: Option<String>,
    passcode: Option<String>,
    headers: Vec<(String, String)>,
    use_tls: bool,
    tls_server_name: Option<String>,
    max_frame_size: usize,
}

/// A required builder field that has not been set yet
pub struct Unset;

impl Connector<Unset, Unset> {
    /// Start configuring a connection
    ///
    /// `server` and `virtualhost` are required; everything else has a default.
    /// Finish with [`connect`](Connector::connect) to open the connection, or
    /// with [`msg`](Connector::msg) for just the CONNECT frame.
    ///
    /// ```rust
    /// use async_stomp::ToServer;
    /// use async_stomp::client::Connector;
    ///
    /// let msg = Connector::builder()
    ///     .server("stomp.example.com:61613")
    ///     .virtualhost("stomp.example.com")
    ///     .login("guest".to_string())
    ///     .msg();
    ///
    /// let ToServer::Connect { host, login, .. } = msg.content else {
    ///     panic!("expected a CONNECT");
    /// };
    /// assert_eq!(host, "stomp.example.com");
    /// assert_eq!(login.as_deref(), Some("guest"));
    /// ```
    ///
    /// Leaving out a required field is a compile error:
    ///
    /// ```compile_fail
    /// # use async_stomp::client::Connector;
    /// // No virtualhost, so there is nothing to connect with
    /// let conn = Connector::builder().server("stomp.example.com:61613").connect();
    /// ```
    pub fn builder() -> Connector<Unset, Unset> {
        Connector {
            server: Unset,
            virtualhost: Unset,
            login: None,
            passcode: None,
            headers: Vec::new(),
            use_tls: false,
            tls_server_name: None,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        }
    }
}

impl<S, V> Connector<S, V> {
    /// Address of the STOMP server, for instance `"localhost:61613"`
    pub fn server<T: tokio::net::ToSocketAddrs + Clone>(self, server: T) -> Connector<T, V> {
        Connector {
            server,
            virtualhost: self.virtualhost,
            login: self.login,
            passcode: self.passcode,
            headers: self.headers,
            use_tls: self.use_tls,
            tls_server_name: self.tls_server_name,
            max_frame_size: self.max_frame_size,
        }
    }

    /// Virtual host to connect to, sent as the `host` header. When the server
    /// has no virtual hosts, use the same host name as `server`.
    pub fn virtualhost<T: Into<String> + Clone>(self, virtualhost: T) -> Connector<S, T> {
        Connector {
            server: self.server,
            virtualhost,
            login: self.login,
            passcode: self.passcode,
            headers: self.headers,
            use_tls: self.use_tls,
            tls_server_name: self.tls_server_name,
            max_frame_size: self.max_frame_size,
        }
    }

    /// Username, if the server requires authentication
    pub fn login(mut self, login: String) -> Self {
        self.login = Some(login);
        self
    }

    /// Password, if the server requires authentication
    pub fn passcode(mut self, passcode: String) -> Self {
        self.passcode = Some(passcode);
        self
    }

    /// Extra headers for the CONNECT frame, such as a broker specific client id
    pub fn headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Connect over TLS. Defaults to `false`.
    pub fn use_tls(mut self, use_tls: bool) -> Self {
        self.use_tls = use_tls;
        self
    }

    /// Give up on a frame from the server once it grows past this many bytes.
    /// Defaults to [`DEFAULT_MAX_FRAME_SIZE`].
    ///
    /// The bound is what stops a broken or hostile `content-length` from
    /// growing the read buffer until the process runs out of memory. Raise it
    /// only alongside the matching setting on the broker.
    pub fn max_frame_size(mut self, max_frame_size: usize) -> Self {
        self.max_frame_size = max_frame_size;
        self
    }

    /// Host name to verify the TLS certificate against. Defaults to the host
    /// from `server`.
    pub fn tls_server_name(mut self, tls_server_name: String) -> Self {
        self.tls_server_name = Some(tls_server_name);
        self
    }
}

impl<S: tokio::net::ToSocketAddrs + Clone, V: Into<String> + Clone> Connector<S, V> {
    /// Creates a TLS connector with default trust anchors
    ///
    /// This method configures a TLS connector with the system's default trust anchors
    /// for certificate verification.
    async fn create_tls_connector(&self) -> Result<TlsConnector> {
        // Create a root certificate store with webpki's built-in roots
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };

        // Create a TLS client configuration with the root certificates
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
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

            // Create a copy of server_name to avoid the borrow after move issue
            let server_name_copy = server_name.clone();

            // Try to parse the server name as an IP address first
            let dns_name = if let Ok(ip_addr) = server_name_copy.parse::<IpAddr>() {
                // Handle IP address
                match ip_addr {
                    IpAddr::V4(ipv4) => ServerName::IpAddress(ipv4.into()),
                    IpAddr::V6(ipv6) => ServerName::IpAddress(ipv6.into()),
                }
            } else {
                // Handle DNS name
                ServerName::DnsName(
                    server_name_copy
                        .try_into()
                        .map_err(|_| anyhow!("Invalid DNS name: {}", server_name))?,
                )
            };

            // Connect with TLS
            let tls_stream = tls_connector.connect(dns_name, tcp).await?;
            TransportStream::Tls(tls_stream)
        } else {
            // Use plain TCP
            TransportStream::Plain(tcp)
        };

        // Create a framed transport with the STOMP codec
        let mut transport =
            ClientCodec::with_max_frame_size(self.max_frame_size).framed(transport_stream);

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
    let Some(FromServer::Connected { version, .. }) = msg.as_ref().map(|m| &m.content) else {
        return Err(anyhow!("unexpected reply: {:?}", msg));
    };

    // CONNECT only ever offers 1.2, so another version back means the server
    // ignored the negotiation. Running on regardless would mean acknowledging
    // and escaping by rules this client does not implement, and the resulting
    // misbehaviour is far harder to trace than a refused connection.
    if version != "1.2" {
        return Err(anyhow!(
            "server negotiated STOMP {version}, this client speaks 1.2"
        ));
    }
    Ok(())
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
pub struct Subscriber<S = Unset, I = Unset> {
    destination: S,
    id: I,
    ack: Option<AckMode>,
    headers: Vec<(String, String)>,
}

impl Subscriber<Unset, Unset> {
    /// Start configuring a subscription
    ///
    /// `destination` and `id` are required; the headers are optional.
    ///
    /// ```rust
    /// use async_stomp::ToServer;
    /// use async_stomp::client::Subscriber;
    ///
    /// let msg = Subscriber::builder()
    ///     .destination("queue.test")
    ///     .id("sub-1")
    ///     .subscribe();
    ///
    /// let ToServer::Subscribe { destination, id, .. } = msg.content else {
    ///     panic!("expected a SUBSCRIBE");
    /// };
    /// assert_eq!(destination, "queue.test");
    /// assert_eq!(id, "sub-1");
    /// ```
    ///
    /// Leaving out a required field is a compile error:
    ///
    /// ```compile_fail
    /// # use async_stomp::client::Subscriber;
    /// // No id, so there is nothing to subscribe with
    /// let msg = Subscriber::builder().destination("queue.test").subscribe();
    /// ```
    pub fn builder() -> Subscriber<Unset, Unset> {
        Subscriber {
            destination: Unset,
            id: Unset,
            ack: None,
            headers: Vec::new(),
        }
    }
}

impl<S, I> Subscriber<S, I> {
    /// The queue or topic to subscribe to
    pub fn destination<T: Into<String>>(self, destination: T) -> Subscriber<T, I> {
        Subscriber {
            destination,
            id: self.id,
            ack: self.ack,
            headers: self.headers,
        }
    }

    /// Identifier for this subscription, chosen by you. Incoming messages
    /// carry it, and [`ToServer::Unsubscribe`] refers back to it.
    pub fn id<T: Into<String>>(self, id: T) -> Subscriber<S, T> {
        Subscriber {
            destination: self.destination,
            id,
            ack: self.ack,
            headers: self.headers,
        }
    }

    /// How messages on this subscription are acknowledged
    ///
    /// Leaving it unset means [`AckMode::Auto`], where the broker considers a
    /// message delivered the moment it sends it. The other modes require an
    /// [`ToServer::Ack`] per message, built from the `ack` field of the
    /// [`FromServer::Message`](crate::FromServer::Message).
    ///
    /// ```rust
    /// use async_stomp::AckMode;
    /// use async_stomp::client::Subscriber;
    ///
    /// let msg = Subscriber::builder()
    ///     .destination("queue.test")
    ///     .id("sub-1")
    ///     .ack(AckMode::ClientIndividual)
    ///     .subscribe();
    /// ```
    pub fn ack(mut self, ack: AckMode) -> Self {
        self.ack = Some(ack);
        self
    }

    /// Extra headers for the SUBSCRIBE frame, such as a broker specific
    /// subscription name
    pub fn headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
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
            ack: self.ack,
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

/// Largest frame this client will buffer, unless told otherwise
///
/// Matches the default ActiveMQ and Artemis accept, so a message the broker
/// took is a message this client can read.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 104_857_600;

/// Codec for encoding/decoding STOMP protocol frames for client usage
///
/// This codec handles the conversion between STOMP protocol frames and Rust types,
/// implementing the tokio_util::codec::Encoder and Decoder traits.
#[derive(Debug)]
pub struct ClientCodec {
    /// Refuse to keep buffering once an unfinished frame passes this size
    max_frame_size: usize,
}

impl Default for ClientCodec {
    fn default() -> Self {
        ClientCodec {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        }
    }
}

impl ClientCodec {
    /// A codec bounded by [`DEFAULT_MAX_FRAME_SIZE`]
    pub fn new() -> Self {
        Self::default()
    }

    /// A codec that gives up on a frame once it grows past `max_frame_size`
    ///
    /// The bound is what stops a broken or hostile `content-length` from
    /// growing the read buffer until the process runs out of memory.
    ///
    /// ```rust
    /// use async_stomp::client::ClientCodec;
    ///
    /// let codec = ClientCodec::with_max_frame_size(1024 * 1024);
    /// ```
    pub fn with_max_frame_size(max_frame_size: usize) -> Self {
        ClientCodec { max_frame_size }
    }
}

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
            Err(ErrMode::Incomplete(_)) => {
                // More data is wanted. If the unfinished frame is already over
                // the bound, no amount of further reading makes it acceptable,
                // so stop before the buffer grows any further.
                if src.len() > self.max_frame_size {
                    bail!("Frame exceeds the maximum of {} bytes", self.max_frame_size);
                }
                return Ok(None);
            }
            Err(e) => bail!("Parse failed: {:?}", e), // Parsing error
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
        AckMode, Message, ToServer,
        client::{ClientCodec, Connector, Subscriber},
    };
    use bytes::BytesMut;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::codec::Decoder;

    /// Tests the creation of a STOMP subscription message
    ///
    /// This test validates that a subscription message created using the Subscriber builder
    /// contains the correct destination, ID, and custom headers. It verifies that the
    /// subscription message serializes to the same byte sequence as a manually constructed
    /// equivalent message.
    ///
    /// If this test fails, it means the Subscriber builder is not correctly constructing
    /// STOMP SUBSCRIBE frames according to the protocol specification, which would cause
    /// client subscriptions to fail or behave incorrectly when connecting to a STOMP server.
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

    /// Tests the creation of a STOMP connection message
    ///
    /// This test validates that a connection message created using the Connector builder
    /// contains the correct server, virtualhost, login credentials, and custom headers.
    /// It verifies that the connection message serializes to the same byte sequence as
    /// a manually constructed equivalent message.
    ///
    /// If this test fails, it means the Connector builder is not correctly constructing
    /// STOMP CONNECT frames according to the protocol specification, which would cause
    /// client connections to fail when attempting to connect to a STOMP server.
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

    /// Tests that the optional builder fields are left out when not set
    ///
    /// Everything but the server and the virtualhost is optional, and an
    /// option that was never asked for must not turn up in the frame.
    ///
    /// If this test fails, the client sends credentials or headers the caller
    /// never supplied, which brokers may well reject.
    #[test]
    fn connection_message_defaults() {
        let connect_msg = Connector::builder()
            .server("stomp.example.com")
            .virtualhost("virtual.stomp.example.com")
            .msg();

        let mut buffer = BytesMut::new();
        connect_msg.to_frame().serialize(&mut buffer);
        let frame = String::from_utf8_lossy(&buffer);

        assert!(!frame.contains("login:"), "{frame}");
        assert!(!frame.contains("passcode:"), "{frame}");
        assert!(frame.contains("host:virtual.stomp.example.com"), "{frame}");
    }

    /// Tests that setting a field twice keeps the value set last
    ///
    /// The builder lets any field be set again, so the later call has to win
    /// rather than the value being merged or the first one kept.
    ///
    /// If this test fails, a connector assembled in steps, where a later step
    /// overrides an earlier default, connects with the wrong values.
    #[test]
    fn later_setter_calls_win() {
        let connect_msg = Connector::builder()
            .server("first.example.com")
            .server("second.example.com")
            .virtualhost("first.example.com")
            .virtualhost("second.example.com")
            .login("first".to_string())
            .login("second".to_string())
            .msg();

        let mut buffer = BytesMut::new();
        connect_msg.to_frame().serialize(&mut buffer);
        let frame = String::from_utf8_lossy(&buffer);

        assert!(frame.contains("host:second.example.com"), "{frame}");
        assert!(frame.contains("login:second"), "{frame}");
    }

    /// Tests that the handshake puts the same CONNECT frame on the wire that
    /// `msg` returns
    ///
    /// The handshake and `msg` build that frame separately, so the frame a
    /// caller inspects beforehand is only the one actually sent for as long as
    /// the two agree.
    ///
    /// If this test fails, the two have drifted apart and connections are made
    /// with headers other than the ones `msg` reports.
    #[tokio::test]
    async fn handshake_sends_the_connect_message() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Bind the test server");
        let addr = listener.local_addr().expect("Test server address");

        // A server that accepts one connection, keeps whatever frame it is
        // sent, and answers it with CONNECTED
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("Accept a connection");

            // A frame ends with a null byte, so read until one turns up rather
            // than trusting the whole frame to arrive in a single read
            let mut received = Vec::new();
            while !received.ends_with(b"\0") {
                let mut chunk = [0u8; 256];
                let len = socket
                    .read(&mut chunk)
                    .await
                    .expect("Read the CONNECT frame");
                assert!(len > 0, "The client closed before sending a whole frame");
                received.extend_from_slice(&chunk[..len]);
            }

            socket
                .write_all(b"CONNECTED\nversion:1.2\n\n\0")
                .await
                .expect("Reply CONNECTED");
            received
        });

        // Two identically configured connectors: one to connect with, one to
        // read the expected frame off of
        let connector = || {
            Connector::builder()
                .server(addr.to_string())
                .virtualhost("virtual.stomp.example.com")
                .login("guest_login".to_string())
                .passcode("guest_passcode".to_string())
                .headers(vec![("client-id".to_string(), "ClientTest".to_string())])
        };

        connector()
            .connect()
            .await
            .expect("Connect to the test server");

        let mut expected = BytesMut::new();
        connector().msg().to_frame().serialize(&mut expected);

        assert_eq!(server.await.expect("The test server"), expected.to_vec());
    }

    /// Tests that the ack mode reaches the SUBSCRIBE frame
    ///
    /// The mode decides whether the broker expects an ACK per message, so
    /// getting it onto the wire is what makes the difference between a message
    /// that is redelivered after a crash and one that is lost.
    ///
    /// If this test fails, a subscription asking for explicit acknowledgment
    /// silently runs in auto mode instead.
    #[test]
    fn subscribe_sets_the_ack_mode() {
        let msg = Subscriber::builder()
            .destination("queue.test")
            .id("sub-1")
            .ack(AckMode::ClientIndividual)
            .subscribe();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);
        let frame = String::from_utf8_lossy(&buffer);

        assert!(frame.contains("ack:client-individual"), "{frame}");
    }

    /// Tests that no ack mode is sent unless one was asked for
    ///
    /// Leaving the header off means the server's default, auto, which is what
    /// every caller of this crate has had so far.
    ///
    /// If this test fails, existing subscriptions change acknowledgment mode
    /// under their callers.
    #[test]
    fn subscribe_leaves_the_ack_mode_out_by_default() {
        let msg = Subscriber::builder()
            .destination("queue.test")
            .id("sub-1")
            .subscribe();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);
        let frame = String::from_utf8_lossy(&buffer);

        assert!(!frame.contains("ack:"), "{frame}");
    }

    /// Reads one whole frame, which ends at the first null byte
    async fn read_frame(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut received = Vec::new();
        while !received.ends_with(b"\0") {
            let mut chunk = [0u8; 256];
            let len = socket.read(&mut chunk).await.expect("Read a frame");
            if len == 0 {
                break;
            }
            received.extend_from_slice(&chunk[..len]);
        }
        received
    }

    /// Tests that a CONNECTED naming another version is refused
    ///
    /// CONNECT only ever offers 1.2, so a different version back means the
    /// server ignored the negotiation. Carrying on would mean acknowledging and
    /// escaping by rules this client does not implement.
    ///
    /// If this test fails, such a session is accepted and misbehaves later, far
    /// from the cause.
    #[tokio::test]
    async fn handshake_refuses_a_version_it_did_not_ask_for() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Bind the test server");
        let addr = listener.local_addr().expect("Test server address");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("Accept a connection");
            read_frame(&mut socket).await;
            socket
                .write_all(b"CONNECTED\nversion:1.1\n\n\0")
                .await
                .expect("Reply CONNECTED");
            // Hold the socket open long enough for the client to read the frame
            tokio::time::sleep(Duration::from_millis(500)).await;
        });

        let error = Connector::builder()
            .server(addr.to_string())
            .virtualhost("test")
            .connect()
            .await
            .expect_err("A CONNECTED naming 1.1 should be refused");

        assert!(error.to_string().contains("1.1"), "{error}");
    }

    /// Tests that an unfinished frame past the bound is refused
    ///
    /// A content-length far larger than anything real would otherwise have the
    /// decoder wait, and the read buffer grow, until the process runs out of
    /// memory.
    ///
    /// If this test fails, a broken or hostile length header can exhaust the
    /// client's memory.
    #[test]
    fn a_frame_growing_past_the_bound_is_refused() {
        let mut codec = ClientCodec::with_max_frame_size(64);
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(
            b"MESSAGE\ndestination:/queue/a\nmessage-id:1\nsubscription:s\ncontent-length:999999999\n\n",
        );
        buffer.extend_from_slice(&[b'x'; 200]);

        assert!(
            codec.decode(&mut buffer).is_err(),
            "An unfinished frame past the bound should be refused"
        );
    }

    /// Tests that an unfinished frame under the bound is simply waited on
    ///
    /// The bound must not turn ordinary fragmentation, where a frame arrives
    /// over several reads, into an error.
    ///
    /// If this test fails, any message split across TCP segments fails to
    /// arrive.
    #[test]
    fn an_unfinished_frame_under_the_bound_waits_for_more() {
        let mut codec = ClientCodec::with_max_frame_size(1024);
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(b"MESSAGE\ndestination:/queue/a\nmessage-id:1\n");

        assert!(
            codec
                .decode(&mut buffer)
                .expect("A partial frame is not an error")
                .is_none(),
            "The decoder should ask for more data"
        );
    }
}
