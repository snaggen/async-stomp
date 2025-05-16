use crate::{AckMode, FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};
use bytes::{BufMut, BytesMut};
use std::borrow::Cow;
use winnow::{
    ModalResult, Parser, Partial,
    ascii::{alpha1, line_ending, till_line_ending},
    combinator::{delimited, opt, repeat, separated_pair, terminated, trace},
    error::StrContext::Label,
    token::{literal, take, take_till, take_until},
};

/// Type definitions for working with headers
/// HeaderTuple represents a header key with an optional value
type HeaderTuple<'a> = (&'a [u8], Option<Cow<'a, [u8]>>);
/// Header represents a header key with a mandatory value
type Header<'a> = (&'a [u8], Cow<'a, [u8]>);

/// Low-level representation of a STOMP protocol frame
///
/// A Frame is the low-level representation of a STOMP message as it appears
/// on the wire. It consists of a command, headers, and an optional body.
/// This is generally used internally and converted to/from the higher-level
/// Message<T> types.
#[derive(Debug)]
pub(crate) struct Frame<'a> {
    /// The STOMP command (e.g., "CONNECT", "SEND", "MESSAGE")
    command: &'a [u8],
    /// Headers as key-value pairs
    // TODO use ArrayVec to keep headers on the stack
    // (makes this object zero-allocation)
    headers: Vec<Header<'a>>,
    /// Optional message body
    body: Option<&'a [u8]>,
}

impl<'a> Frame<'a> {
    /// Create a new Frame with the given command, headers, and body
    ///
    /// This function constructs a new Frame from its component parts.
    /// Headers with None values will be filtered out.
    pub(crate) fn new(
        command: &'a [u8],
        headers: &[HeaderTuple<'a>],
        body: Option<&'a [u8]>,
    ) -> Frame<'a> {
        // Filter out headers with None values and convert to the required format
        let headers = headers
            .iter()
            // filter out headers with None value
            .filter_map(|&(k, ref v)| v.as_ref().map(|i| (k, i.clone())))
            .collect();
        Frame {
            command,
            headers,
            body,
        }
    }

    /// Serialize this frame into a byte buffer
    ///
    /// This method writes the frame in STOMP wire format to the provided BytesMut buffer.
    /// The format is:
    /// - Command followed by newline
    /// - Headers (key:value) each followed by newline
    /// - Empty line (newline)
    /// - Body (if present)
    /// - Null byte terminator
    pub(crate) fn serialize(&self, buffer: &mut BytesMut) {
        /// Helper function to write bytes with proper escaping of special characters
        fn write_escaped(b: u8, buffer: &mut BytesMut) {
            match b {
                b'\r' => buffer.put_slice(b"\\r"),  // Carriage return
                b'\n' => buffer.put_slice(b"\\n"),  // Line feed
                b':' => buffer.put_slice(b"\\c"),   // Colon
                b'\\' => buffer.put_slice(b"\\\\"), // Backslash
                b => buffer.put_u8(b),              // Regular character
            }
        }

        // Calculate required capacity to avoid reallocations
        let requires = self.command.len()
            + self.body.map(|b| b.len() + 20).unwrap_or(0)
            + self
                .headers
                .iter()
                .fold(0, |acc, (k, v)| acc + k.len() + v.len())
            + 30;

        // Ensure buffer has enough space
        if buffer.remaining_mut() < requires {
            buffer.reserve(requires);
        }

        // Write command
        buffer.put_slice(self.command);
        buffer.put_u8(b'\n');

        // Write headers
        self.headers.iter().for_each(|&(key, ref val)| {
            // Write key with proper escaping
            for byte in key {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b':');

            // Write value with proper escaping
            for byte in val.iter() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b'\n');
        });

        // Empty line separating headers from body
        buffer.put_u8(b'\n');

        // Write body if present
        if let Some(body) = self.body {
            buffer.put_slice(body);
        }

        // Null byte terminator
        buffer.put_u8(b'\x00');
    }

    /// Add extra headers to this frame if they don't already exist
    ///
    /// This method adds headers from the provided collection to the frame,
    /// but only if a header with the same key doesn't already exist.
    pub fn add_extra_headers(&mut self, headers: &'a [(Vec<u8>, Vec<u8>)]) {
        if !headers.is_empty() {
            // Create a set of existing header keys for efficient lookup
            let existing_headers: Vec<&[u8]> = self.headers.iter().map(|(k, _v)| *k).collect();

            // Add headers that don't already exist
            headers
                .iter()
                .filter(|f| !existing_headers.contains(&f.0.as_ref()))
                .for_each(|(k, v)| {
                    self.headers.push((k.as_ref(), Cow::Borrowed(v.as_ref())));
                });
        }
    }
}

// Parsing functions using the winnow crate

/// Extract the content-length value from headers if present
///
/// This helper function looks for a content-length header and parses its value
/// as a u32 if found.
fn get_content_length(headers: &[(&[u8], Cow<[u8]>)]) -> Option<u32> {
    for h in headers {
        if h.0 == b"content-length" {
            return std::str::from_utf8(&h.1)
                .ok()
                .and_then(|v| v.parse::<u32>().ok());
        }
    }
    None
}

/// Convert an empty slice to None, or Some(slice) if not empty
///
/// This helper function is used during parsing to convert empty body slices to None.
fn is_empty_slice(s: &[u8]) -> Option<&[u8]> {
    if s.is_empty() { None } else { Some(s) }
}

/// Parse a complete STOMP frame from a byte buffer
///
/// This function attempts to parse a complete STOMP frame from the input.
/// If successful, it returns the parsed Frame.
/// If the input doesn't contain a complete frame, it returns Incomplete.
/// If the input contains invalid data, it returns an error.
pub fn parse_frame<'a>(input: &mut Partial<&'a [u8]>) -> ModalResult<Frame<'a>> {
    // Parse the command and headers
    let (command, headers): (_, Vec<_>) = trace(
        "parse_frame",
        (
            delimited(opt(line_ending.complete_err()), alpha1, line_ending), // command
            terminated(
                repeat(0.., parse_header), // header
                line_ending,
            ),
        ),
    )
    .context(Label("Command/Headers"))
    .parse_next(input)?;

    // Parse the body according to the content-length header if present
    let body: Option<&[u8]> = match get_content_length(&headers) {
        None => take_until(0.., "\x00")
            .map(is_empty_slice)
            .context(Label("Body (null terminated)"))
            .parse_next(input)?,
        Some(length) => take(length)
            .map(Some)
            .context(Label("Body (fixed size)"))
            .parse_next(input)?,
    };

    // Parse the frame terminator
    (literal("\x00"), opt(line_ending.complete_err()))
        .context(Label("NullTermination/LineEnding"))
        .parse_next(input)?;

    Ok(Frame {
        command,
        headers,
        body,
    })
}

/// Parse a single STOMP header from a byte buffer
///
/// This function parses a single header in the format "key:value\n".
/// It returns the parsed header as a (key, value) tuple.
pub fn parse_header<'a>(input: &mut Partial<&'a [u8]>) -> ModalResult<Header<'a>> {
    trace(
        "parse_header",
        separated_pair(
            take_till(1.., [':', '\r', '\n']),
            literal(":"),
            terminated(till_line_ending, line_ending).map(Cow::Borrowed),
        ),
    )
    .context(Label("Header"))
    .parse_next(input)
}

/// Unescape a header value according to STOMP spec
///
/// Converts escaped sequences back to their original characters:
/// - \r -> carriage return
/// - \n -> line feed
/// - \c -> colon
/// - \\ -> backslash
fn unescape_header_value(value: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(value.len());
    let mut i = 0;

    while i < value.len() {
        if value[i] == b'\\' && i + 1 < value.len() {
            match value[i + 1] {
                b'r' => result.push(b'\r'),
                b'n' => result.push(b'\n'),
                b'c' => result.push(b':'),
                b'\\' => result.push(b'\\'),
                _ => {
                    // If not a recognized escape sequence, keep as is
                    result.push(value[i]);
                    result.push(value[i + 1]);
                }
            }
            i += 2;
        } else {
            result.push(value[i]);
            i += 1;
        }
    }

    result
}

/// Fetch a header value by key from a collection of headers
///
/// This helper function looks up a header by key and returns its value
/// as a String if found, or None if not found.
fn fetch_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Option<String> {
    let kk = key.as_bytes();
    for &(k, ref v) in headers {
        if k == kk {
            // Unescape any escape sequences in the header value
            let unescaped = unescape_header_value(v);
            return String::from_utf8(unescaped).ok();
        }
    }
    None
}

/// Convert all headers to a collection of (String, String) pairs
///
/// This helper function converts raw binary headers to String pairs,
/// which is more convenient for higher-level message types.
fn all_headers<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)]) -> Vec<(String, String)> {
    let mut res = Vec::new();
    for &(k, ref v) in headers {
        // Unescape any escape sequences in the header value
        let unescaped = unescape_header_value(v);
        let entry = (
            String::from_utf8(k.to_vec()).unwrap(),
            String::from_utf8(unescaped).unwrap(),
        );
        res.push(entry);
    }
    res
}

/// Extract optional headers that aren't in the expected_keys list
///
/// This helper function extracts headers that aren't part of the required
/// headers for a specific message type, and returns them as a Vec of String pairs.
fn optional_headers<'a>(
    headers: &'a [(&'a [u8], Cow<'a, [u8]>)],
    expected_keys: &[&[u8]],
) -> Option<Vec<(String, String)>> {
    let res: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !expected_keys.contains(k))
        .map(|(k, v)| {
            // Unescape any escape sequences in the header value
            let unescaped = unescape_header_value(v);
            (
                String::from_utf8(k.to_vec()).unwrap(),
                String::from_utf8(unescaped).unwrap(),
            )
        })
        .collect();
    if res.is_empty() { None } else { Some(res) }
}

/// Fetch a required header value by key from a collection of headers
///
/// This function looks up a header by key and returns its value as a String.
/// If the header is not found, it returns an error.
fn expect_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Result<String> {
    fetch_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

impl<'a> Frame<'a> {
    /// Convert this frame to a Message<ToServer>
    ///
    /// This method interprets the frame as a client-to-server message
    /// and converts it to the appropriate ToServer enum variant.
    #[allow(dead_code)]
    pub(crate) fn to_client_msg(&'a self) -> Result<Message<ToServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use ToServer::*;
        let h = &self.headers;
        let expect_keys: &[&[u8]];

        // Determine the message type based on the command and create the appropriate content
        let content = match self.command {
            b"STOMP" | b"CONNECT" | b"stomp" | b"connect" => {
                expect_keys = &[
                    b"accept-version",
                    b"host",
                    b"login",
                    b"passcode",
                    b"heart-beat",
                ];
                let heartbeat = if let Some(hb) = fh(h, "heart-beat") {
                    Some(parse_heartbeat(&hb)?)
                } else {
                    None
                };
                Connect {
                    accept_version: eh(h, "accept-version")?,
                    host: eh(h, "host")?,
                    login: fh(h, "login"),
                    passcode: fh(h, "passcode"),
                    heartbeat,
                }
            }
            b"DISCONNECT" | b"disconnect" => {
                expect_keys = &[b"receipt"];
                Disconnect {
                    receipt: fh(h, "receipt"),
                }
            }
            b"SEND" | b"send" => {
                expect_keys = &[b"destination", b"transaction"];
                Send {
                    destination: eh(h, "destination")?,
                    transaction: fh(h, "transaction"),
                    headers: optional_headers(h, expect_keys),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            b"SUBSCRIBE" | b"subscribe" => {
                expect_keys = &[b"destination", b"id", b"ack"];
                Subscribe {
                    destination: eh(h, "destination")?,
                    id: eh(h, "id")?,
                    ack: match fh(h, "ack").as_deref() {
                        Some("auto") => Some(AckMode::Auto),
                        Some("client") => Some(AckMode::Client),
                        Some("client-individual") => Some(AckMode::ClientIndividual),
                        Some(other) => bail!("Invalid ack mode: {}", other),
                        None => None,
                    },
                }
            }
            b"UNSUBSCRIBE" | b"unsubscribe" => {
                expect_keys = &[b"id"];
                Unsubscribe { id: eh(h, "id")? }
            }
            b"ACK" | b"ack" => {
                expect_keys = &[b"id", b"transaction"];
                Ack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"NACK" | b"nack" => {
                expect_keys = &[b"id", b"transaction"];
                Nack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"BEGIN" | b"begin" => {
                expect_keys = &[b"transaction"];
                Begin {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"COMMIT" | b"commit" => {
                expect_keys = &[b"transaction"];
                Commit {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"ABORT" | b"abort" => {
                expect_keys = &[b"transaction"];
                Abort {
                    transaction: eh(h, "transaction")?,
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };

        // Collect any extra headers not required by the specific message type
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (v).to_vec()))
                } else {
                    None
                }
            })
            .collect();

        Ok(Message {
            content,
            extra_headers,
        })
    }

    /// Convert this frame to a Message<FromServer>
    ///
    /// This method interprets the frame as a server-to-client message
    /// and converts it to the appropriate FromServer enum variant.
    pub(crate) fn to_server_msg(&'a self) -> Result<Message<FromServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use FromServer::{Connected, Error, Message as Msg, Receipt};
        let h = &self.headers;
        let expect_keys: &[&[u8]];

        // Determine the message type based on the command and create the appropriate content
        let content = match self.command {
            b"CONNECTED" | b"connected" => {
                expect_keys = &[b"version", b"session", b"server", b"heart-beat"];
                Connected {
                    version: eh(h, "version")?,
                    session: fh(h, "session"),
                    server: fh(h, "server"),
                    heartbeat: fh(h, "heart-beat"),
                }
            }
            b"MESSAGE" | b"message" => {
                expect_keys = &[b"destination", b"message-id", b"subscription"];
                Msg {
                    destination: eh(h, "destination")?,
                    message_id: eh(h, "message-id")?,
                    subscription: eh(h, "subscription")?,
                    headers: all_headers(h),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            b"RECEIPT" | b"receipt" => {
                expect_keys = &[b"receipt-id"];
                Receipt {
                    receipt_id: eh(h, "receipt-id")?,
                }
            }
            b"ERROR" | b"error" => {
                expect_keys = &[b"message"];
                Error {
                    message: fh(h, "message"),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };

        // Collect any extra headers not required by the specific message type
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (v).to_vec()))
                } else {
                    None
                }
            })
            .collect();

        Ok(Message {
            content,
            extra_headers,
        })
    }
}

/// Convert an Option<String> to Option<Cow<[u8]>>
///
/// This helper function is used when creating frames to convert String
/// header values to the binary format needed by the Frame struct.
fn opt_str_to_bytes(s: &Option<String>) -> Option<Cow<'_, [u8]>> {
    s.as_ref().map(|v| Cow::Borrowed(v.as_bytes()))
}

/// Parse a heartbeat header value into a (u32, u32) tuple
///
/// This helper function parses the heart-beat header value which is
/// in the format "cx,cy" where cx is the client's heartbeat interval
/// and cy is the server's heartbeat interval.
fn parse_heartbeat(hb: &str) -> Result<(u32, u32)> {
    let mut split = hb.splitn(2, ',');
    let left = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    let right = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    Ok((left.parse()?, right.parse()?))
}

impl ToServer {
    /// Convert this ToServer enum to a Frame
    ///
    /// This method creates a Frame representation of the message
    /// that can be serialized and sent over the wire.
    pub(crate) fn to_frame(&self) -> Frame {
        use self::opt_str_to_bytes as sb;
        use Cow::*;
        use ToServer::*;

        // Create a Frame with the appropriate command, headers, and body
        // based on the ToServer variant
        match *self {
            Connect {
                ref accept_version,
                ref host,
                ref login,
                ref passcode,
                ref heartbeat,
            } => Frame::new(
                b"CONNECT",
                &[
                    (b"accept-version", Some(Borrowed(accept_version.as_bytes()))),
                    (b"host", Some(Borrowed(host.as_bytes()))),
                    (b"login", sb(login)),
                    (
                        b"heart-beat",
                        heartbeat.map(|(v1, v2)| Owned(format!("{},{}", v1, v2).into())),
                    ),
                    (b"passcode", sb(passcode)),
                ],
                None,
            ),
            Disconnect { ref receipt } => {
                Frame::new(b"DISCONNECT", &[(b"receipt", sb(receipt))], None)
            }
            Subscribe {
                ref destination,
                ref id,
                ref ack,
            } => Frame::new(
                b"SUBSCRIBE",
                &[
                    (b"destination", Some(Borrowed(destination.as_bytes()))),
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (
                        b"ack",
                        ack.map(|ack| match ack {
                            AckMode::Auto => Borrowed(&b"auto"[..]),
                            AckMode::Client => Borrowed(&b"client"[..]),
                            AckMode::ClientIndividual => Borrowed(&b"client-individual"[..]),
                        }),
                    ),
                ],
                None,
            ),
            Unsubscribe { ref id } => Frame::new(
                b"UNSUBSCRIBE",
                &[(b"id", Some(Borrowed(id.as_bytes())))],
                None,
            ),
            Send {
                ref destination,
                ref transaction,
                ref headers,
                ref body,
            } => {
                // Create the base headers for the SEND frame
                let mut hdr: Vec<HeaderTuple> = vec![
                    (b"destination", Some(Borrowed(destination.as_bytes()))),
                    (b"id", sb(transaction)),
                ];

                // Add any custom headers
                if headers.is_some() {
                    for (key, val) in headers.as_ref().unwrap() {
                        hdr.push((key.as_bytes(), Some(Borrowed(val.as_bytes()))));
                    }
                }

                Frame::new(b"SEND", &hdr, body.as_ref().map(|v| v.as_ref()))
            }
            Ack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"ACK",
                &[
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (b"transaction", sb(transaction)),
                ],
                None,
            ),
            Nack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"NACK",
                &[
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (b"transaction", sb(transaction)),
                ],
                None,
            ),
            Begin { ref transaction } => Frame::new(
                b"BEGIN",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
            Commit { ref transaction } => Frame::new(
                b"COMMIT",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
            Abort { ref transaction } => Frame::new(
                b"ABORT",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// For all Frames Client -> Server
    /// https://stomp.github.io/stomp-specification-1.2.html#Client_Frames
    fn parse_and_serialize_to_server(
        data: &[u8],
        frame: Frame<'_>,
        headers_expect: Vec<(&[u8], &[u8])>,
        body_expect: Option<&[u8]>,
    ) {
        let fh: Vec<(&[u8], &[u8])> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        println!("Provided Headers: ");
        for f in &fh {
            println!(
                "  {}: {}",
                std::str::from_utf8(f.0).unwrap(),
                std::str::from_utf8(f.1).unwrap()
            );
        }
        println!("Expected Headers: ");
        for f in &headers_expect {
            println!(
                "  {}: {}",
                std::str::from_utf8(f.0).unwrap(),
                std::str::from_utf8(f.1).unwrap()
            );
        }

        println!("Provided Body: ");
        println!(
            "{}",
            std::str::from_utf8(frame.body.unwrap_or(b"")).unwrap()
        );
        println!("Expected Body: ");
        println!(
            "{}",
            std::str::from_utf8(body_expect.unwrap_or(b"")).unwrap()
        );
        assert_eq!(fh, headers_expect, "headers dont match");
        assert_eq!(frame.body, body_expect, "body doesnt match");
        let stomp = frame.to_client_msg().unwrap();
        let mut buffer = BytesMut::new();
        stomp.to_frame().serialize(&mut buffer);
        println!("left: {}", std::str::from_utf8(&buffer).unwrap());
        println!("right: {}", std::str::from_utf8(data).unwrap());
        assert_eq!(&*buffer, data, "frame data doesnt match");
    }

    /// Tests STOMP CONNECT frame parsing and serialization with a heartbeat configuration
    ///
    /// This test validates that a CONNECT frame with heartbeat configuration can be correctly
    /// parsed from raw bytes into a Frame structure and then serialized back to the exact
    /// same byte representation. It verifies the command, headers, and heart-beat values
    /// are preserved correctly through the parse-serialize cycle.
    ///
    /// If this test fails, it means the STOMP frame parser or serializer is not correctly
    /// handling CONNECT frames with heartbeat configurations, which would cause connection
    /// issues or prevent proper heartbeat functionality when connecting to a STOMP server.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#CONNECT
    /// without heartbeat configured
    fn parse_and_serialize_client_connect_with_heartbeat() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
heart-beat:6,7
passcode:password\n\n\x00"
            .to_vec();
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"heart-beat", b"6,7"),
            (b"passcode", b"password"),
        ];

        assert_eq!(frame.command, b"CONNECT");
        parse_and_serialize_to_server(&data, frame, headers_expect, None);
    }

    /// Tests STOMP CONNECT frame parsing and serialization without heartbeat configuration
    ///
    /// This test validates that a CONNECT frame without heartbeat configuration can be correctly
    /// parsed from raw bytes into a Frame structure and then serialized back to the exact
    /// same byte representation. It verifies the command and headers are preserved correctly
    /// through the parse-serialize cycle.
    ///
    /// If this test fails, it means the STOMP frame parser or serializer is not correctly
    /// handling basic CONNECT frames without optional heartbeat settings, which would cause
    /// connection failures when connecting to a STOMP server.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#CONNECT
    /// with heartbeat configured
    fn parse_and_serialize_client_connect_without_heartbeat() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
passcode:password\n\n\x00";
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        eprintln!("Frame: {frame:?}");
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"passcode", b"password"),
        ];

        assert_eq!(frame.command, b"CONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

    /// Tests STOMP DISCONNECT frame parsing and serialization
    ///
    /// This test validates that a DISCONNECT frame can be correctly parsed from raw bytes
    /// into a Frame structure and then serialized back to the exact same byte representation.
    /// It verifies the command and receipt header are preserved correctly through the
    /// parse-serialize cycle.
    ///
    /// If this test fails, it means the STOMP frame parser or serializer is not correctly
    /// handling DISCONNECT frames, which would cause issues with graceful client disconnection
    /// and potentially leave server resources allocated unnecessarily.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#DISCONNECT
    fn parse_and_serialize_client_disconnect() {
        let data = b"DISCONNECT\nreceipt:77\n\n\x00";
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(b"receipt", b"77")];

        assert_eq!(frame.command, b"DISCONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

    /// Tests STOMP SEND frame parsing and serialization with minimal headers
    ///
    /// This test validates that a SEND frame with only the required destination header
    /// can be correctly parsed from raw bytes into a Frame structure and then serialized
    /// back to the exact same byte representation. It verifies the command, headers, and body
    /// are preserved correctly through the parse-serialize cycle.
    ///
    /// If this test fails, it means the STOMP frame parser or serializer is not correctly
    /// handling basic SEND frames, which would prevent clients from sending messages to
    /// destinations in the message broker.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#SEND
    /// minimum headers set without
    /// note: first \x00 will terminate body without content-length header!
    /// https://stomp.github.io/stomp-specification-1.2.html#Header_content-length
    fn parse_and_serialize_client_send_message_minimum() {
        let mut data = b"SEND\ndestination:/queue/a\n\n".to_vec();
        let body = b"this body contains no nulls \n and \n newlines OK?";
        data.extend_from_slice(body);
        data.extend_from_slice(b"\x00");
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(&b"destination"[..], &b"/queue/a"[..])];
        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body));
    }

    /// Tests STOMP SEND frame parsing and serialization with content-length and content-type headers
    ///
    /// This test validates that a SEND frame with recommended headers (content-type and content-length)
    /// can be correctly parsed from raw bytes into a Frame structure and then serialized back to the
    /// exact same byte representation. It especially verifies that a message body containing null bytes
    /// is handled correctly when a content-length header is present.
    ///
    /// If this test fails, it means the STOMP frame parser or serializer is not correctly handling
    /// SEND frames with content-length headers, which would cause binary message data or message bodies
    /// containing null bytes to be corrupted or incorrectly processed.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#SEND
    /// recommended headers set
    /// note: additional \x00 are only allowed with content-length header!
    /// https://stomp.github.io/stomp-specification-1.2.html#Header_content-length
    fn parse_and_serialize_client_send_message_recommended() {
        let mut data =
            b"SEND\ndestination:/queue/a\ncontent-type:text/html;charset=utf-8\n".to_vec();
        let body = "this body contains \x00 nulls \n and \r\n newlines \x00 OK?";
        let rest = format!("content-length:{}\n\n{}\x00", body.len(), body);
        data.extend_from_slice(rest.as_bytes());
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"/queue/a"[..]),
            (b"content-type", b"text/html;charset=utf-8"),
            (b"content-length", b"50"),
        ];

        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body.as_bytes()));
    }

    /// Tests parsing of STOMP MESSAGE frame with escaped characters in headers
    ///
    /// This test validates that a MESSAGE frame with headers containing escape sequences
    /// (e.g., \n, \r, \c, \\) can be correctly parsed and the escape sequences properly
    /// converted to their corresponding characters. It verifies the header unescape
    /// functionality works correctly for all supported escape sequences.
    ///
    /// If this test fails, it means the header unescaping mechanism is not working correctly,
    /// which would cause headers with special characters to be misinterpreted and potentially
    /// lead to protocol errors or incorrect message routing.
    #[test]
    /// Test parsing of message with all escape sequences
    fn parse_message_with_escaped_characters() {
        // Create a MESSAGE frame with all types of escaped characters
        let data = b"MESSAGE
destination:/queue/test
message-id:ID\\cnotificationator\\n\\rwith\\\\backslash-1\\c1
subscription:sub-123\n\ntest message body\x00";

        // Parse the frame
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();

        // Convert to server message
        let message = frame.to_server_msg().unwrap();

        // Verify that all escape sequences have been unescaped properly
        if let FromServer::Message { message_id, .. } = message.content {
            assert_eq!(
                message_id, "ID:notificationator\n\rwith\\backslash-1:1",
                "Message ID should have all escape sequences unescaped"
            );
        } else {
            panic!("Expected Message type but got: {:?}", message.content);
        }
    }

    /// Tests serialization of STOMP messages with special characters in headers
    ///
    /// This test validates that when creating a STOMP message with special characters
    /// in headers (e.g., colons, newlines, carriage returns, backslashes), these characters
    /// are properly escaped in the serialized output according to the STOMP specification.
    ///
    /// If this test fails, it means the header escaping mechanism is not working correctly,
    /// which would cause messages with special characters in headers to be rejected by the
    /// server or lead to protocol errors when communicating with a STOMP broker.
    #[test]
    /// Test that when sending a message with special characters back to the server,
    /// the special characters are properly escaped
    fn serialize_message_with_special_characters() {
        // Create a message ID with all types of special characters
        let message_id = "ID:notificationator\n\rwith\\backslash-1:1";

        // Create an ACK message with this ID
        let ack_message = Message {
            content: ToServer::Ack {
                id: message_id.to_string(),
                transaction: None,
            },
            extra_headers: vec![],
        };

        // Serialize the message
        let mut buffer = BytesMut::new();
        ack_message.to_frame().serialize(&mut buffer);

        // Check that the serialized message contains the properly escaped characters
        let serialized = String::from_utf8_lossy(&buffer);

        // Each special character should be properly escaped
        assert!(
            serialized.contains("ID\\cnotificationator\\n\\rwith\\\\backslash-1\\c1"),
            "Serialized message should contain properly escaped special characters.\nActual: {}",
            serialized
        );
    }
}
