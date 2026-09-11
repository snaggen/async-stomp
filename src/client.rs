//! The STOMP client: connecting, subscribing, sending and receiving.
//!
//! [`Connector`] opens a connection and gives you a [`ClientTransport`];
//! [`Subscriber`] builds the SUBSCRIBE message to send on it. [`ClientCodec`]
//! is exposed for driving STOMP over a socket this crate does not manage.

use crate::frame;
use crate::{AckMode, FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};
use bytes::{Buf, BufMut, BytesMut};
use futures::channel::{mpsc, oneshot};
use futures::prelude::*;
use futures::ready;
use futures::sink::SinkExt;
use rustls::pki_types::ServerName;
use std::fmt;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::codec::{Decoder, Encoder, Framed, FramedRead, FramedWrite};
use winnow::Partial;
use winnow::error::ErrMode;
use winnow::stream::Offset;

/// A framed STOMP connection that the caller reads and writes directly
type PlainFramed = Framed<Monitored<TransportStream>, ClientCodec>;

/// Read half of a framed STOMP connection whose write half is owned elsewhere
type FramedReader = FramedRead<Monitored<ReadHalf<TransportStream>>, ClientCodec>;

/// Write half of a framed STOMP connection
type FramedWriter = FramedWrite<WriteHalf<TransportStream>, ClientCodec>;

/// Something to put on the wire, either a frame or a bare heart-beat
enum Outgoing {
    Message(Message<ToServer>),
    Heartbeat,
}

/// An item queued for the write pump, paired with the channel used to report
/// the result of the write back to the caller
type PendingWrite = (Outgoing, oneshot::Sender<Result<()>>);

/// How a connection is wired up underneath
///
/// Only a connection that beats on its own needs a background task, and only a
/// connection with a background task needs its socket split. Everything else
/// reads and writes one framed socket directly, which is both the cheaper and
/// the longer established path.
// The direct variant holds the whole framed socket, which is large mostly
// because a TLS stream is. Boxing it would only add an indirection to every
// poll on the common path, and a transport is owned one at a time anyway.
#[allow(clippy::large_enum_variant)]
enum Transport {
    /// Nothing beats on its own: the caller drives the socket, as it always has
    Direct(PlainFramed),
    /// A write pump owns the write half and keeps the session alive
    Pumped { reader: FramedReader, pump: Pump },
}

impl Transport {
    /// When the server was last heard from, whichever half is doing the reading
    fn last_read(&self) -> Instant {
        match self {
            Transport::Direct(framed) => framed.get_ref().last_read,
            Transport::Pumped { reader, .. } => reader.get_ref().last_read,
        }
    }
}

/// The caller's end of a write pump
///
/// Items go to the pump one at a time, and the pump reports back what happened
/// to each. Waiting for that report is what lets `send` keep reporting its own
/// write result, now that the write no longer happens on the caller's task.
struct Pump {
    /// Hands outgoing items to the pump task.
    tx: mpsc::Sender<PendingWrite>,
    /// Set between handing an item over and the pump confirming the write.
    pending: Option<oneshot::Receiver<Result<()>>>,
}

impl Pump {
    /// Accept another item once the previous one has been settled
    ///
    /// Settling first is what keeps a write result from being dropped unseen,
    /// and is why a pumped connection does not batch.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.poll_flush(cx))?;
        self.tx
            .poll_ready(cx)
            .map_err(|_| anyhow!("Stomp connection closed"))
    }

    /// Hand an item over to the pump
    fn start_send(&mut self, item: Outgoing) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .start_send((item, ack_tx))
            .map_err(|_| anyhow!("Stomp connection closed"))?;
        self.pending = Some(ack_rx);
        Ok(())
    }

    /// Wait for the pump to report the result of the last write
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let Some(ack) = self.pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let result = ready!(Pin::new(ack).poll(cx));
        self.pending = None;
        // A cancelled oneshot means the pump is gone, and so is the socket.
        Poll::Ready(result.unwrap_or_else(|_| Err(anyhow!("Stomp connection closed"))))
    }

    /// Write one item and wait for the result
    async fn send(&mut self, item: Outgoing) -> Result<()> {
        future::poll_fn(|cx| self.poll_ready(cx)).await?;
        self.start_send(item)?;
        future::poll_fn(|cx| self.poll_flush(cx)).await
    }
}

/// An established STOMP connection, returned by [`Connector::connect`]
///
/// A `Sink` of [`Message<ToServer>`] and a `Stream` of [`Message<FromServer>`].
/// Use it directly, or split it with [`futures::StreamExt::split`] to send and
/// receive from different tasks.
///
/// `send` completes once the message has reached the socket and reports any
/// write error itself. On a heart-beating connection, which writes from a
/// background task, that means `feed` behaves like `send` rather than
/// buffering.
///
/// ```rust,no_run
/// use async_stomp::client::Connector;
/// use async_stomp::{FromServer, ToServer};
/// use futures::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), anyhow::Error> {
/// let conn = Connector::builder()
///     .server("127.0.0.1:61613")
///     .virtualhost("/")
///     .connect()
///     .await?;
///
/// let (mut sink, mut stream) = conn.split();
///
/// sink.send(ToServer::Send {
///     destination: "queue.test".into(),
///     transaction: None,
///     headers: None,
///     body: Some(b"hello".to_vec()),
/// }.into()).await?;
///
/// while let Some(msg) = stream.next().await {
///     println!("{:?}", msg?.content);
/// }
/// # Ok(())
/// # }
/// ```
pub struct ClientTransport {
    /// The socket, either driven directly or through a write pump.
    transport: Transport,
    /// Negotiated interval for our own heart-beats.
    outgoing: Option<Duration>,
    /// Negotiated interval for the server's heart-beats.
    incoming: Option<Duration>,
    /// How long the server may stay quiet, and a ticker to check it with.
    supervise: Option<(Duration, Interval)>,
}

impl ClientTransport {
    /// Take ownership of a connection once the handshake has settled
    fn new(
        transport: Transport,
        outgoing: Option<Duration>,
        incoming: Option<Duration>,
        automatic: bool,
    ) -> Self {
        // Allow twice the negotiated interval before declaring the server dead.
        // The spec asks receivers to apply an error margin for network delays.
        let supervise = automatic.then_some(incoming).flatten().map(|period| {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            (period * 2, ticker)
        });

        Self {
            transport,
            outgoing,
            incoming,
            supervise,
        }
    }

    /// The heart-beat intervals the handshake settled on, as
    /// `(outgoing, incoming)`
    ///
    /// `None` for a direction means it is off, either because you did not ask
    /// for it or because the server declined. Both sides have to agree, and the
    /// interval is then the slower of the two, so this can differ from what you
    /// requested.
    ///
    /// ```rust,no_run
    /// # use async_stomp::client::Connector;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), anyhow::Error> {
    /// let conn = Connector::builder()
    ///     .server("127.0.0.1:61613")
    ///     .virtualhost("/")
    ///     .heartbeat(20_000, 20_000)
    ///     .connect()
    ///     .await?;
    ///
    /// let (outgoing, incoming) = conn.heartbeat();
    /// println!("beating every {outgoing:?}, expecting every {incoming:?}");
    /// # Ok(())
    /// # }
    /// ```
    pub fn heartbeat(&self) -> (Option<Duration>, Option<Duration>) {
        (self.outgoing, self.incoming)
    }

    /// Send a single heart-beat now
    ///
    /// Only needed after `auto_heartbeat(false)`, which leaves the beating to
    /// you. Like sending a message, this completes once the write has reached
    /// the socket. Sending more of them than the negotiated interval calls for
    /// does no harm.
    ///
    /// ```rust,no_run
    /// # use async_stomp::client::Connector;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), anyhow::Error> {
    /// let mut conn = Connector::builder()
    ///     .server("127.0.0.1:61613")
    ///     .virtualhost("/")
    ///     .heartbeat(20_000, 20_000)
    ///     .auto_heartbeat(false)
    ///     .connect()
    ///     .await?;
    ///
    /// if let (Some(interval), _) = conn.heartbeat() {
    ///     tokio::time::sleep(interval / 2).await;
    ///     conn.send_heartbeat().await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_heartbeat(&mut self) -> Result<()> {
        match &mut self.transport {
            Transport::Direct(framed) => framed.send(Heartbeat).await,
            Transport::Pumped { pump, .. } => pump.send(Outgoing::Heartbeat).await,
        }
    }

    /// Fail the stream if the server has been quiet for longer than the
    /// negotiated incoming interval allows
    fn poll_server_timeout(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Message<FromServer>>>> {
        let Self {
            transport,
            supervise,
            ..
        } = self;
        let Some((tolerance, ticker)) = supervise.as_mut() else {
            return Poll::Pending;
        };

        // Drain the ticker so that it re-arms and registers our waker. The tick
        // itself carries no information, only the timestamp below does.
        while ticker.poll_tick(cx).is_ready() {}

        let quiet_for = transport.last_read().elapsed();
        if quiet_for > *tolerance {
            Poll::Ready(Some(Err(anyhow!(
                "No data from server for {quiet_for:?}, expected a heart-beat every {:?}",
                *tolerance / 2
            ))))
        } else {
            Poll::Pending
        }
    }
}

impl fmt::Debug for ClientTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTransport")
            .field("outgoing", &self.outgoing)
            .field("incoming", &self.incoming)
            .finish_non_exhaustive()
    }
}

impl Stream for ClientTransport {
    type Item = Result<Message<FromServer>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = match &mut this.transport {
            Transport::Direct(framed) => Pin::new(framed).poll_next(cx),
            Transport::Pumped { reader, .. } => Pin::new(reader).poll_next(cx),
        };
        match polled {
            Poll::Pending => this.poll_server_timeout(cx),
            ready => ready,
        }
    }
}

impl Sink<Message<ToServer>> for ClientTransport {
    type Error = anyhow::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match &mut self.get_mut().transport {
            Transport::Direct(framed) => {
                Sink::<Message<ToServer>>::poll_ready(Pin::new(framed), cx)
            }
            Transport::Pumped { pump, .. } => pump.poll_ready(cx),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Message<ToServer>) -> Result<()> {
        match &mut self.get_mut().transport {
            Transport::Direct(framed) => Pin::new(framed).start_send(item),
            Transport::Pumped { pump, .. } => pump.start_send(Outgoing::Message(item)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match &mut self.get_mut().transport {
            Transport::Direct(framed) => {
                Sink::<Message<ToServer>>::poll_flush(Pin::new(framed), cx)
            }
            Transport::Pumped { pump, .. } => pump.poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match &mut self.get_mut().transport {
            Transport::Direct(framed) => {
                Sink::<Message<ToServer>>::poll_close(Pin::new(framed), cx)
            }
            Transport::Pumped { pump, .. } => {
                ready!(pump.poll_flush(cx))?;
                // The pump sees the end of the channel and shuts the socket down.
                pump.tx.close_channel();
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Forwards outgoing messages to the socket and keeps the session alive
///
/// A heart-beat is only emitted when `interval` elapses with nothing to send.
/// Real traffic resets the timer implicitly, because the timeout is armed only
/// while the pump is waiting on the channel.
///
/// Each message is answered on its oneshot once it has been written and
/// flushed, so that the caller's `send` reports the actual write result.
async fn write_pump(
    mut writer: FramedWriter,
    mut rx: mpsc::Receiver<PendingWrite>,
    interval: Option<Duration>,
) {
    loop {
        let next = match interval {
            Some(period) => match tokio::time::timeout(period, rx.next()).await {
                Ok(next) => next,
                Err(_) => {
                    // Nothing to send within the interval, keep the session alive.
                    if writer.send(Heartbeat).await.is_err() {
                        break;
                    }
                    continue;
                }
            },
            None => rx.next().await,
        };

        // Every sender was dropped: the transport is gone, close the socket.
        let Some((item, ack)) = next else { break };

        let result = match item {
            Outgoing::Message(msg) => writer.send(msg).await,
            Outgoing::Heartbeat => writer.send(Heartbeat).await,
        };
        let failed = result.is_err();
        let _ = ack.send(result);
        if failed {
            break;
        }
    }

    // Closing only flushes and shuts the socket down, so the item type the
    // `Sink` impl is picked for does not matter, but it has to be named.
    let _ = SinkExt::<Heartbeat>::close(&mut writer).await;
}

/// Wraps the reading side of a socket so that the transport knows when the
/// server was last heard from
///
/// Per the STOMP spec any received data, a frame or a bare EOL, is an
/// indication that the remote end is alive, so stamping raw reads is enough.
/// Writing is passed straight through, for the connections that write through
/// the same object they read from.
struct Monitored<T> {
    inner: T,
    last_read: Instant,
}

impl<T> Monitored<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            last_read: Instant::now(),
        }
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Monitored<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            this.last_read = Instant::now();
        }
        poll
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Monitored<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The socket underneath a connection, plain TCP or TLS
///
/// [`Connector`] picks the variant from `use_tls` and owns it from then on, so
/// you rarely name this type. It is public for anyone driving [`ClientCodec`]
/// over a socket of their own.
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

/// Configures and opens a connection, yielding a [`ClientTransport`]
///
/// `server` and `virtualhost` are required; everything else has a default.
///
/// ```rust,no_run
/// use async_stomp::client::Connector;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), anyhow::Error> {
/// let conn = Connector::builder()
///     .server("stomp.example.com:61613")
///     .virtualhost("stomp.example.com")
///     .login("guest".to_string())
///     .passcode("guest".to_string())
///     .connect()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # Heart-beating
///
/// Off by default. Turning it on keeps an idle connection alive against brokers
/// that close quiet sessions, such as Apache Artemis with its 60 second default
/// connection TTL. The two values are milliseconds: the shortest interval you
/// can guarantee between your own transmissions, and the interval you would
/// like the server to transmit at. Zero disables that direction.
///
/// ```rust,no_run
/// # use async_stomp::client::Connector;
/// # #[tokio::main]
/// # async fn main() -> Result<(), anyhow::Error> {
/// let conn = Connector::builder()
///     .server("stomp.example.com:61613")
///     .virtualhost("stomp.example.com")
///     .heartbeat(20_000, 20_000)
///     .connect()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// Add `.auto_heartbeat(false)` to negotiate as usual but do the beating
/// yourself with [`ClientTransport::send_heartbeat`].
///
/// # TLS
///
/// ```rust,no_run
/// # use async_stomp::client::Connector;
/// # #[tokio::main]
/// # async fn main() -> Result<(), anyhow::Error> {
/// let conn = Connector::builder()
///     .server("stomp.example.com:61614")
///     .virtualhost("stomp.example.com")
///     .use_tls(true)
///     // Defaults to the host from `server` when left out
///     .tls_server_name("stomp.example.com".to_string())
///     .connect()
///     .await?;
/// # Ok(())
/// # }
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
    heartbeat: Option<(u32, u32)>,
    auto_heartbeat: bool,
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
            heartbeat: None,
            auto_heartbeat: true,
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
            heartbeat: self.heartbeat,
            auto_heartbeat: self.auto_heartbeat,
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
            heartbeat: self.heartbeat,
            auto_heartbeat: self.auto_heartbeat,
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

    /// Heart-beat intervals in milliseconds: the shortest interval you can
    /// guarantee between your own transmissions, and the interval you would
    /// like the server to transmit at. Zero disables that direction.
    ///
    /// Defaults to no heart-beating. Brokers may still close idle connections:
    /// Apache Artemis applies a 60 second connection TTL when the client asks
    /// for none.
    pub fn heartbeat(mut self, outgoing: u32, incoming: u32) -> Self {
        self.heartbeat = Some((outgoing, incoming));
        self
    }

    /// Handle heart-beating for you. Defaults to `true`.
    ///
    /// With `false` the `heart-beat` header is still sent and negotiated, but
    /// nothing beats on its own and a silent server is not reported. Read the
    /// outcome with [`ClientTransport::heartbeat`] and beat with
    /// [`ClientTransport::send_heartbeat`].
    pub fn auto_heartbeat(mut self, auto_heartbeat: bool) -> Self {
        self.auto_heartbeat = auto_heartbeat;
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

    /// Open the connection and perform the STOMP handshake
    ///
    /// Fails if the socket cannot be opened, if the server rejects the
    /// CONNECT, or if it replies with anything but CONNECTED.
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

        // Whether this connection will beat on its own is known from our own
        // request: the spec disables the direction outright when we offer a
        // zero interval, so the server's answer cannot turn it back on.
        let heartbeat = self.heartbeat;
        let automatic = self.auto_heartbeat;
        let beats = automatic && matches!(heartbeat, Some((offer, _)) if offer > 0);
        let max_frame_size = self.max_frame_size;

        let (transport, outgoing, incoming) = establish(
            transport_stream,
            self.msg(),
            heartbeat,
            beats,
            max_frame_size,
        )
        .await?;

        Ok(ClientTransport::new(
            transport, outgoing, incoming, automatic,
        ))
    }

    /// Build the CONNECT message without connecting
    ///
    /// For driving the handshake yourself. Note that heart-beating is then
    /// yours to handle too: only [`connect`](Self::connect) sets it up.
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
                heartbeat: self.heartbeat,
            },
            extra_headers,
        }
    }
}

/// Performs the STOMP protocol handshake and wires up the transport
///
/// A connection that beats on its own has to give its write half to a pump
/// task, and so has its socket split. Every other connection keeps a single
/// framed socket that the caller reads and writes directly.
///
/// Returns the transport along with the negotiated heart-beat intervals as
/// `(outgoing, incoming)`. `requested` repeats what the CONNECT frame asks for,
/// since the server's reply has to be negotiated against it.
async fn establish(
    stream: TransportStream,
    connect: Message<ToServer>,
    requested: Option<(u32, u32)>,
    beats: bool,
    max_frame_size: usize,
) -> Result<(Transport, Option<Duration>, Option<Duration>)> {
    let codec = || ClientCodec::with_max_frame_size(max_frame_size);

    if !beats {
        let mut framed = codec().framed(Monitored::new(stream));
        framed.send(connect).await?;
        let (outgoing, incoming) = accept_connected(framed.next().await.transpose()?, requested)?;
        return Ok((Transport::Direct(framed), outgoing, incoming));
    }

    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = FramedRead::new(Monitored::new(read_half), codec());
    let mut writer = FramedWrite::new(write_half, codec());

    writer.send(connect).await?;
    let (outgoing, incoming) = accept_connected(reader.next().await.transpose()?, requested)?;

    // A single slot is enough: the sink settles each item before accepting the
    // next one, so there is never more than one write in flight.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(write_pump(writer, rx, outgoing));

    let pump = Pump { tx, pending: None };
    Ok((Transport::Pumped { reader, pump }, outgoing, incoming))
}

/// Checks that the server accepted the connection and negotiates heart-beating
///
/// Anything other than a CONNECTED frame for STOMP 1.2 means the handshake
/// failed.
fn accept_connected(
    reply: Option<Message<FromServer>>,
    requested: Option<(u32, u32)>,
) -> Result<(Option<Duration>, Option<Duration>)> {
    let Some(FromServer::Connected {
        version, heartbeat, ..
    }) = reply.as_ref().map(|m| &m.content)
    else {
        return Err(anyhow!("unexpected reply: {:?}", reply));
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

    // An absent heart-beat header means the server wants none in either direction
    let (cx, cy) = requested.unwrap_or((0, 0));
    let (sx, sy) = match heartbeat {
        Some(hb) => frame::parse_heartbeat(hb)?,
        None => (0, 0),
    };

    Ok((negotiate(cx, sy), negotiate(sx, cy)))
}

/// Pick the effective heart-beat interval for one direction
///
/// Per the STOMP 1.2 spec the direction is disabled if either side declines,
/// and otherwise both sides settle on the slower of the two intervals.
fn negotiate(sender: u32, receiver: u32) -> Option<Duration> {
    (sender != 0 && receiver != 0).then(|| Duration::from_millis(sender.max(receiver).into()))
}

/// Close a connection the way the spec prescribes
///
/// Sends DISCONNECT with a receipt, waits for the matching RECEIPT, then shuts
/// the socket down. The receipt is the point: without it the client cannot tell
/// whether the server processed everything it was sent.
///
/// Frames still in flight arrive before the receipt and are discarded, so do
/// this once the caller is finished reading.
///
/// ```rust,no_run
/// use async_stomp::client::{disconnect, Connector};
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), anyhow::Error> {
/// let mut conn = Connector::builder()
///     .server("127.0.0.1:61613")
///     .virtualhost("/")
///     .connect()
///     .await?;
///
/// disconnect(&mut conn, "bye").await?;
/// # Ok(())
/// # }
/// ```
pub async fn disconnect(transport: &mut ClientTransport, receipt: impl Into<String>) -> Result<()> {
    let receipt = receipt.into();
    transport
        .send(
            ToServer::Disconnect {
                receipt: Some(receipt.clone()),
            }
            .into(),
        )
        .await?;

    while let Some(msg) = transport.next().await.transpose()? {
        match msg.content {
            FromServer::Receipt { receipt_id } if receipt_id == receipt => {
                transport.close().await?;
                return Ok(());
            }
            FromServer::Error { message, .. } => {
                bail!(
                    "Server refused the disconnect: {}",
                    message.unwrap_or_default()
                )
            }
            // Whatever was already on its way to us, which the disconnect
            // makes moot
            _ => continue,
        }
    }

    bail!("The server closed the connection without acknowledging the DISCONNECT")
}

/// Builds a SUBSCRIBE message
///
/// The `id` is yours to choose and is what [`ToServer::Unsubscribe`] refers
/// back to. Send the result on the transport like any other message.
///
/// ```rust
/// use async_stomp::client::Subscriber;
///
/// let subscribe = Subscriber::builder()
///     .destination("queue.test")
///     .id("sub-1")
///     // Anything the broker needs beyond destination, id and ack mode
///     .headers(vec![("activemq.subscriptionName".to_string(), "sub-1".to_string())])
///     .subscribe();
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
    /// Build the SUBSCRIBE message, ready to send on the transport
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

/// Translates between STOMP frames on the wire and [`Message`] values
///
/// [`Connector`] wires this up for you. Use it directly only to run the client
/// side of STOMP over a socket this crate does not manage. It decodes into
/// [`Message<FromServer>`] and encodes [`Message<ToServer>`] and [`Heartbeat`];
/// heart-beats arriving from the server are consumed silently.
///
/// ```rust
/// use async_stomp::FromServer;
/// use async_stomp::client::ClientCodec;
/// use bytes::BytesMut;
/// use tokio_util::codec::Decoder;
///
/// let mut buf = BytesMut::from(
///     &b"MESSAGE\ndestination:queue.test\nmessage-id:1\nsubscription:sub-1\n\nhi\0"[..],
/// );
///
/// let msg = ClientCodec::new().decode(&mut buf).unwrap().unwrap();
/// let FromServer::Message { destination, body, .. } = msg.content else {
///     panic!("expected a MESSAGE");
/// };
/// assert_eq!(destination, "queue.test");
/// assert_eq!(body.as_deref(), Some(&b"hi"[..]));
/// ```
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

    /// Decodes one frame from the server, or `None` if more bytes are needed
    ///
    /// Heart-beats are consumed here and never surface as a message.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        // A STOMP frame never starts with an EOL, so any leading newline is a
        // heart-beat from the server. Drop them here: left in the buffer they
        // would accumulate for the life of the session, and the parser only
        // tolerates a single one in front of the next real frame.
        let heartbeats = src
            .iter()
            .take_while(|&&b| b == b'\n' || b == b'\r')
            .count();
        src.advance(heartbeats);
        if src.is_empty() {
            return Ok(None);
        }

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

    /// Encodes a message as a STOMP frame
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

/// A STOMP heart-beat, for encoding with [`ClientCodec`]
///
/// A heart-beat is a bare end-of-line rather than a frame, so it is not a
/// [`ToServer`] variant. On a connection from [`Connector`] you never encode
/// one yourself: use [`ClientTransport::send_heartbeat`].
///
/// ```rust
/// use async_stomp::client::{ClientCodec, Heartbeat};
/// use bytes::BytesMut;
/// use tokio_util::codec::Encoder;
///
/// let mut buf = BytesMut::new();
/// ClientCodec::new().encode(Heartbeat, &mut buf).unwrap();
/// assert_eq!(&buf[..], b"\n");
/// ```
pub struct Heartbeat;

impl Encoder<Heartbeat> for ClientCodec {
    type Error = anyhow::Error;

    /// Encodes a heart-beat, which is a bare end-of-line rather than a frame
    fn encode(
        &mut self,
        _item: Heartbeat,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        dst.put_u8(b'\n');
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        AckMode, FromServer, Message, ToServer,
        client::{ClientCodec, Connector, Heartbeat, Subscriber, disconnect, negotiate},
    };
    use bytes::BytesMut;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::codec::{Decoder, Encoder};

    /// A complete MESSAGE frame, used to check that the decoder finds a frame
    /// that follows one or more heart-beats
    const MESSAGE_FRAME: &[u8] = b"MESSAGE\ndestination:d\nmessage-id:1\nsubscription:s\n\n\x00";

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
    /// A caller can inspect the frame with `msg` beforehand, which is only
    /// worth anything if it is the frame actually sent.
    ///
    /// If this test fails, connections are made with headers other than the
    /// ones `msg` reports.
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

    /// Tests that disconnecting sends a receipt and waits for it
    ///
    /// The spec's shutdown is DISCONNECT with a receipt, then wait for the
    /// RECEIPT before closing, so that the client knows the server processed
    /// everything it was sent.
    ///
    /// If this test fails, a clean shutdown closes the socket early and the
    /// last frames sent may never have been processed.
    #[tokio::test]
    async fn disconnect_asks_for_a_receipt_and_waits_for_it() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Bind the test server");
        let addr = listener.local_addr().expect("Test server address");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("Accept a connection");
            read_frame(&mut socket).await;
            socket
                .write_all(b"CONNECTED\nversion:1.2\n\n\0")
                .await
                .expect("Reply CONNECTED");

            let disconnect_frame = read_frame(&mut socket).await;
            socket
                .write_all(b"RECEIPT\nreceipt-id:bye\n\n\0")
                .await
                .expect("Reply RECEIPT");
            disconnect_frame
        });

        let mut conn = Connector::builder()
            .server(addr.to_string())
            .virtualhost("test")
            .connect()
            .await
            .expect("Connect to the test server");

        disconnect(&mut conn, "bye")
            .await
            .expect("Disconnect cleanly");

        let sent = server.await.expect("The test server");
        let sent = String::from_utf8_lossy(&sent);
        assert!(sent.starts_with("DISCONNECT\n"), "{sent}");
        assert!(sent.contains("receipt:bye"), "{sent}");
    }

    /// Tests that the configured heartbeat ends up in the CONNECT frame
    ///
    /// The builder takes the two intervals separately, but the protocol wants
    /// them as a single `heart-beat:x,y` header.
    ///
    /// If this test fails, the client either asks for the wrong intervals or
    /// none at all, and brokers that close idle connections, such as Apache
    /// Artemis, will drop the session.
    #[test]
    fn connection_message_with_heartbeat() {
        let connect_msg = Connector::builder()
            .server("stomp.example.com")
            .virtualhost("stomp.example.com")
            .heartbeat(5000, 10000)
            .msg();

        let mut buffer = BytesMut::new();
        connect_msg.to_frame().serialize(&mut buffer);

        let frame = String::from_utf8_lossy(&buffer);
        assert!(frame.contains("heart-beat:5000,10000"), "{frame}");
    }

    /// Tests the heartbeat negotiation rules from the STOMP 1.2 specification
    ///
    /// A direction is only active if the sender offers to send and the receiver
    /// wants to receive, and the agreed interval is then the slower of the two,
    /// so that neither side has to beat faster than it promised.
    ///
    /// If this test fails, the client will either beat at the wrong rate or
    /// beat when it should stay silent, both of which lead to the server
    /// dropping the connection.
    #[test]
    fn heartbeat_negotiation() {
        // Either side declining disables the direction
        assert_eq!(negotiate(0, 0), None);
        assert_eq!(negotiate(1000, 0), None);
        assert_eq!(negotiate(0, 1000), None);

        // Otherwise the slower of the two wins, regardless of which side it is
        assert_eq!(negotiate(1000, 5000), Some(Duration::from_millis(5000)));
        assert_eq!(negotiate(5000, 1000), Some(Duration::from_millis(5000)));
        assert_eq!(negotiate(1000, 1000), Some(Duration::from_millis(1000)));
    }

    /// Tests that a heartbeat is encoded as a bare end-of-line
    ///
    /// The specification requires an EOL rather than a frame, so anything else,
    /// a null terminator in particular, would be read by the server as a
    /// malformed frame.
    #[test]
    fn encode_heartbeat() {
        let mut buffer = BytesMut::new();
        ClientCodec::new()
            .encode(Heartbeat, &mut buffer)
            .expect("Encode heartbeat");

        assert_eq!(&buffer[..], b"\n");
    }

    /// Tests that heartbeats from the server are consumed without disturbing
    /// the frames around them
    ///
    /// Heartbeats arrive as newlines in the middle of the byte stream, and the
    /// decoder has to drop them and still find the frame that follows.
    ///
    /// If this test fails, an idle connection to a beating server either grows
    /// a buffer of newlines without bound or fails to parse the next real
    /// frame, killing the connection.
    #[test]
    fn decode_skips_server_heartbeats() {
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(b"\n\n\n");
        buffer.extend_from_slice(MESSAGE_FRAME);

        let msg = ClientCodec::new()
            .decode(&mut buffer)
            .expect("Decode")
            .expect("A message after the heartbeats");

        assert!(matches!(msg.content, FromServer::Message { .. }));
        assert!(buffer.is_empty(), "The whole frame should be consumed");
    }

    /// Tests that heartbeats alone are consumed rather than buffered
    ///
    /// A quiet connection to a beating server sees nothing but newlines, and
    /// they must not be left in the buffer waiting for a frame that may be
    /// hours away.
    #[test]
    fn decode_consumes_lone_heartbeats() {
        for beat in [&b"\n"[..], &b"\r\n"[..], &b"\n\n\n"[..]] {
            let mut buffer = BytesMut::new();
            buffer.extend_from_slice(beat);

            let msg = ClientCodec::new().decode(&mut buffer).expect("Decode");

            assert!(msg.is_none(), "A heartbeat is not a message");
            assert!(buffer.is_empty(), "Heartbeats should not be buffered");
        }
    }
}
