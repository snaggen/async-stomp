use anyhow::{anyhow, bail};
use bytes::{BufMut, BytesMut};

use winnow::{
    ascii::{alpha1, line_ending, not_line_ending},
    token::{tag, take, take_till1, take_until0},
    combinator::{opt, repeat, repeat_till0},
    sequence::{delimited, separated_pair, terminated},
    IResult, Parser,
};

use std::borrow::Cow;

use crate::{AckMode, FromServer, Message, Result, ToServer};
type HeaderTuple<'a> = (&'a [u8], Option<Cow<'a, [u8]>>);
type Header<'a> = (&'a [u8], Cow<'a, [u8]>);

#[derive(Debug)]
pub(crate) struct Frame<'a> {
    command: &'a [u8],
    // TODO use ArrayVec to keep headers on the stack
    // (makes this object zero-allocation)
    headers: Vec<Header<'a>>,
    body: Option<&'a [u8]>,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(
        command: &'a [u8],
        headers: &[HeaderTuple<'a>],
        body: Option<&'a [u8]>,
    ) -> Frame<'a> {
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

    pub(crate) fn serialize(&self, buffer: &mut BytesMut) {
        fn write_escaped(b: u8, buffer: &mut BytesMut) {
            match b {
                b'\r' => buffer.put_slice(b"\\r"),
                b'\n' => buffer.put_slice(b"\\n"),
                b':' => buffer.put_slice(b"\\c"),
                b'\\' => buffer.put_slice(b"\\\\"),
                b => buffer.put_u8(b),
            }
        }
        let requires = self.command.len()
            + self.body.map(|b| b.len() + 20).unwrap_or(0)
            + self
                .headers
                .iter()
                .fold(0, |acc, (k, v)| acc + k.len() + v.len())
            + 30;
        if buffer.remaining_mut() < requires {
            buffer.reserve(requires);
        }
        buffer.put_slice(self.command);
        buffer.put_u8(b'\n');
        self.headers.iter().for_each(|&(key, ref val)| {
            for byte in key {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b':');
            for byte in val.iter() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b'\n');
        });
        buffer.put_u8(b'\n');
        if let Some(body) = self.body {
            buffer.put_slice(body);
        }
        buffer.put_u8(b'\x00');
    }
}

// Nom definitions
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

fn is_empty_slice(s: &[u8]) -> Option<&[u8]> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn parse_frame(input: &[u8]) -> IResult<&[u8], Frame> {
    // read stream until header end
    repeat_till0::<&[u8], &[u8], Vec<u8>, Vec<u8>, winnow::error::InputError<&[u8]>, _, _>(
        take(1_usize),
        repeat(1, line_ending),
    )
    .parse_next(input)?;

    let (input, (command, headers)): (_, (_, Vec<_>)) = (
        delimited(opt(line_ending.complete_err()), alpha1, line_ending), // command
        terminated(
            repeat(0.., parse_header), // header
            line_ending,
        ),
    )
        .parse_next(input)?;

    let (input, body) = match get_content_length(&headers) {
        None => take_until0("\x00").map(is_empty_slice).parse_next(input)?,
        Some(length) => take(length).map(Some).parse_next(input)?,
    };

    let (input, _) = (tag("\x00"), opt(line_ending.complete_err())).parse_next(input)?;

    Ok((
        input,
        Frame {
            command,
            headers,
            body,
        },
    ))
}

fn parse_header(input: &[u8]) -> IResult<&[u8], Header> {
    separated_pair(
        take_till1(":\r\n"),
        tag(":"),
        terminated(not_line_ending, line_ending).map(Cow::Borrowed),
    )
    .complete_err()
    .parse_next(input)
}

fn fetch_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Option<String> {
    let kk = key.as_bytes();
    for &(k, ref v) in headers {
        if k == kk {
            return String::from_utf8(v.to_vec()).ok();
        }
    }
    None
}

fn all_headers<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)]) -> Vec<(String, String)> {
    let mut res = Vec::new();
    for &(k, ref v) in headers {
        let entry = (
            String::from_utf8(k.to_vec()).unwrap(),
            String::from_utf8(v.to_vec()).unwrap(),
        );
        res.push(entry);
    }
    res
}

fn optional_headers<'a>(
    headers: &'a [(&'a [u8], Cow<'a, [u8]>)],
    expected_keys: &[&[u8]],
) -> Option<Vec<(String, String)>> {
    let res: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !expected_keys.contains(k))
        .map(|(k, v)| {
            (
                String::from_utf8(k.to_vec()).unwrap(),
                String::from_utf8(v.to_vec()).unwrap(),
            )
        })
        .collect();
    if res.is_empty() {
        None
    } else {
        Some(res)
    }
}

fn expect_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Result<String> {
    fetch_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

impl<'a> Frame<'a> {
    #[allow(dead_code)]
    pub(crate) fn to_client_msg(&'a self) -> Result<Message<ToServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use ToServer::*;
        let h = &self.headers;
        let expect_keys: &[&[u8]];
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

    pub(crate) fn to_server_msg(&'a self) -> Result<Message<FromServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use FromServer::{Connected, Error, Message as Msg, Receipt};
        let h = &self.headers;
        let expect_keys: &[&[u8]];
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

fn opt_str_to_bytes(s: &Option<String>) -> Option<Cow<'_, [u8]>> {
    s.as_ref().map(|v| Cow::Borrowed(v.as_bytes()))
}

fn parse_heartbeat(hb: &str) -> Result<(u32, u32)> {
    let mut split = hb.splitn(2, ',');
    let left = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    let right = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    Ok((left.parse()?, right.parse()?))
}

impl ToServer {
    pub(crate) fn to_frame(&self) -> Frame {
        use self::opt_str_to_bytes as sb;
        use Cow::*;
        use ToServer::*;
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
                let mut hdr: Vec<HeaderTuple> = vec![
                    (b"destination", Some(Borrowed(destination.as_bytes()))),
                    (b"id", sb(transaction)),
                ];
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
        let (_, frame) = parse_frame(&data).unwrap();
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
        let (_, frame) = parse_frame(data).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"passcode", b"password"),
        ];

        assert_eq!(frame.command, b"CONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#DISCONNECT
    fn parse_and_serialize_client_disconnect() {
        let data = b"DISCONNECT\nreceipt:77\n\n\x00";
        let (_, frame) = parse_frame(data).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(b"receipt", b"77")];

        assert_eq!(frame.command, b"DISCONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

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
        let (_, frame) = parse_frame(&data).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(&b"destination"[..], &b"/queue/a"[..])];
        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body));
    }

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
        let (_, frame) = parse_frame(&data).unwrap();
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"/queue/a"[..]),
            (b"content-type", b"text/html;charset=utf-8"),
            (b"content-length", b"50"),
        ];

        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body.as_bytes()));
    }
}
