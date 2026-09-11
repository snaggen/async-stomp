//! An async [STOMP 1.2](https://stomp.github.io/stomp-specification-1.2.html)
//! client, built on the tokio stack.
//!
//! [`client::Connector`] establishes a connection and hands back a
//! [`client::ClientTransport`], which is a `Sink` of [`ToServer`] messages and
//! a `Stream` of [`FromServer`] ones. [`client::Subscriber`] builds the
//! SUBSCRIBE message.
//!
//! ```rust,no_run
//! use async_stomp::client::{Connector, Subscriber};
//! use async_stomp::FromServer;
//! use futures::prelude::*;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), anyhow::Error> {
//! let mut conn = Connector::builder()
//!     .server("127.0.0.1:61613")
//!     .virtualhost("/")
//!     .connect()
//!     .await?;
//!
//! let subscribe = Subscriber::builder()
//!     .destination("queue.test")
//!     .id("sub-1")
//!     .subscribe();
//! conn.send(subscribe).await?;
//!
//! while let Some(msg) = conn.next().await {
//!     if let FromServer::Message { body, .. } = msg?.content {
//!         println!("{}", String::from_utf8_lossy(&body.unwrap_or_default()));
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Heart-beating is off by default. Turning it on keeps an idle connection
//! alive against brokers that close quiet sessions, see [`client::Connector`].

#![warn(missing_docs)]

use custom_debug_derive::Debug as CustomDebug;
use frame::Frame;

pub mod client;
mod frame;

/// Type alias for library results that use anyhow::Error
pub(crate) type Result<T> = std::result::Result<T, anyhow::Error>;

/// A STOMP frame: its content, plus any headers the content type has no field
/// for
///
/// `T` is [`ToServer`] or [`FromServer`]. Since a bare [`ToServer`] converts
/// into a `Message` with no extra headers, `.into()` is usually all you need.
///
/// ```rust
/// use async_stomp::{Message, ToServer};
///
/// let mut msg: Message<ToServer> = ToServer::Send {
///     destination: "queue.test".into(),
///     transaction: None,
///     headers: None,
///     body: Some(b"hello".to_vec()),
/// }
/// .into();
///
/// // Headers the variant does not cover go here, as raw bytes
/// msg.extra_headers.push((b"priority".to_vec(), b"9".to_vec()));
/// ```
#[derive(Debug)]
pub struct Message<T> {
    /// The message itself, a [`ToServer`] or [`FromServer`] variant
    pub content: T,
    /// Headers the content type has no field for, as raw bytes
    pub extra_headers: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Helper function for pretty-printing binary data in debug output
///
/// This function converts binary data (Option<Vec<u8>>) to a UTF-8 string
/// for better readability in debug output.
fn pretty_bytes(b: &Option<Vec<u8>>, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    if let Some(v) = b {
        write!(f, "{}", String::from_utf8_lossy(v))
    } else {
        write!(f, "None")
    }
}

/// A message received from the server
///
/// Reached through the `content` field of a [`Message`] yielded by the
/// transport's `Stream`.
///
/// ```rust
/// use async_stomp::FromServer;
///
/// fn handle(content: FromServer) {
///     match content {
///         FromServer::Message { destination, body, .. } => {
///             println!("{destination}: {:?}", body.as_deref().map(String::from_utf8_lossy));
///         }
///         FromServer::Receipt { receipt_id } => println!("receipt {receipt_id}"),
///         FromServer::Error { message, .. } => eprintln!("error {message:?}"),
///         _ => {}
///     }
/// }
/// ```
#[derive(CustomDebug, Clone)]
pub enum FromServer {
    /// Connection established acknowledgment
    ///
    /// Sent by the server in response to a successful CONNECT/STOMP frame.
    #[doc(hidden)] // The user shouldn't need to know about this one
    Connected {
        /// Protocol version
        version: String,
        /// Optional session identifier
        session: Option<String>,
        /// Optional server identifier
        server: Option<String>,
        /// Optional heartbeat settings
        heartbeat: Option<String>,
    },

    /// Message received from a subscription
    ///
    /// Conveys messages from subscriptions to the client. Contains the
    /// message content and associated metadata.
    Message {
        /// Destination the message was sent to
        destination: String,
        /// Unique message identifier
        message_id: String,
        /// Subscription identifier this message relates to
        subscription: String,
        /// Value to acknowledge this message with
        ///
        /// Present when the subscription asked for explicit acknowledgment.
        /// Pass it as the `id` of [`ToServer::Ack`] or [`ToServer::Nack`] —
        /// acknowledging by `message_id` is the STOMP 1.0 and 1.1 rule, and a
        /// 1.2 broker answers an ERROR and closes the connection.
        ack: Option<String>,
        /// All headers included in the message
        headers: Vec<(String, String)>,
        /// Optional message body
        #[debug(with = "pretty_bytes")]
        body: Option<Vec<u8>>,
    },

    /// Receipt confirmation
    ///
    /// Sent from the server to the client once a server has successfully
    /// processed a client frame that requested a receipt.
    Receipt {
        /// Receipt identifier matching the client's receipt request
        receipt_id: String,
    },

    /// Error notification
    ///
    /// Sent when something goes wrong. After sending an Error,
    /// the server will close the connection.
    Error {
        /// Optional error message
        message: Option<String>,
        /// Optional error body with additional details
        #[debug(with = "pretty_bytes")]
        body: Option<Vec<u8>>,
    },
}

// TODO tidy this lot up with traits?
impl Message<FromServer> {
    // fn to_frame<'a>(&'a self) -> Frame<'a> {
    //     unimplemented!()
    // }

    /// Convert a Frame into a Message<FromServer>
    ///
    /// This internal method handles conversion from the low-level Frame
    /// representation to the high-level Message representation.
    fn from_frame(frame: Frame) -> Result<Message<FromServer>> {
        frame.to_server_msg()
    }
}

/// A message to send to the server
///
/// Convert one into a [`Message`] with `.into()` and hand it to the transport's
/// `Sink`. CONNECT is not among the variants you need: [`client::Connector`]
/// sends it during the handshake.
///
/// ```rust
/// use async_stomp::{Message, ToServer};
///
/// // Publish to a destination
/// let publish: Message<ToServer> = ToServer::Send {
///     destination: "queue.test".into(),
///     transaction: None,
///     headers: None,
///     body: Some(b"hello".to_vec()),
/// }
/// .into();
///
/// // Acknowledge a message you have finished processing
/// let ack: Message<ToServer> = ToServer::Ack {
///     id: "message-123".into(),
///     transaction: None,
/// }
/// .into();
/// ```
#[derive(Debug, Clone)]
pub enum ToServer {
    /// Connection request message
    ///
    /// First frame sent to the server to establish a STOMP session.
    #[doc(hidden)] // The user shouldn't need to know about this one
    Connect {
        /// Protocol versions the client supports
        accept_version: String,
        /// Virtual host the client wants to connect to
        host: String,
        /// Optional authentication username
        login: Option<String>,
        /// Optional authentication password
        passcode: Option<String>,
        /// Optional heartbeat configuration (cx, cy)
        heartbeat: Option<(u32, u32)>,
    },

    /// Send a message to a destination in the messaging system
    ///
    /// Used to send a message to a specific destination like a queue or topic.
    Send {
        /// Destination to send the message to
        destination: String,
        /// Optional transaction identifier
        transaction: Option<String>,
        /// Optional additional headers to include
        headers: Option<Vec<(String, String)>>,
        /// Optional message body
        body: Option<Vec<u8>>,
    },

    /// Register to listen to a given destination
    ///
    /// Creates a subscription to receive messages from a specific destination.
    Subscribe {
        /// Destination to subscribe to
        destination: String,
        /// Client-generated subscription identifier
        id: String,
        /// Optional acknowledgment mode
        ack: Option<AckMode>,
    },

    /// Remove an existing subscription
    ///
    /// Cancels a subscription so the client stops receiving messages from it.
    Unsubscribe {
        /// Subscription identifier to unsubscribe from
        id: String,
    },

    /// Acknowledge consumption of a message from a subscription
    ///
    /// Used with 'client' or 'client-individual' acknowledgment modes to
    /// confirm successful processing of a message.
    Ack {
        /// The `ack` field of the [`FromServer::Message`] being acknowledged
        ///
        /// Not its `message_id`: that is the STOMP 1.0 and 1.1 rule, and a 1.2
        /// broker answers an ERROR and closes the connection.
        id: String,
        /// Optional transaction identifier
        transaction: Option<String>,
    },

    /// Notify the server that the client did not consume the message
    ///
    /// Used with 'client' or 'client-individual' acknowledgment modes to
    /// indicate that a message could not be processed successfully.
    Nack {
        /// The `ack` field of the [`FromServer::Message`] being rejected
        ///
        /// Not its `message_id`, for the same reason as [`ToServer::Ack`].
        id: String,
        /// Optional transaction identifier
        transaction: Option<String>,
    },

    /// Start a transaction
    ///
    /// Begins a new transaction that can group multiple STOMP operations.
    Begin {
        /// Client-generated transaction identifier
        transaction: String,
    },

    /// Commit an in-progress transaction
    ///
    /// Completes a transaction and applies all its operations.
    Commit {
        /// Transaction identifier to commit
        transaction: String,
    },

    /// Roll back an in-progress transaction
    ///
    /// Cancels a transaction and rolls back all its operations.
    Abort {
        /// Transaction identifier to abort
        transaction: String,
    },

    /// Gracefully disconnect from the server
    ///
    /// Cleanly ends the STOMP session. Clients MUST NOT send any more
    /// frames after the DISCONNECT frame is sent.
    Disconnect {
        /// Optional receipt request
        receipt: Option<String>,
    },
}

/// How messages from a subscription are acknowledged
///
/// Set on [`ToServer::Subscribe`]; leaving it at `None` means [`AckMode::Auto`].
///
/// ```rust
/// use async_stomp::{AckMode, Message, ToServer};
///
/// // Each message has to be acknowledged individually before it counts as done
/// let subscribe: Message<ToServer> = ToServer::Subscribe {
///     destination: "queue.test".into(),
///     id: "sub-1".into(),
///     ack: Some(AckMode::ClientIndividual),
/// }
/// .into();
/// ```
#[derive(Debug, Clone, Copy)]
pub enum AckMode {
    /// Auto acknowledgment (the default if not specified)
    ///
    /// The client does not need to send ACK frames; the server will
    /// assume the client received the message as soon as it is sent.
    Auto,

    /// Client acknowledgment
    ///
    /// The client must send an ACK frame for each message received.
    /// An ACK acknowledges all messages received so far on the connection.
    Client,

    /// Client individual acknowledgment
    ///
    /// The client must send an ACK frame for each individual message.
    /// Only the individual message referenced in the ACK is acknowledged.
    ClientIndividual,
}

impl Message<ToServer> {
    /// Convert this message to a low-level Frame
    ///
    /// This method converts the high-level Message to the low-level Frame
    /// representation needed for serialization.
    fn to_frame<'a>(&'a self) -> Frame<'a> {
        // Create a frame from the message content
        let mut frame = self.content.to_frame();
        // Add any extra headers to the frame
        frame.add_extra_headers(&self.extra_headers);
        frame
    }

    /// Convert a Frame into a Message<ToServer>
    ///
    /// This internal method handles conversion from the low-level Frame
    /// representation to the high-level Message representation.
    #[allow(dead_code)]
    fn from_frame(frame: Frame) -> Result<Message<ToServer>> {
        frame.to_client_msg()
    }
}

/// Wraps a [`ToServer`] in a [`Message`] with no extra headers
impl From<ToServer> for Message<ToServer> {
    fn from(content: ToServer) -> Message<ToServer> {
        Message {
            content,
            extra_headers: vec![],
        }
    }
}
