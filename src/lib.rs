//! tokio-stomp - A library for asynchronous streaming of STOMP messages
//!
//! This library provides an async Rust implementation of the STOMP (Simple/Streaming Text Oriented Messaging Protocol),
//! built on the tokio stack. It allows for creating STOMP clients that can connect to message brokers,
//! subscribe to destinations, send messages, and receive messages asynchronously.
//!
//! The primary types exposed by this library are:
//! - `client::Connector` - For establishing connections to STOMP servers
//! - `client::Subscriber` - For creating subscription messages
//! - `Message<T>` - For representing STOMP protocol messages
//! - `ToServer` - Enum of all message types that can be sent to a server
//! - `FromServer` - Enum of all message types that can be received from a server

use custom_debug_derive::Debug as CustomDebug;
use frame::Frame;

pub mod client;
mod frame;

/// Type alias for library results that use anyhow::Error
pub(crate) type Result<T> = std::result::Result<T, anyhow::Error>;

/// A representation of a STOMP frame
///
/// This struct holds the content of a STOMP message (which can be either
/// a message sent to the server or received from the server) along with
/// any extra headers that were present in the frame but not required by
/// the specific message type.
#[derive(Debug)]
pub struct Message<T> {
    /// The message content, which is either a ToServer or FromServer enum
    pub content: T,
    /// Headers present in the frame which were not required by the content type
    /// Stored as raw bytes to avoid unnecessary conversions
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

/// A STOMP message sent from the server
///
/// This enum represents all possible message types that can be received from
/// a STOMP server according to the STOMP 1.2 specification.
///
/// See the [STOMP 1.2 Specification](https://stomp.github.io/stomp-specification-1.2.html)
/// for more detailed information about each message type.
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

/// A STOMP message sent by the client
///
/// This enum represents all possible message types that can be sent to
/// a STOMP server according to the STOMP 1.2 specification.
///
/// See the [STOMP 1.2 Specification](https://stomp.github.io/stomp-specification-1.2.html)
/// for more detailed information about each message type.
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
        /// Message or subscription identifier to acknowledge
        id: String,
        /// Optional transaction identifier
        transaction: Option<String>,
    },

    /// Notify the server that the client did not consume the message
    ///
    /// Used with 'client' or 'client-individual' acknowledgment modes to
    /// indicate that a message could not be processed successfully.
    Nack {
        /// Message or subscription identifier to negative-acknowledge
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

/// Acknowledgment modes for STOMP subscriptions
///
/// Controls how messages should be acknowledged when received through a subscription.
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
    fn to_frame(&self) -> Frame {
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

/// Implement From<ToServer> for Message<ToServer> to allow easy conversion
///
/// This allows ToServer enum variants to be easily converted to a Message
/// with empty extra_headers, which is a common need when sending messages.
impl From<ToServer> for Message<ToServer> {
    /// Convert a ToServer enum into a Message<ToServer>
    ///
    /// This creates a Message with the given content and empty extra_headers.
    fn from(content: ToServer) -> Message<ToServer> {
        Message {
            content,
            extra_headers: vec![],
        }
    }
}
