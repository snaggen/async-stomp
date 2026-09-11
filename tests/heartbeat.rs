//! Heart-beat tests against a minimal in-process STOMP server
//!
//! These run without a broker, so unlike the tests in `client.rs` they are not
//! ignored by default. The two tests at the end are the exception: they need a
//! real broker with an idle timeout and are marked `#[ignore]`.

use async_stomp::client::Connector;
use async_stomp::{FromServer, ToServer};
use futures::prelude::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Reads bytes up to and including the null terminator that ends a STOMP frame
///
/// Reads one byte at a time so that nothing following the frame, a heart-beat
/// in particular, is swallowed along with it.
async fn read_frame(sock: &mut TcpStream) -> String {
    let mut frame = Vec::new();
    let mut byte = [0u8; 1];
    while sock.read_exact(&mut byte).await.is_ok() && byte[0] != b'\0' {
        frame.push(byte[0]);
    }
    String::from_utf8(frame).expect("STOMP frames are text in these tests")
}

/// Accepts one connection and completes the handshake, answering with the given
/// `heart-beat` header value
///
/// Returns the socket and the CONNECT frame the client sent.
async fn accept_handshake(listener: &TcpListener, heartbeat: &str) -> (TcpStream, String) {
    let (mut sock, _) = listener.accept().await.expect("Accept a connection");
    let connect = read_frame(&mut sock).await;
    sock.write_all(format!("CONNECTED\nversion:1.2\nheart-beat:{heartbeat}\n\n\0").as_bytes())
        .await
        .expect("Send CONNECTED");
    (sock, connect)
}

/// Collects everything the client sends within the given window
async fn collect_for(sock: &mut TcpStream, window: Duration) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + window;
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    while let Ok(Ok(read)) = tokio::time::timeout_at(deadline, sock.read(&mut buf)).await {
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buf[..read]);
    }
    data
}

/// A SEND message, for tests that need some real traffic on the connection
fn send_msg() -> async_stomp::Message<ToServer> {
    ToServer::Send {
        destination: "/test".into(),
        transaction: None,
        headers: None,
        body: Some(b"payload".to_vec()),
    }
    .into()
}

/// Tests that an idle connection is kept alive with heartbeats
///
/// This is the whole point of heart-beating: a client that has nothing to say
/// still has to prove it is alive, or brokers with an idle timeout will drop
/// the session.
///
/// If this test fails, long-lived subscriptions that see little traffic will be
/// disconnected by the broker.
#[tokio::test]
async fn sends_heartbeats_when_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, connect) = accept_handshake(&listener, "250,250").await;
        assert!(
            connect.contains("heart-beat:250,250"),
            "CONNECT should carry the requested interval, got: {connect}"
        );
        collect_for(&mut sock, Duration::from_millis(1200)).await
    });

    let conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(250, 250)
        .connect()
        .await
        .expect("Connect");

    assert_eq!(
        conn.heartbeat(),
        (
            Some(Duration::from_millis(250)),
            Some(Duration::from_millis(250))
        )
    );

    // Stay quiet and let the background pump do the talking
    let data = server.await.expect("Server task");

    assert!(
        data.iter().all(|&b| b == b'\n'),
        "An idle client should send nothing but heartbeats, got: {:?}",
        String::from_utf8_lossy(&data)
    );
    assert!(
        data.len() >= 2,
        "Expected several heartbeats in 1.2s at a 250ms interval, got {}",
        data.len()
    );
}

/// Tests that no heartbeat is sent while there is real traffic
///
/// A heartbeat is only needed when nothing else has been sent, and sending them
/// alongside ordinary frames is pure waste.
///
/// If this test fails, every busy connection carries needless traffic.
#[tokio::test]
async fn no_heartbeats_while_sending() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = accept_handshake(&listener, "250,250").await;
        collect_for(&mut sock, Duration::from_millis(700)).await
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(250, 250)
        .connect()
        .await
        .expect("Connect");

    // Well inside the 250ms interval, so a heartbeat is never due
    for _ in 0..7 {
        conn.send(send_msg()).await.expect("Send");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let data = server.await.expect("Server task");

    // A heartbeat between two frames shows up as a chunk starting with a newline
    for chunk in data.split(|&b| b == b'\0') {
        assert!(
            chunk.is_empty() || chunk.starts_with(b"SEND"),
            "Unexpected bytes between frames: {:?}",
            String::from_utf8_lossy(chunk)
        );
    }
}

/// Tests that the server can decline heartbeats
///
/// Both sides have to agree before a direction is active, so a server that
/// answers with zeroes must not be beaten at.
///
/// If this test fails, the client sends heartbeats the server never asked for,
/// which a strict server may treat as a protocol error.
#[tokio::test]
async fn no_heartbeats_when_server_declines() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = accept_handshake(&listener, "0,0").await;
        collect_for(&mut sock, Duration::from_millis(800)).await
    });

    let conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(250, 250)
        .connect()
        .await
        .expect("Connect");

    assert_eq!(conn.heartbeat(), (None, None));

    let data = server.await.expect("Server task");
    assert!(data.is_empty(), "Nothing should have been sent");
}

/// Tests the negotiation against an Artemis-shaped reply
///
/// Artemis derives its connection TTL from the interval the client offers and
/// answers with roughly that interval halved, so the client has to settle on
/// the slower of the two values rather than on the server's.
///
/// If this test fails, the client beats at the wrong rate against a real
/// broker.
#[tokio::test]
async fn negotiates_slower_of_the_two() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (sock, _) = accept_handshake(&listener, "10000,10000").await;
        // Hold the connection open for the duration of the test
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(sock);
    });

    let conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(20000, 20000)
        .connect()
        .await
        .expect("Connect");

    assert_eq!(
        conn.heartbeat(),
        (
            Some(Duration::from_millis(20000)),
            Some(Duration::from_millis(20000))
        )
    );

    server.await.expect("Server task");
}

/// Tests that a frame following heartbeats is still decoded
///
/// Heartbeats arrive as newlines in the middle of the byte stream, and used to
/// be left in the read buffer where they broke the next frame.
///
/// If this test fails, any connection to a beating server dies as soon as a
/// message arrives after an idle period.
#[tokio::test]
async fn receives_frame_after_heartbeats() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = accept_handshake(&listener, "250,0").await;
        for _ in 0..3 {
            sock.write_all(b"\n").await.expect("Send heartbeat");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        sock.write_all(b"MESSAGE\ndestination:/test\nmessage-id:1\nsubscription:s\n\nhi\0")
            .await
            .expect("Send message");
        // Keep the socket open until the client has read the frame
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        // Only ask to receive, so the client itself stays silent
        .heartbeat(0, 250)
        .connect()
        .await
        .expect("Connect");

    let msg = conn
        .next()
        .await
        .expect("A message")
        .expect("A well formed message");

    let FromServer::Message { body, .. } = msg.content else {
        panic!("Expected a MESSAGE, got {:?}", msg.content);
    };
    assert_eq!(body.as_deref(), Some(&b"hi"[..]));

    server.await.expect("Server task");
}

/// Tests that a server which stops beating is detected
///
/// Having negotiated an incoming interval, silence past that interval means the
/// connection is dead even though the socket is still open.
///
/// If this test fails, a client can wait forever on a connection to a server
/// that is gone.
#[tokio::test]
async fn detects_silent_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (sock, _) = accept_handshake(&listener, "250,0").await;
        // Go quiet without closing the socket
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(sock);
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(0, 250)
        .connect()
        .await
        .expect("Connect");

    let result = tokio::time::timeout(Duration::from_secs(2), conn.next())
        .await
        .expect("The stream should fail rather than hang");

    assert!(
        matches!(result, Some(Err(_))),
        "Expected an error, got {result:?}"
    );

    server.abort();
}

/// Tests that a failing write is reported by the send that caused it
///
/// Writing goes through a background pump, and the caller has to keep getting
/// the real result: an application publishing to a queue needs to know right
/// away whether the message got out.
///
/// If this test fails, a broken connection looks like a series of successful
/// sends and messages are silently lost.
#[tokio::test]
async fn send_reports_write_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (sock, _) = accept_handshake(&listener, "0,0").await;
        // Hang up on the client
        drop(sock);
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .connect()
        .await
        .expect("Connect");
    server.await.expect("Server task");

    // The first write after the peer hangs up still lands in the socket buffer,
    // the error only surfaces once the reset comes back.
    let mut failed = false;
    for _ in 0..20 {
        if conn.send(send_msg()).await.is_err() {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(failed, "Sending on a closed connection should fail");
}

/// Tests that messages reach the server in order and intact
///
/// Outgoing messages are handed to a background pump, which must neither
/// reorder nor mangle them.
///
/// If this test fails, the write pump has broken the ordering guarantee that
/// STOMP clients rely on for subscribe-then-send sequences.
#[tokio::test]
async fn messages_pass_through_the_pump_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = accept_handshake(&listener, "0,0").await;
        let first = read_frame(&mut sock).await;
        let second = read_frame(&mut sock).await;
        (first, second)
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .connect()
        .await
        .expect("Connect");

    conn.send(
        ToServer::Subscribe {
            destination: "/test".into(),
            id: "sub".into(),
            ack: None,
        }
        .into(),
    )
    .await
    .expect("Send subscribe");
    conn.send(send_msg()).await.expect("Send message");

    let (first, second) = server.await.expect("Server task");
    assert!(first.starts_with("SUBSCRIBE"), "Got: {first}");
    assert!(second.starts_with("SEND"), "Got: {second}");
}

/// Tests that dropping the transport closes the connection
///
/// The write half lives in a background task, which has to notice that the
/// transport is gone and shut the socket down rather than leak it.
///
/// If this test fails, sockets and tasks accumulate for the lifetime of the
/// process.
#[tokio::test]
async fn drop_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = accept_handshake(&listener, "0,0").await;
        let mut buf = [0u8; 64];
        sock.read(&mut buf).await.expect("Read from client")
    });

    let conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .connect()
        .await
        .expect("Connect");
    drop(conn);

    let read = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("The server should see the connection close")
        .expect("Server task");

    assert_eq!(read, 0, "Expected end of stream");
}

// The tests below need a real broker that closes idle connections. Start one
// with podman, using the NIO journal so that it also works rootless:
//
// podman run -d --name artemis -p 61613:61613 \
//   -e ARTEMIS_USER=artemis -e ARTEMIS_PASSWORD=artemis \
//   -e EXTRA_ARGS="--http-host 0.0.0.0 --relax-jolokia --nio" \
//   docker.io/apache/activemq-artemis:latest
//
// Then run them with `cargo test --test heartbeat -- --ignored`.

/// Artemis with its default connection TTL, which is what the tests below
/// measure against
const ARTEMIS_TTL: Duration = Duration::from_secs(60);

/// Waits for the connection to break, up to `limit`
///
/// Returns how long the connection stayed up, or `None` if it outlived `limit`.
async fn time_until_disconnect(
    conn: &mut async_stomp::client::ClientTransport,
    limit: Duration,
) -> Option<Duration> {
    let start = Instant::now();
    match tokio::time::timeout(limit, conn.next()).await {
        // Either an ERROR frame, a parse error, or a clean end of stream
        Ok(_) => Some(start.elapsed()),
        Err(_) => None,
    }
}

/// Tests that Artemis really does drop an idle connection that asked for no
/// heartbeats
///
/// This is the behaviour the heartbeat support exists for, and the reason the
/// README singles Artemis out. If the broker stopped doing this, the advice in
/// the documentation would be wrong.
#[tokio::test]
#[ignore]
async fn artemis_drops_idle_connection_without_heartbeat() {
    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("artemis".to_string())
        .passcode("artemis".to_string())
        .connect()
        .await
        .expect("Connect to Artemis");

    assert_eq!(conn.heartbeat(), (None, None));

    let lasted = time_until_disconnect(&mut conn, ARTEMIS_TTL * 2)
        .await
        .expect("Artemis should drop an idle connection with no heartbeats");

    println!("Idle connection without heartbeats lasted {lasted:?}");
    assert!(
        lasted < ARTEMIS_TTL * 2,
        "Expected a drop around the {ARTEMIS_TTL:?} connection TTL, lasted {lasted:?}"
    );
}

/// Tests that heartbeats keep an idle connection alive past the broker's TTL
///
/// The whole point of the feature: an application that subscribes and then sits
/// quiet has to stay connected.
///
/// If this test fails, long-lived low-traffic subscriptions are dropped despite
/// the heartbeat setting, meaning the beats are not going out or not going out
/// often enough.
#[tokio::test]
#[ignore]
async fn artemis_keeps_idle_connection_with_heartbeat() {
    let mut conn = Connector::builder()
        .server("localhost:61613")
        .virtualhost("/")
        .login("artemis".to_string())
        .passcode("artemis".to_string())
        .heartbeat(2_000, 2_000)
        .connect()
        .await
        .expect("Connect to Artemis");

    println!("Artemis negotiated {:?}", conn.heartbeat());
    let (outgoing, _) = conn.heartbeat();
    assert!(outgoing.is_some(), "Artemis should accept our heartbeats");

    // Well past the TTL that dropped the connection in the test above
    let broke = time_until_disconnect(&mut conn, ARTEMIS_TTL + Duration::from_secs(30)).await;
    assert!(broke.is_none(), "Connection broke after {broke:?}");

    // Still usable, not merely still open
    conn.send(send_msg()).await.expect("Send after idling");
}

/// Tests that automatic heart-beating can be turned off while still negotiating
///
/// Some applications want to drive the beating themselves, for instance to tie
/// it to their own scheduling. Opting out has to keep the `heart-beat` header
/// and the negotiation, since that is what tells the application how often to
/// beat, and only stop the acting on it.
///
/// If this test fails, opting out either silently keeps beating or loses the
/// negotiated intervals, leaving the application without the information it
/// needs to do the job itself.
#[tokio::test]
async fn manual_heartbeat_opt_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let (idle_tx, idle_rx) = futures::channel::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut sock, connect) = accept_handshake(&listener, "250,250").await;
        // The header still goes out, that is the whole point of opting out
        // rather than simply not asking for heartbeats
        assert!(connect.contains("heart-beat:250,250"), "Got: {connect}");

        let idle = collect_for(&mut sock, Duration::from_millis(700)).await;
        idle_tx.send(()).expect("Signal the client");
        (
            idle,
            collect_for(&mut sock, Duration::from_millis(300)).await,
        )
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(250, 250)
        .auto_heartbeat(false)
        .connect()
        .await
        .expect("Connect");

    // The application is told what to beat at, even though nothing beats for it
    assert_eq!(
        conn.heartbeat(),
        (
            Some(Duration::from_millis(250)),
            Some(Duration::from_millis(250))
        )
    );

    // Stay idle well past the interval, then beat by hand
    idle_rx.await.expect("Wait for the idle window");
    conn.send_heartbeat().await.expect("Send heartbeat");

    let (idle, manual) = server.await.expect("Server task");
    assert!(
        idle.is_empty(),
        "Nothing should be sent automatically, got: {idle:?}"
    );
    assert_eq!(manual, b"\n", "Expected exactly one heartbeat");
}

/// Tests that opting out also stops the client from failing a quiet server
///
/// Supervising the incoming direction is the other half of the automatic
/// handling, and an application that took the job over may well have its own
/// idea of when a server counts as gone.
///
/// If this test fails, an application that opted out still gets its stream
/// killed on a schedule it did not choose.
#[tokio::test]
async fn manual_opt_out_does_not_supervise_the_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let server = tokio::spawn(async move {
        let (sock, _) = accept_handshake(&listener, "250,0").await;
        // Go quiet, well past twice the negotiated interval
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(sock);
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .heartbeat(0, 250)
        .auto_heartbeat(false)
        .connect()
        .await
        .expect("Connect");

    assert_eq!(conn.heartbeat().1, Some(Duration::from_millis(250)));

    let quiet = tokio::time::timeout(Duration::from_secs(1), conn.next()).await;
    assert!(
        quiet.is_err(),
        "The stream should stay pending, got {quiet:?}"
    );

    server.abort();
}

/// Tests that a connection without heart-beating still batches its writes
///
/// Such a connection writes straight to the socket, so `feed` buffers exactly
/// as `Framed` has always done. A connection that beats cannot do this: it has
/// to settle each write to keep reporting the result, which is why the two
/// differ here and nowhere else.
///
/// If this test fails, the default connection has picked up the write pump and
/// with it a background task, a channel hop and the loss of batching, none of
/// which it needs.
#[tokio::test]
async fn plain_connection_still_batches() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("Bind");
    let addr = listener.local_addr().expect("Local address").to_string();

    let (fed_tx, fed_rx) = futures::channel::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut sock, connect) = accept_handshake(&listener, "0,0").await;
        assert!(
            !connect.contains("heart-beat"),
            "No header without a request, got: {connect}"
        );

        let buffered = collect_for(&mut sock, Duration::from_millis(300)).await;
        fed_tx.send(()).expect("Signal the client");
        (
            buffered,
            collect_for(&mut sock, Duration::from_millis(300)).await,
        )
    });

    let mut conn = Connector::builder()
        .server(addr)
        .virtualhost("/")
        .connect()
        .await
        .expect("Connect");

    conn.feed(send_msg()).await.expect("Feed first");
    conn.feed(send_msg()).await.expect("Feed second");

    // Nothing should have left the client yet
    fed_rx.await.expect("Wait for the buffered window");
    conn.flush().await.expect("Flush");

    let (buffered, flushed) = server.await.expect("Server task");
    assert!(
        buffered.is_empty(),
        "Fed messages should still be buffered, got: {:?}",
        String::from_utf8_lossy(&buffered)
    );
    assert_eq!(
        flushed.iter().filter(|&&b| b == b'\0').count(),
        2,
        "Both messages should arrive on flush"
    );
}
