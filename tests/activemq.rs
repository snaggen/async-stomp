//! Regression tests against a real broker
//!
//! These check what a unit test cannot: that the frames this crate puts on the
//! wire mean to a broker what the spec says they mean. They are ignored by
//! default since they need a running server.
//!
//! The broker needs simple authentication enabled, because one test proves that
//! a passcode holding characters the CONNECT frame must *not* escape still
//! authenticates. Add this to `conf/activemq.xml` just before `</broker>`:
//!
//! ```xml
//! <plugins>
//!     <simpleAuthenticationPlugin>
//!         <users>
//!             <authenticationUser username="admin" password="admin" groups="users,admins"/>
//!             <authenticationUser username="tricky" password="pa:ss\word" groups="users,admins"/>
//!         </users>
//!     </simpleAuthenticationPlugin>
//! </plugins>
//! ```
//!
//! ```text
//! podman run -d --rm --name stomp-amq -p 61613:61613 \
//!     -v "$PWD/activemq.xml:/opt/apache-activemq/conf/activemq.xml:Z" \
//!     docker.io/apache/activemq-classic:latest
//! cargo test --test activemq -- --ignored
//! ```
//!
//! ActiveMQ Classic rather than Artemis, since Artemis expects heart-beating
//! that this branch does not implement yet.

use async_stomp::client::{ClientTransport, Connector, Subscriber, disconnect};
use async_stomp::{AckMode, FromServer, Message, ToServer};
use futures::prelude::*;
use std::time::Duration;
use tokio::time::timeout;

const BROKER: &str = "127.0.0.1:61613";
const LOGIN: &str = "admin";
const PASSCODE: &str = "admin";
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait before concluding that nothing is coming. Only used where
/// the absence of a message is the thing being tested.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------- helpers --

/// A destination nothing else is using, so that a leftover message from an
/// earlier run cannot be mistaken for this one's
fn queue(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("System time after the epoch")
        .as_nanos();
    format!("/queue/async-stomp-{name}-{nanos}")
}

async fn try_connect(login: &str, passcode: &str) -> Result<ClientTransport, anyhow::Error> {
    Connector::builder()
        .server(BROKER)
        .virtualhost("/")
        .login(login.to_string())
        .passcode(passcode.to_string())
        .connect()
        .await
}

async fn connect() -> ClientTransport {
    try_connect(LOGIN, PASSCODE)
        .await
        .expect("Connect to the broker")
}

/// Wait for the next frame, failing rather than hanging if none arrives
async fn next_frame(conn: &mut ClientTransport) -> FromServer {
    timeout(REPLY_TIMEOUT, conn.next())
        .await
        .expect("The broker should answer within the timeout")
        .expect("The stream should not have ended")
        .expect("The frame should parse")
        .content
}

/// Wait a short while for a frame, returning None if none arrives
///
/// For the tests where nothing arriving is the expected outcome.
async fn next_frame_or_silence(conn: &mut ClientTransport) -> Option<FromServer> {
    match timeout(SILENCE_TIMEOUT, conn.next()).await {
        Ok(Some(Ok(msg))) => Some(msg.content),
        Ok(Some(Err(e))) => panic!("The frame should parse: {e}"),
        Ok(None) => None,
        Err(_) => None,
    }
}

/// What a received MESSAGE carries, as far as these tests care
struct Received {
    body: Option<Vec<u8>>,
    /// The value an ACK for this message has to quote
    ack: Option<String>,
    headers: Vec<(String, String)>,
}

/// Take the next frame, which must be a MESSAGE
async fn next_message(conn: &mut ClientTransport) -> Received {
    match next_frame(conn).await {
        FromServer::Message {
            body, ack, headers, ..
        } => Received { body, ack, headers },
        other => panic!("Expected a MESSAGE frame, got {other:?}"),
    }
}

async fn next_body(conn: &mut ClientTransport) -> Option<Vec<u8>> {
    next_message(conn).await.body
}

/// Subscribe and wait for the broker to confirm it
///
/// The receipt matters: without it a message sent immediately afterwards can
/// beat the subscription, and the test then fails for a reason that is not the
/// one being tested.
async fn subscribe(conn: &mut ClientTransport, destination: &str, id: &str, ack: Option<AckMode>) {
    let mut builder = Subscriber::builder().destination(destination).id(id);
    if let Some(mode) = ack {
        builder = builder.ack(mode);
    }
    let msg = with_receipt(builder.subscribe(), "sub");
    conn.send(msg).await.expect("Send the SUBSCRIBE frame");
    expect_receipt(conn, "sub").await;
}

fn publish(
    destination: &str,
    transaction: Option<&str>,
    headers: Option<Vec<(String, String)>>,
    body: &[u8],
) -> Message<ToServer> {
    ToServer::Send {
        destination: destination.to_string(),
        transaction: transaction.map(str::to_string),
        headers,
        body: Some(body.to_vec()),
    }
    .into()
}

fn with_receipt(mut msg: Message<ToServer>, receipt: &str) -> Message<ToServer> {
    msg.extra_headers
        .push((b"receipt".to_vec(), receipt.as_bytes().to_vec()));
    msg
}

async fn expect_receipt(conn: &mut ClientTransport, expected: &str) {
    match next_frame(conn).await {
        FromServer::Receipt { receipt_id } => assert_eq!(receipt_id, expected),
        other => panic!("Expected a RECEIPT for '{expected}', got {other:?}"),
    }
}

fn header<'a>(headers: &'a [(String, String)], key: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Send a message and wait for the broker to confirm it was processed
async fn send_confirmed(conn: &mut ClientTransport, msg: Message<ToServer>, receipt: &str) {
    conn.send(with_receipt(msg, receipt))
        .await
        .expect("Send the frame");
    expect_receipt(conn, receipt).await;
}

// ------------------------------------------------------------ connection --

/// Tests that a connection can be opened and closed the way the spec describes
///
/// The graceful shutdown is DISCONNECT with a receipt, then wait for the
/// RECEIPT before closing the socket, which is what `disconnect` does.
///
/// If this test fails, the handshake or the receipt mechanism is broken and
/// every other test here is untrustworthy.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_connection_opens_and_closes_cleanly() {
    let mut conn = connect().await;
    disconnect(&mut conn, "bye")
        .await
        .expect("Disconnect should be acknowledged");
}

/// Tests that a passcode holding a colon and a backslash authenticates
///
/// CONNECT is one of the two frames the spec exempts from header escaping, so
/// such a passcode has to go out verbatim.
///
/// Note what this test does **not** prove. Measured on ActiveMQ Classic 6.x,
/// the broker unescapes CONNECT headers anyway — itself a deviation, since the
/// spec exempts them — and so accepts both `pa:ss\word` and its escaped form
/// `pa\css\\word`. Against this broker the test therefore passes whether or not
/// the escaping exemption is implemented, and it is the unit tests in
/// `src/frame.rs` that pin the wire format down. Keep it for the brokers that
/// do take CONNECT headers at face value, where it is the difference between
/// connecting and not.
///
/// If this test fails, credentials containing `:` or `\` cannot be used at all,
/// and the failure looks like a wrong password rather than a protocol bug.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_passcode_with_reserved_characters_authenticates() {
    try_connect("tricky", "pa:ss\\word")
        .await
        .expect("A passcode holding ':' and '\\' should authenticate unescaped");
}

/// Tests that the broker's rejection of bad credentials reaches the caller
///
/// The handshake takes anything that is not a CONNECTED frame as a failure, and
/// an authentication failure arrives as an ERROR frame.
///
/// If this test fails, a failed login is reported as a successful connection
/// and the first real frame fails instead, far from the cause.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn wrong_credentials_are_reported() {
    let result = try_connect(LOGIN, "not the passcode").await;
    assert!(result.is_err(), "A bad passcode should not connect");
}

// ------------------------------------------------------------------ send --

/// Tests that a message survives a round trip through the broker
///
/// The baseline: publish to a queue, receive it back on a subscription.
///
/// If this test fails, nothing else in this file means anything.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_simple_message_round_trips() {
    let dest = queue("simple");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-simple", None).await;

    conn.send(publish(&dest, None, None, b"hello"))
        .await
        .expect("Send");

    assert_eq!(next_body(&mut conn).await.as_deref(), Some(&b"hello"[..]));
}

/// Tests that a body containing NULL octets survives a round trip
///
/// The broker reads such a body by its content-length header; without one the
/// body is cut at the first NULL. This is the same property as the unit test
/// covers, but through a real broker rather than this crate's own parser.
///
/// If this test fails, binary payloads are truncated somewhere between here and
/// the consumer.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_body_with_null_octets_survives_the_broker() {
    let dest = queue("binary");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-binary", None).await;

    let payload = b"binary \x00 payload \x00 here".to_vec();
    conn.send(publish(&dest, None, None, &payload))
        .await
        .expect("Send the binary body");

    assert_eq!(
        next_body(&mut conn).await.as_deref(),
        Some(payload.as_slice()),
        "The whole body should arrive, NULL octets included"
    );
}

/// Tests that a body larger than one TCP segment survives a round trip
///
/// A frame this size arrives in many reads, so it exercises the decoder's
/// handling of a partially received frame against a real socket.
///
/// If this test fails, large messages are truncated or the decoder mis-handles
/// a frame split across reads.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_large_body_survives_the_broker() {
    let dest = queue("large");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-large", None).await;

    let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    conn.send(publish(&dest, None, None, &payload))
        .await
        .expect("Send the large body");

    let received = next_body(&mut conn).await.expect("A body");
    assert_eq!(received.len(), payload.len(), "Length should match");
    assert_eq!(received, payload, "Contents should match");
}

/// Tests that a body of non-ASCII UTF-8 survives a round trip
///
/// STOMP is UTF-8 throughout, and a multi-byte sequence must not be split or
/// re-encoded on the way.
///
/// If this test fails, any message not in plain ASCII is corrupted.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_utf8_body_survives_the_broker() {
    let dest = queue("utf8");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-utf8", None).await;

    let payload = "räksmörgås — 日本語 — 🦀".as_bytes().to_vec();
    conn.send(publish(&dest, None, None, &payload))
        .await
        .expect("Send the UTF-8 body");

    assert_eq!(next_body(&mut conn).await.as_deref(), Some(&payload[..]));
}

/// Tests that user headers reach the consumer
///
/// The spec says a MESSAGE carries all user-defined headers the SEND had, which
/// is what makes header-based filtering and metadata work at all.
///
/// If this test fails, anything a producer attaches to a message is lost.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn user_headers_are_forwarded_to_the_consumer() {
    let dest = queue("headers");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-headers", None).await;

    conn.send(publish(
        &dest,
        None,
        Some(vec![
            ("x-one".to_string(), "first".to_string()),
            ("x-two".to_string(), "second".to_string()),
        ]),
        b"with headers",
    ))
    .await
    .expect("Send with headers");

    let headers = next_message(&mut conn).await.headers;
    assert_eq!(header(&headers, "x-one"), Some("first"), "{headers:?}");
    assert_eq!(header(&headers, "x-two"), Some("second"), "{headers:?}");
}

/// Tests that a header value needing escapes survives a round trip
///
/// SEND is not one of the frames exempt from escaping, so a colon and a
/// backslash in a user header have to be escaped on the way out and come back
/// unescaped. This is the counterpart to the CONNECT test, which covers the
/// exemption.
///
/// If this test fails, escaping is applied to the wrong set of frames and user
/// headers are mangled in one direction or the other.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_header_value_needing_escapes_survives_the_broker() {
    let dest = queue("escape");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-escape", None).await;

    let tricky = "a:b\\c";
    conn.send(publish(
        &dest,
        None,
        Some(vec![("x-tricky".to_string(), tricky.to_string())]),
        b"escaped header",
    ))
    .await
    .expect("Send with a tricky header");

    let headers = next_message(&mut conn).await.headers;
    assert_eq!(
        header(&headers, "x-tricky"),
        Some(tricky),
        "The header value should come back as it was sent. Headers: {headers:?}"
    );
}

/// Tests that a MESSAGE names the destination and the subscription it belongs to
///
/// Both are required headers, and the subscription id is how a client with more
/// than one subscription knows which one a message came from.
///
/// If this test fails, a multi-subscription consumer cannot route its messages.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_message_names_its_destination_and_subscription() {
    let dest = queue("routing");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-routing", None).await;

    conn.send(publish(&dest, None, None, b"routed"))
        .await
        .expect("Send");

    let headers = next_message(&mut conn).await.headers;
    assert_eq!(header(&headers, "destination"), Some(dest.as_str()));
    assert_eq!(header(&headers, "subscription"), Some("sub-routing"));
}

/// Tests that a SEND can ask for a receipt
///
/// A receipt is how a producer learns that the broker has taken responsibility
/// for a message, rather than only that it was written to a socket.
///
/// If this test fails, there is no way to publish reliably.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_send_can_ask_for_a_receipt() {
    let dest = queue("receipt");
    let mut conn = connect().await;
    send_confirmed(
        &mut conn,
        publish(&dest, None, None, b"confirmed"),
        "send-1",
    )
    .await;
}

// ---------------------------------------------------------- transactions --

/// Tests that a message sent inside a committed transaction is delivered
///
/// Half of the transaction check: binding a message to a transaction must not
/// lose it.
///
/// If this test fails, transactional sends are dropped rather than held.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_committed_transaction_delivers_its_messages() {
    let dest = queue("commit");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-commit", None).await;

    conn.send(
        ToServer::Begin {
            transaction: "tx-commit".into(),
        }
        .into(),
    )
    .await
    .expect("Begin");
    conn.send(publish(&dest, Some("tx-commit"), None, b"committed"))
        .await
        .expect("Send inside the transaction");
    conn.send(
        ToServer::Commit {
            transaction: "tx-commit".into(),
        }
        .into(),
    )
    .await
    .expect("Commit");

    assert_eq!(
        next_body(&mut conn).await.as_deref(),
        Some(&b"committed"[..])
    );
}

/// Tests that a message sent inside an aborted transaction is discarded
///
/// The end-to-end check that the transaction header reaches the broker under
/// the name the spec gives it. A sentinel sent outside the transaction follows
/// it, so the assertion tells "the aborted message was discarded" apart from
/// "nothing has arrived yet".
///
/// If this test fails, SEND is not binding messages to the transaction at all
/// and an ABORT no longer takes them back — the message is published the moment
/// it is sent.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn an_aborted_transaction_discards_its_messages() {
    let dest = queue("abort");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-abort", None).await;

    conn.send(
        ToServer::Begin {
            transaction: "tx-abort".into(),
        }
        .into(),
    )
    .await
    .expect("Begin");
    conn.send(publish(
        &dest,
        Some("tx-abort"),
        None,
        b"inside the transaction",
    ))
    .await
    .expect("Send inside the transaction");
    conn.send(
        ToServer::Abort {
            transaction: "tx-abort".into(),
        }
        .into(),
    )
    .await
    .expect("Abort");

    // Sent outside the transaction, so it arrives whatever happens. Had the
    // aborted message been published anyway, it would be queued ahead of this.
    conn.send(publish(&dest, None, None, b"sentinel"))
        .await
        .expect("Send the sentinel");

    assert_eq!(
        next_body(&mut conn).await.as_deref(),
        Some(&b"sentinel"[..]),
        "The aborted message should never have been delivered"
    );
}

/// Tests that a transaction holds its messages back until the commit
///
/// Nothing may be delivered while the transaction is open, and then everything
/// at once, in order. Checking a single message cannot tell a working
/// transaction from one whose messages were published immediately and happened
/// to arrive in the right order.
///
/// If this test fails, transactions are not atomic and a consumer can observe
/// half of one.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_transaction_holds_its_messages_until_the_commit() {
    let dest = queue("atomic");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-atomic", None).await;

    conn.send(
        ToServer::Begin {
            transaction: "tx-atomic".into(),
        }
        .into(),
    )
    .await
    .expect("Begin");
    for body in [b"one", b"two"] {
        conn.send(publish(&dest, Some("tx-atomic"), None, body))
            .await
            .expect("Send inside the transaction");
    }

    assert!(
        next_frame_or_silence(&mut conn).await.is_none(),
        "Nothing should be delivered before the commit"
    );

    conn.send(
        ToServer::Commit {
            transaction: "tx-atomic".into(),
        }
        .into(),
    )
    .await
    .expect("Commit");

    assert_eq!(next_body(&mut conn).await.as_deref(), Some(&b"one"[..]));
    assert_eq!(next_body(&mut conn).await.as_deref(), Some(&b"two"[..]));
}

// --------------------------------------------------------- subscriptions --

/// Tests that two subscriptions on one connection are told apart
///
/// The subscription id on a MESSAGE is what makes more than one subscription
/// per connection usable.
///
/// If this test fails, a consumer cannot tell which of its subscriptions a
/// message belongs to.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn two_subscriptions_are_told_apart_by_id() {
    let first = queue("multi-a");
    let second = queue("multi-b");
    let mut conn = connect().await;
    subscribe(&mut conn, &first, "sub-a", None).await;
    subscribe(&mut conn, &second, "sub-b", None).await;

    conn.send(publish(&second, None, None, b"to b"))
        .await
        .expect("Send to the second queue");

    let received = next_message(&mut conn).await;
    let (body, headers) = (received.body, received.headers);
    assert_eq!(body.as_deref(), Some(&b"to b"[..]));
    assert_eq!(header(&headers, "subscription"), Some("sub-b"));
    assert_eq!(header(&headers, "destination"), Some(second.as_str()));
}

/// Tests that UNSUBSCRIBE stops delivery
///
/// Proving an absence needs a wait, so this test is deliberately slow. The
/// message is confirmed with a receipt first, so the silence that follows means
/// the subscription is gone rather than that the broker has not caught up.
///
/// If this test fails, a consumer that unsubscribes keeps receiving messages it
/// is no longer prepared to handle.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn unsubscribe_stops_delivery() {
    let dest = queue("unsub");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-unsub", None).await;

    conn.send(with_receipt(
        ToServer::Unsubscribe {
            id: "sub-unsub".into(),
        }
        .into(),
        "unsub",
    ))
    .await
    .expect("Unsubscribe");
    expect_receipt(&mut conn, "unsub").await;

    send_confirmed(
        &mut conn,
        publish(&dest, None, None, b"after unsubscribe"),
        "after",
    )
    .await;

    assert!(
        next_frame_or_silence(&mut conn).await.is_none(),
        "No message should arrive after unsubscribing"
    );
}

// ------------------------------------------------------------ ack / nack --

/// Tests that a subscription needing acknowledgment gets an ack header, and
/// that acknowledging with its value is accepted
///
/// STOMP 1.2 acknowledges by the MESSAGE's `ack` header, not its `message-id`.
/// The receipt is the only way to hear that the broker accepted the ACK at all.
///
/// If this test fails, explicit acknowledgment does not work and every message
/// in client-ack mode is eventually redelivered.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn acknowledging_with_the_ack_header_is_accepted() {
    let dest = queue("ack-ok");
    let mut conn = connect().await;
    subscribe(
        &mut conn,
        &dest,
        "sub-ack-ok",
        Some(AckMode::ClientIndividual),
    )
    .await;

    conn.send(publish(&dest, None, None, b"needs an ack"))
        .await
        .expect("Send");

    let ack = next_message(&mut conn)
        .await
        .ack
        .expect("A client-individual subscription must get an ack value");

    send_confirmed(
        &mut conn,
        ToServer::Ack {
            id: ack,
            transaction: None,
        }
        .into(),
        "ack-1",
    )
    .await;
}

/// Records that this broker rejects an ACK carrying the message-id
///
/// `message-id` is the STOMP 1.0/1.1 way, and the crate's README has long shown
/// it. ActiveMQ answers an ERROR frame and, as the spec requires after an
/// ERROR, closes the connection — so the mistake costs the session, not just
/// the acknowledgment. This test pins that down so the README is not quietly
/// restored to the old pattern.
///
/// If this test fails, the broker changed its behaviour and the advice in
/// STOMP-COMPLIANCE.md about the ACK id should be re-checked against it.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn acknowledging_with_the_message_id_is_rejected() {
    let dest = queue("ack-bad");
    let mut conn = connect().await;
    subscribe(
        &mut conn,
        &dest,
        "sub-ack-bad",
        Some(AckMode::ClientIndividual),
    )
    .await;

    conn.send(publish(&dest, None, None, b"needs an ack"))
        .await
        .expect("Send");

    let received = next_message(&mut conn).await;
    let message_id = header(&received.headers, "message-id")
        .expect("A MESSAGE carries a message-id")
        .to_string();
    let ack = received
        .ack
        .as_deref()
        .expect("A client-individual subscription gets an ack value");
    assert_ne!(
        ack, message_id,
        "This test only means something while the two differ"
    );

    conn.send(
        ToServer::Ack {
            id: message_id,
            transaction: None,
        }
        .into(),
    )
    .await
    .expect("Send the wrong ACK");

    match next_frame(&mut conn).await {
        FromServer::Error { message, .. } => {
            let message = message.unwrap_or_default();
            assert!(
                message.contains("Unexpected ACK"),
                "Expected a complaint about the ACK, got {message:?}"
            );
        }
        other => panic!("Expected an ERROR frame, got {other:?}"),
    }
}

/// Tests that a message left unacknowledged is redelivered
///
/// The point of client acknowledgment: a consumer that dies mid-processing must
/// not take the message with it. The first connection is dropped without
/// acknowledging, and a second one picks the message up again.
///
/// If this test fails, messages are lost when a consumer fails, which is the
/// one thing client-ack mode exists to prevent.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn an_unacknowledged_message_is_redelivered() {
    let dest = queue("redeliver");
    let mut producer = connect().await;
    send_confirmed(&mut producer, publish(&dest, None, None, b"unacked"), "p").await;

    {
        let mut consumer = connect().await;
        subscribe(
            &mut consumer,
            &dest,
            "sub-first",
            Some(AckMode::ClientIndividual),
        )
        .await;
        assert_eq!(
            next_body(&mut consumer).await.as_deref(),
            Some(&b"unacked"[..])
        );
        // Dropped without an ACK, as a consumer that crashed would be
    }

    let mut second = connect().await;
    subscribe(
        &mut second,
        &dest,
        "sub-second",
        Some(AckMode::ClientIndividual),
    )
    .await;
    assert_eq!(
        next_body(&mut second).await.as_deref(),
        Some(&b"unacked"[..]),
        "The message should be redelivered to the next consumer"
    );
}

/// Tests that an acknowledged message is not redelivered
///
/// The counterpart to the redelivery test: once acknowledged, a message is
/// gone. Without this, a test suite cannot tell a working ACK from one the
/// broker ignored.
///
/// If this test fails, messages are delivered more than once even after being
/// acknowledged.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn an_acknowledged_message_is_not_redelivered() {
    let dest = queue("acked");
    let mut producer = connect().await;
    send_confirmed(&mut producer, publish(&dest, None, None, b"acked"), "p").await;

    {
        let mut consumer = connect().await;
        subscribe(
            &mut consumer,
            &dest,
            "sub-first",
            Some(AckMode::ClientIndividual),
        )
        .await;
        let ack = next_message(&mut consumer).await.ack.expect("An ack value");
        send_confirmed(
            &mut consumer,
            ToServer::Ack {
                id: ack,
                transaction: None,
            }
            .into(),
            "ack-1",
        )
        .await;
    }

    let mut second = connect().await;
    subscribe(
        &mut second,
        &dest,
        "sub-second",
        Some(AckMode::ClientIndividual),
    )
    .await;
    assert!(
        next_frame_or_silence(&mut second).await.is_none(),
        "An acknowledged message should not come back"
    );
}

/// Tests that the broker accepts a NACK frame
///
/// NACK is how a consumer says it could not process a message. What the broker
/// then does with that message is its own policy, not something the spec fixes
/// or this crate controls, so the receipt is the assertion: it says the broker
/// read and processed the frame rather than answering an ERROR.
///
/// Measured on ActiveMQ Classic 6.x, a NACK outside a transaction neither
/// redelivers the message to another consumer nor routes it to `ActiveMQ.DLQ` —
/// it is simply dropped. Asserting redelivery here would be testing that policy
/// rather than this crate, and would fail against this broker.
///
/// If this test fails, the NACK frame this crate builds is malformed and the
/// broker rejects it.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_nack_is_accepted_by_the_broker() {
    let dest = queue("nack");
    let mut conn = connect().await;
    subscribe(
        &mut conn,
        &dest,
        "sub-nack",
        Some(AckMode::ClientIndividual),
    )
    .await;

    conn.send(publish(&dest, None, None, b"nacked"))
        .await
        .expect("Send");

    let ack = next_message(&mut conn).await.ack.expect("An ack value");

    send_confirmed(
        &mut conn,
        ToServer::Nack {
            id: ack,
            transaction: None,
        }
        .into(),
        "nack-1",
    )
    .await;
}

/// Tests that an acknowledgment can be made part of a transaction
///
/// ACK takes an optional transaction header, which lets a consumer tie "I have
/// processed this" to whatever else the transaction covers.
///
/// If this test fails, acknowledgment cannot be made atomic with the work it
/// reports, and a crash between the two leaves the two out of step.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn an_acknowledgment_can_be_part_of_a_transaction() {
    let dest = queue("tx-ack");
    let mut producer = connect().await;
    send_confirmed(&mut producer, publish(&dest, None, None, b"tx acked"), "p").await;

    {
        let mut consumer = connect().await;
        subscribe(
            &mut consumer,
            &dest,
            "sub-first",
            Some(AckMode::ClientIndividual),
        )
        .await;
        let ack = next_message(&mut consumer).await.ack.expect("An ack value");

        consumer
            .send(
                ToServer::Begin {
                    transaction: "tx-ack".into(),
                }
                .into(),
            )
            .await
            .expect("Begin");
        consumer
            .send(
                ToServer::Ack {
                    id: ack,
                    transaction: Some("tx-ack".into()),
                }
                .into(),
            )
            .await
            .expect("Ack inside the transaction");
        send_confirmed(
            &mut consumer,
            ToServer::Commit {
                transaction: "tx-ack".into(),
            }
            .into(),
            "commit",
        )
        .await;
    }

    let mut second = connect().await;
    subscribe(
        &mut second,
        &dest,
        "sub-second",
        Some(AckMode::ClientIndividual),
    )
    .await;
    assert!(
        next_frame_or_silence(&mut second).await.is_none(),
        "The committed acknowledgment should have removed the message"
    );
}

/// Tests that the ack mode chosen on the builder reaches the broker
///
/// `Subscriber::ack` is the supported way to ask for explicit acknowledgment.
/// The broker only sends an `ack` value on a subscription that asked for one,
/// so its presence is the proof that the mode travelled.
///
/// If this test fails, a subscription that asked for client acknowledgment is
/// running in auto mode, and messages are lost when a consumer fails.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn the_builders_ack_mode_reaches_the_broker() {
    let dest = queue("ackmode");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-ackmode", Some(AckMode::Client)).await;

    conn.send(publish(&dest, None, None, b"client ack"))
        .await
        .expect("Send");

    assert!(
        next_message(&mut conn).await.ack.is_some(),
        "A subscription in client mode should be given something to acknowledge with"
    );
}

/// Tests that a subscription in auto mode is given nothing to acknowledge
///
/// The contrast to the test above: without it, an `ack` value that is always
/// present would pass both.
///
/// If this test fails, callers are handed an acknowledgment value the broker
/// never issued and will not accept.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_subscription_in_auto_mode_gets_no_ack_value() {
    let dest = queue("auto-ack");
    let mut conn = connect().await;
    subscribe(&mut conn, &dest, "sub-auto", None).await;

    conn.send(publish(&dest, None, None, b"no ack needed"))
        .await
        .expect("Send");

    assert_eq!(
        next_message(&mut conn).await.ack,
        None,
        "Auto mode needs no acknowledgment, so there is nothing to quote"
    );
}

/// Tests that a message past the configured bound is refused rather than buffered
///
/// The bound is what stops a broken or hostile `content-length` from growing
/// the read buffer until the process runs out of memory. A real oversized
/// message is the closest thing to that a cooperating broker can produce.
///
/// If this test fails, the bound is not enforced on the receive path and the
/// client will buffer whatever it is sent.
#[tokio::test]
#[ignore = "needs a broker, see the module docs"]
async fn a_message_over_the_frame_bound_is_refused() {
    let dest = queue("bound");

    let mut producer = connect().await;
    send_confirmed(
        &mut producer,
        publish(&dest, None, None, &vec![b'x'; 64 * 1024]),
        "p",
    )
    .await;

    let mut consumer = Connector::builder()
        .server(BROKER)
        .virtualhost("/")
        .login(LOGIN.to_string())
        .passcode(PASSCODE.to_string())
        .max_frame_size(4096)
        .connect()
        .await
        .expect("Connect with a small bound");
    subscribe(&mut consumer, &dest, "sub-bound", None).await;

    match timeout(REPLY_TIMEOUT, consumer.next())
        .await
        .expect("The broker should answer within the timeout")
    {
        Some(Err(e)) => assert!(
            e.to_string().contains("maximum"),
            "Expected the bound to be named in the error, got {e}"
        ),
        other => panic!("Expected the oversized frame to be refused, got {other:?}"),
    }
}
