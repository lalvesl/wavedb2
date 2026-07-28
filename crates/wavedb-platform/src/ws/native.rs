//! Native `connect`: hand-rolled RFC 6455 client handshake over a fresh
//! `TcpStream`, then the [`codec`](super::codec) frames.

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::codec::{
    self, MAX_MESSAGE, Messages, Msg, OP_BINARY, OP_CLOSE, OP_PONG,
};
use crate::error::{Error, Result};

/// Cap on the handshake response head.
const MAX_HEAD: usize = 8 * 1024;

/// The read side past the handshake: bytes the head read pulled early,
/// then the socket.
type ReadSide = tokio::io::Chain<std::io::Cursor<Vec<u8>>, OwnedReadHalf>;

/// An open WebSocket connection — binary messages in both directions.
/// `recv` answers pings internally; the identity handshake above this
/// (`Hello`) belongs to `wavedb-net`.
#[derive(Debug)]
pub struct Conn {
    msgs: Messages<ReadSide>,
    w: OwnedWriteHalf,
}

/// Open a WebSocket to `addr` (`host:port`): TCP connect, upgrade
/// handshake (key from platform entropy, accept verified), ready to
/// exchange binary messages.
///
/// # Errors
/// A socket fault, a non-101 answer, or a wrong `Sec-WebSocket-Accept`.
pub async fn connect(addr: &str) -> Result<Conn> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut nonce = [0u8; 16];
    crate::rand::fill(&mut nonce)?;
    let key = codec::base64(&nonce);
    let head = format!(
        "GET / HTTP/1.1\r\nhost: {addr}\r\nupgrade: websocket\r\n\
         connection: Upgrade\r\nsec-websocket-key: {key}\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    let (head, leftover) = read_head(&mut stream).await?;
    require_upgrade(&head, &codec::accept_key(&key))?;
    let (r, w) = stream.into_split();
    let msgs = Messages::new(tokio::io::AsyncReadExt::chain(
        std::io::Cursor::new(leftover),
        r,
    ));
    Ok(Conn { msgs, w })
}

/// Read the response head up to its blank line; return it with any frame
/// bytes already pulled off the socket.
async fn read_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let leftover = buf.split_off(end + 4);
            buf.truncate(end);
            let text = String::from_utf8(buf)
                .map_err(|_| Error::Http("head is not utf-8"))?;
            return Ok((text, leftover));
        }
        if buf.len() > MAX_HEAD {
            return Err(Error::Http("head too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::Http("connection closed before response"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Require a `101` status line and the accept key the sent nonce demands.
fn require_upgrade(head: &str, expected_accept: &str) -> Result<()> {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    if parts.next() != Some("HTTP/1.1") {
        return Err(Error::Http("not http/1.1"));
    }
    let code: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::Http("bad status line"))?;
    if code != 101 {
        return Err(Error::Status(code));
    }
    let accept = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, value)| value.trim());
    if accept == Some(expected_accept) {
        Ok(())
    } else {
        Err(Error::Http("bad sec-websocket-accept"))
    }
}

impl Conn {
    /// Send one binary message (masked — this is the client side).
    ///
    /// # Errors
    /// A socket fault, or a message past the codec cap.
    pub async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > MAX_MESSAGE {
            return Err(Error::Http("ws frame too large"));
        }
        codec::write_message(&mut self.w, OP_BINARY, bytes, true).await
    }

    /// The next binary message; `None` = the peer closed (a close frame or
    /// the socket's end). Pings are answered internally, pongs ignored.
    ///
    /// # Errors
    /// A socket fault or a protocol violation.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.msgs.next(false).await? {
                Some(Msg::Binary(bytes)) => return Ok(Some(bytes)),
                Some(Msg::Ping(payload)) => {
                    codec::write_message(&mut self.w, OP_PONG, &payload, true)
                        .await?;
                }
                Some(Msg::Pong(_)) => {}
                Some(Msg::Close) => {
                    // Answer the close handshake best-effort; the
                    // connection is over either way.
                    let _ =
                        codec::write_message(&mut self.w, OP_CLOSE, &[], true)
                            .await;
                    return Ok(None);
                }
                None => return Ok(None),
            }
        }
    }

    /// Announce the close (RFC close frame). Dropping the connection
    /// afterwards is the actual teardown.
    ///
    /// # Errors
    /// A socket fault.
    pub async fn close(&mut self) -> Result<()> {
        codec::write_message(&mut self.w, OP_CLOSE, &[], true).await
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::codec::{self, Messages, Msg, OP_BINARY, OP_PING};
    use super::{Error, connect};

    /// A minimal RFC 6455 server: accept one socket, answer the upgrade
    /// with the real accept key, then echo binary messages (and once,
    /// first, ping the client) until close.
    async fn echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let key = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let head = String::from_utf8(buf[..end].to_vec()).unwrap();
                    break head
                        .split("\r\n")
                        .filter_map(|l| l.split_once(':'))
                        .find(|(n, _)| {
                            n.eq_ignore_ascii_case("sec-websocket-key")
                        })
                        .map(|(_, v)| v.trim().to_owned())
                        .unwrap();
                }
            };
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\
                 connection: Upgrade\r\nsec-websocket-accept: {}\r\n\r\n",
                codec::accept_key(&key)
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            let (r, mut w) = sock.split();
            codec::write_message(&mut w, OP_PING, b"beat", false)
                .await
                .unwrap();
            let mut msgs = Messages::new(r);
            while let Some(msg) = msgs.next(true).await.unwrap() {
                match msg {
                    Msg::Binary(bytes) => {
                        codec::write_message(&mut w, OP_BINARY, &bytes, false)
                            .await
                            .unwrap();
                    }
                    Msg::Pong(p) => assert_eq!(p, b"beat"),
                    Msg::Ping(_) => {}
                    Msg::Close => break,
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn handshake_echo_and_close_roundtrip() {
        let addr = echo_server().await;
        let mut conn = connect(&addr).await.unwrap();
        conn.send(b"one").await.unwrap();
        // The server's ping rides ahead of the echo; recv answers it
        // internally and still yields the binary message.
        assert_eq!(conn.recv().await.unwrap().unwrap(), b"one");
        let big = vec![7u8; 70_000];
        conn.send(&big).await.unwrap();
        assert_eq!(conn.recv().await.unwrap().unwrap(), big);
        conn.close().await.unwrap();
    }

    #[tokio::test]
    async fn non_upgrade_answer_is_a_status_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut sink = [0u8; 1024];
            let _ = sock.read(&mut sink).await;
            sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await
                .unwrap();
        });
        let err = connect(&addr).await.unwrap_err();
        assert!(matches!(err, Error::Status(400)));
    }

    #[tokio::test]
    async fn wrong_accept_key_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut sink = [0u8; 1024];
            let _ = sock.read(&mut sink).await;
            sock.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  sec-websocket-accept: bogus\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let err = connect(&addr).await.unwrap_err();
        assert!(matches!(err, Error::Http("bad sec-websocket-accept")));
    }
}
