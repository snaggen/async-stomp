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
        fn write_escaped(b: u8, buffer: &mut BytesMut, escape: bool) {
            match b {
                b'\r' if escape => buffer.put_slice(b"\\r"), // Carriage return
                b'\n' if escape => buffer.put_slice(b"\\n"), // Line feed
                b':' if escape => buffer.put_slice(b"\\c"),  // Colon
                b'\\' if escape => buffer.put_slice(b"\\\\"), // Backslash
                b => buffer.put_u8(b),                       // Regular character
            }
        }

        let escape = headers_are_escaped(self.command);

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
                write_escaped(*byte, buffer, escape);
            }
            buffer.put_u8(b':');

            // Write value with proper escaping
            for byte in val.iter() {
                write_escaped(*byte, buffer, escape);
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

/// Whether the headers of a frame with this command carry escape sequences
///
/// CONNECT and CONNECTED are the two exceptions in the spec, kept unescaped for
/// backward compatibility with STOMP 1.0. A STOMP frame counts as a CONNECT,
/// since a server has to handle the two alike.
fn headers_are_escaped(command: &[u8]) -> bool {
    !matches!(
        command,
        b"CONNECT" | b"connect" | b"CONNECTED" | b"connected" | b"STOMP" | b"stomp"
    )
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

/// Fetch a header value by key, without unescaping it
///
/// For the CONNECT and CONNECTED frames, whose headers never carry escape
/// sequences, so that a literal backslash survives as itself.
fn fetch_raw_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Option<String> {
    let kk = key.as_bytes();
    for &(k, ref v) in headers {
        if k == kk {
            return String::from_utf8(v.to_vec()).ok();
        }
    }
    None
}

/// Fetch a required header value by key, without unescaping it
///
/// The counterpart of [`expect_header`] for CONNECT and CONNECTED.
fn expect_raw_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Result<String> {
    fetch_raw_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

/// Convert all headers to a collection of (String, String) pairs
///
/// This helper function converts raw binary headers to String pairs,
/// which is more convenient for higher-level message types.
fn all_headers<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)]) -> Result<Vec<(String, String)>> {
    headers
        .iter()
        .map(|&(k, ref v)| header_to_string(k, v))
        .collect()
}

/// Convert one header, unescaping its value, into a (String, String) pair
///
/// Headers are UTF-8 by the spec, so bytes that are not are a malformed frame
/// rather than something to render as best we can.
fn header_to_string(key: &[u8], value: &[u8]) -> Result<(String, String)> {
    let key =
        String::from_utf8(key.to_vec()).map_err(|_| anyhow!("Header name is not valid UTF-8"))?;
    // Unescape any escape sequences in the header value
    let value = String::from_utf8(unescape_header_value(value))
        .map_err(|_| anyhow!("Value of header '{}' is not valid UTF-8", key))?;
    Ok((key, value))
}

/// Extract optional headers that aren't in the expected_keys list
///
/// This helper function extracts headers that aren't part of the required
/// headers for a specific message type, and returns them as a Vec of String pairs.
fn optional_headers<'a>(
    headers: &'a [(&'a [u8], Cow<'a, [u8]>)],
    expected_keys: &[&[u8]],
) -> Result<Option<Vec<(String, String)>>> {
    let res: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !expected_keys.contains(k))
        .map(|&(k, ref v)| header_to_string(k, v))
        .collect::<Result<_>>()?;
    Ok(if res.is_empty() { None } else { Some(res) })
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
                // CONNECT headers are not escaped, so they are read as they are
                let heartbeat = if let Some(hb) = fetch_raw_header(h, "heart-beat") {
                    Some(parse_heartbeat(&hb)?)
                } else {
                    None
                };
                Connect {
                    accept_version: expect_raw_header(h, "accept-version")?,
                    host: expect_raw_header(h, "host")?,
                    login: fetch_raw_header(h, "login"),
                    passcode: fetch_raw_header(h, "passcode"),
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
                    headers: optional_headers(h, expect_keys)?,
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
                // CONNECTED headers are not escaped, so they are read as they are
                Connected {
                    version: expect_raw_header(h, "version")?,
                    session: fetch_raw_header(h, "session"),
                    server: fetch_raw_header(h, "server"),
                    heartbeat: fetch_raw_header(h, "heart-beat"),
                }
            }
            b"MESSAGE" | b"message" => {
                expect_keys = &[b"destination", b"message-id", b"subscription", b"ack"];
                Msg {
                    destination: eh(h, "destination")?,
                    message_id: eh(h, "message-id")?,
                    subscription: eh(h, "subscription")?,
                    ack: fh(h, "ack"),
                    headers: all_headers(h)?,
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
    pub(crate) fn to_frame<'a>(&'a self) -> Frame<'a> {
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
                        heartbeat.map(|(v1, v2)| Owned(format!("{v1},{v2}").into())),
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
                    (b"transaction", sb(transaction)),
                ];

                // Add any custom headers
                if headers.is_some() {
                    for (key, val) in headers.as_ref().unwrap() {
                        hdr.push((key.as_bytes(), Some(Borrowed(val.as_bytes()))));
                    }
                }

                // A body holding NULL octets can only be read back if the frame
                // says how long it is, so the spec requires content-length
                // there. It stays off otherwise, where it is merely
                // recommended: brokers such as ActiveMQ read its presence as
                // "this payload is binary" and map the message differently.
                if let Some(body) = body
                    && body.contains(&0)
                    && !hdr.iter().any(|(k, _)| *k == b"content-length")
                {
                    hdr.push((
                        b"content-length",
                        Some(Owned(body.len().to_string().into_bytes())),
                    ));
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
            "Serialized message should contain properly escaped special characters.\nActual: {serialized}"
        );
    }

    /// Tests that a SEND frame names its transaction header `transaction`
    ///
    /// `transaction` is the only optional header the spec gives SEND, and it is
    /// what binds the message to a started transaction. This runs the frame
    /// through the same parse-serialize cycle as the other SEND tests, so it
    /// covers both the name on the wire and the round trip through
    /// `to_client_msg`.
    ///
    /// If this test fails, messages sent inside a transaction are published
    /// straight away instead, and an ABORT no longer takes them back — silently,
    /// since brokers pass an unknown header through as a user header.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#SEND
    fn parse_and_serialize_client_send_message_in_transaction() {
        let mut data = b"SEND\ndestination:/queue/a\ntransaction:tx-1\n\n".to_vec();
        let body = b"sent inside a transaction";
        data.extend_from_slice(body);
        data.extend_from_slice(b"\x00");
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"/queue/a"[..]),
            (b"transaction", b"tx-1"),
        ];

        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body));
    }

    /// Tests that a body holding NULL octets is sent with a content-length
    ///
    /// Without the header a reader stops at the first NULL, so the spec makes
    /// content-length mandatory for such a body. The body is parsed back here
    /// rather than only inspected, since surviving the round trip is the whole
    /// point of the header.
    ///
    /// If this test fails, every binary payload containing a zero byte is
    /// silently truncated at that byte on its way to the broker.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#Header_content-length
    fn send_with_null_octets_in_the_body_sets_content_length() {
        let body = b"binary \x00 payload \x00 here".to_vec();
        let msg: Message<ToServer> = ToServer::Send {
            destination: "/queue/a".into(),
            transaction: None,
            headers: None,
            body: Some(body.clone()),
        }
        .into();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);

        let serialized = String::from_utf8_lossy(&buffer);
        assert!(
            serialized.contains(&format!("content-length:{}", body.len())),
            "Expected a content-length header.\nActual: {serialized:?}"
        );

        let parsed = parse_frame(&mut Partial::new(&buffer[..])).expect("Parse the frame back");
        assert_eq!(
            parsed.body,
            Some(body.as_slice()),
            "The whole body should survive the round trip"
        );
    }

    /// Tests that an ordinary body is sent without a content-length
    ///
    /// For a body without NULL octets the header is only recommended, and
    /// adding it is not free: ActiveMQ reads its presence as "this payload is
    /// binary" and maps the message to a different JMS type. Leaving it off
    /// keeps what every existing caller already puts on the wire.
    ///
    /// If this test fails, messages that used to arrive as text arrive as bytes
    /// instead, which consumers notice.
    #[test]
    fn send_without_null_octets_leaves_content_length_off() {
        let msg: Message<ToServer> = ToServer::Send {
            destination: "/queue/a".into(),
            transaction: None,
            headers: None,
            body: Some(b"an ordinary text body".to_vec()),
        }
        .into();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);
        let serialized = String::from_utf8_lossy(&buffer);

        assert!(
            !serialized.contains("content-length"),
            "Did not expect a content-length header.\nActual: {serialized:?}"
        );
    }

    /// Tests that a caller supplying content-length is not overruled
    ///
    /// The header may be set through the `headers` field, and the frame must
    /// then carry it once. Two entries would make the broker use the first and
    /// the meaning depend on ordering.
    ///
    /// If this test fails, a caller who sets the header themselves produces a
    /// frame with a duplicate, which is at best wasteful and at worst ambiguous.
    #[test]
    fn send_keeps_a_caller_supplied_content_length() {
        let body = b"a\x00b".to_vec();
        let msg: Message<ToServer> = ToServer::Send {
            destination: "/queue/a".into(),
            transaction: None,
            headers: Some(vec![("content-length".into(), body.len().to_string())]),
            body: Some(body),
        }
        .into();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);
        let serialized = String::from_utf8_lossy(&buffer);

        assert_eq!(
            serialized.matches("content-length:").count(),
            1,
            "Expected exactly one content-length header.\nActual: {serialized:?}"
        );
    }

    /// Tests that CONNECT headers go out unescaped
    ///
    /// The spec exempts CONNECT and CONNECTED from header escaping, for
    /// backward compatibility with STOMP 1.0. A colon or a backslash in a
    /// credential is therefore sent as itself.
    ///
    /// If this test fails, a login or passcode containing `:` or `\` reaches the
    /// broker altered, and authentication fails for a reason nothing in the
    /// error points at.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#Value_Encoding
    fn connect_headers_are_not_escaped() {
        let msg: Message<ToServer> = ToServer::Connect {
            accept_version: "1.2".into(),
            host: "stomp.example.com".into(),
            login: Some("DOMAIN\\user".into()),
            passcode: Some("pa:ss\\word".into()),
            heartbeat: None,
        }
        .into();

        let mut buffer = BytesMut::new();
        msg.to_frame().serialize(&mut buffer);
        let serialized = String::from_utf8_lossy(&buffer);

        assert!(
            serialized.contains("login:DOMAIN\\user\n"),
            "Login should be sent verbatim.\nActual: {serialized:?}"
        );
        assert!(
            serialized.contains("passcode:pa:ss\\word\n"),
            "Passcode should be sent verbatim.\nActual: {serialized:?}"
        );
    }

    /// Tests that CONNECTED headers are read unescaped
    ///
    /// The mirror image of the rule above: a backslash in a `session` or
    /// `server` value is a backslash, not the start of an escape sequence.
    ///
    /// If this test fails, session identifiers and server names containing a
    /// backslash are mangled on the way in.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#Value_Encoding
    fn connected_headers_are_not_unescaped() {
        let data = b"CONNECTED\nversion:1.2\nsession:ID\\c1\nserver:ActiveMQ\\n5\n\n\x00";
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let message = frame.to_server_msg().expect("Read the CONNECTED frame");

        let FromServer::Connected {
            session, server, ..
        } = message.content
        else {
            panic!("Expected a CONNECTED frame but got: {:?}", message.content);
        };
        assert_eq!(session.as_deref(), Some("ID\\c1"));
        assert_eq!(server.as_deref(), Some("ActiveMQ\\n5"));
    }

    /// Tests that a header which is not valid UTF-8 is reported, not panicked on
    ///
    /// Headers are UTF-8 by the spec, so these bytes are a malformed frame. A
    /// client has to reject them: the conversion runs inside the codec, on
    /// whatever task polls the transport, where a panic takes the task with it.
    ///
    /// If this test fails, anything on the wire able to put a stray byte in a
    /// header can bring the client down.
    #[test]
    fn a_header_that_is_not_utf8_is_an_error() {
        let from_server =
            b"MESSAGE\ndestination:/queue/a\nmessage-id:1\nsubscription:sub-1\nx-bin:\xff\xfe\n\n\x00";
        let frame = parse_frame(&mut Partial::new(from_server.as_slice())).unwrap();
        assert!(
            frame.to_server_msg().is_err(),
            "A MESSAGE with a non-UTF-8 header should be rejected"
        );

        let to_server = b"SEND\ndestination:/queue/a\nx-bin:\xff\xfe\n\n\x00";
        let frame = parse_frame(&mut Partial::new(to_server.as_slice())).unwrap();
        assert!(
            frame.to_client_msg().is_err(),
            "A SEND with a non-UTF-8 header should be rejected"
        );
    }

    /// Tests that a MESSAGE carries the value an ACK has to quote
    ///
    /// STOMP 1.2 acknowledges by the `ack` header, and it is a different value
    /// from `message-id`. Reaching it used to mean scanning the header list
    /// while `message_id` sat there as a field, which is how the wrong one gets
    /// picked.
    ///
    /// If this test fails, the value needed to acknowledge a message is not
    /// exposed and callers fall back to `message_id`, which a 1.2 broker
    /// rejects with an ERROR and a closed connection.
    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#MESSAGE
    fn a_message_carries_its_ack_value() {
        let data =
            b"MESSAGE\ndestination:/queue/a\nmessage-id:mid-1\nsubscription:sub-1\nack:ack-1\n\nbody\x00";
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let message = frame.to_server_msg().expect("Read the MESSAGE frame");

        let FromServer::Message {
            ack, message_id, ..
        } = message.content
        else {
            panic!("Expected a MESSAGE frame");
        };
        assert_eq!(ack.as_deref(), Some("ack-1"));
        assert_ne!(
            ack.as_deref(),
            Some(message_id.as_str()),
            "The two are different values, which is the whole point"
        );
    }

    /// Tests that a MESSAGE needing no acknowledgment has no ack value
    ///
    /// The header is only sent for a subscription in client or
    /// client-individual mode, so in auto mode there is nothing to quote.
    ///
    /// If this test fails, callers are handed an acknowledgment value that the
    /// broker never issued.
    #[test]
    fn a_message_in_auto_mode_has_no_ack_value() {
        let data =
            b"MESSAGE\ndestination:/queue/a\nmessage-id:mid-1\nsubscription:sub-1\n\nbody\x00";
        let frame = parse_frame(&mut Partial::new(data.as_slice())).unwrap();
        let message = frame.to_server_msg().expect("Read the MESSAGE frame");

        let FromServer::Message { ack, .. } = message.content else {
            panic!("Expected a MESSAGE frame");
        };
        assert_eq!(ack, None);
    }
}
