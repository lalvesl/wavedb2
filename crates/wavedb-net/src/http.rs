//! Minimal HTTP/1.1 framing for the dumb tunnel (native only).
//!
//! WaveDB uses no HTTP semantics: a request is `POST` + `content-length` +
//! the envelope bytes; a response is a `200` head with **no content-length**
//! followed by length-prefixed frames (`[len u32 LE][bytes]`, each a wire
//! [`StreamFrame`](crate::frame::StreamFrame)) written progressively — the
//! `connection: close` the tunnel always sends is what delimits the body.
//! Anything else on the socket is a transport fault, not a protocol.
//! Identity, commands, and refusals never touch this layer — they live in
//! the [`frame`](crate::frame) envelopes.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

/// Cap on the head section (request/status line + headers).
const MAX_HEAD: usize = 8 * 1024;

/// Cap on a declared request body and on one response frame. One frame is
/// one operation or one walk item — nothing legitimate approaches this.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// A parsed head: the first line plus the one header the tunnel reads.
#[derive(Debug)]
struct Head {
    first_line: String,
    content_length: Option<usize>,
}

/// Split the head bytes into the first line + `content-length`.
fn parse_head(bytes: &[u8]) -> Result<Head> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| Error::Http("head is not utf-8"))?;
    let mut lines = text.split("\r\n");
    let first_line = lines.next().ok_or(Error::Http("empty head"))?.to_owned();
    let mut content_length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            let n: usize = value
                .trim()
                .parse()
                .map_err(|_| Error::Http("bad content-length"))?;
            content_length = Some(n);
        }
    }
    Ok(Head {
        first_line,
        content_length,
    })
}

/// Position of the `\r\n\r\n` head terminator, if present.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read up to and including the blank line; return `(head, leftover)` where
/// `leftover` is any body bytes already pulled off the socket. `None` = the
/// peer closed before sending anything (a clean end of connection).
async fn read_head<R>(r: &mut R) -> Result<Option<(Head, Vec<u8>)>>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head_end(&buf) {
            let leftover = buf.split_off(end + 4);
            buf.truncate(end);
            return Ok(Some((parse_head(&buf)?, leftover)));
        }
        if buf.len() > MAX_HEAD {
            return Err(Error::Http("head too large"));
        }
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(Error::Http("connection closed mid-head"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read the declared body, reusing bytes already read past the head.
async fn read_body<R>(
    r: &mut R,
    declared: usize,
    mut leftover: Vec<u8>,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    if declared > MAX_BODY {
        return Err(Error::BodyTooLarge {
            limit: MAX_BODY,
            have: declared,
        });
    }
    if leftover.len() > declared {
        return Err(Error::Http("body longer than declared"));
    }
    let missing = declared - leftover.len();
    if missing > 0 {
        let start = leftover.len();
        leftover.resize(declared, 0);
        r.read_exact(&mut leftover[start..]).await?;
    }
    Ok(leftover)
}

/// Server side: read one `POST` request's body. `None` = the peer closed
/// the connection instead of sending another request.
pub async fn read_post<R>(r: &mut R) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let Some((head, leftover)) = read_head(r).await? else {
        return Ok(None);
    };
    if !head.first_line.starts_with("POST ") {
        return Err(Error::Http("only POST"));
    }
    let declared = head
        .content_length
        .ok_or(Error::Http("missing content-length"))?;
    Ok(Some(read_body(r, declared, leftover).await?))
}

/// Server side: write the `200` head that a frame sequence follows. No
/// `content-length` — the frames stream out as produced and
/// `connection: close` delimits the body.
pub async fn write_ok_head<W>(w: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
          connection: close\r\n\r\n",
    )
    .await?;
    Ok(())
}

/// Server side: write one `[len u32 LE][bytes]` frame and flush, so the peer
/// sees it before the next one is produced.
pub async fn write_frame<W>(w: &mut W, bytes: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(bytes.len())
        .map_err(|_| Error::Http("frame too large"))?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Server side: reject bytes that never became a WaveDB request. This is
/// the only non-200 the node sends — it means the *transport* broke.
pub async fn write_bad_request<W>(w: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_all(
        b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\
          connection: close\r\n\r\n",
    )
    .await?;
    w.flush().await?;
    Ok(())
}

/// Client side: POST `body` on a fresh connection and position the reader
/// at the response's frame sequence (after the `200` head).
///
/// # Errors
/// [`Error::Status`] on a non-200 answer; otherwise socket/framing faults.
pub async fn post(addr: &str, body: &[u8]) -> Result<FrameReader<TcpStream>> {
    let mut stream = TcpStream::connect(addr).await?;
    let head = format!(
        "POST / HTTP/1.1\r\nhost: {addr}\r\n\
         content-type: application/octet-stream\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    read_ok_head(stream).await
}

/// Client side: read a response head, requiring a `200`; the returned reader
/// yields the body's frames.
async fn read_ok_head<R>(mut r: R) -> Result<FrameReader<R>>
where
    R: AsyncRead + Unpin,
{
    let Some((head, leftover)) = read_head(&mut r).await? else {
        return Err(Error::Http("connection closed before response"));
    };
    let mut parts = head.first_line.split(' ');
    if parts.next() != Some("HTTP/1.1") {
        return Err(Error::Http("not http/1.1"));
    }
    let code: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or(Error::Http("bad status line"))?;
    if code != 200 {
        return Err(Error::Status(code));
    }
    Ok(FrameReader {
        reader: r,
        buf: leftover,
    })
}

/// An incremental reader over a response's `[len u32 LE][bytes]` frames —
/// each is available as soon as the node flushes it, which is what makes a
/// walk streamable without buffering the whole collection.
#[derive(Debug)]
pub struct FrameReader<R> {
    reader: R,
    /// Bytes already pulled off the socket past the previous boundary.
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Read until `self.buf` holds at least `n` bytes. `Ok(false)` = the
    /// peer closed first with the buffer empty (a clean end of stream).
    async fn fill(&mut self, n: usize) -> Result<bool> {
        let mut chunk = [0u8; 4096];
        while self.buf.len() < n {
            let read = self.reader.read(&mut chunk).await?;
            if read == 0 {
                if self.buf.is_empty() {
                    return Ok(false);
                }
                return Err(Error::Http("connection closed mid-frame"));
            }
            self.buf.extend_from_slice(&chunk[..read]);
        }
        Ok(true)
    }

    /// The next frame's bytes; `None` on a clean end of stream (the peer
    /// closed exactly on a frame boundary).
    ///
    /// # Errors
    /// A socket fault, a close mid-frame, or an oversized frame.
    pub async fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.fill(4).await? {
            return Ok(None);
        }
        // The length prefix was just filled to 4 bytes.
        let len = u32::from_le_bytes(self.buf[..4].try_into().expect("4 bytes"))
            as usize;
        if len > MAX_BODY {
            return Err(Error::BodyTooLarge {
                limit: MAX_BODY,
                have: len,
            });
        }
        if !self.fill(4 + len).await? {
            return Err(Error::Http("connection closed mid-frame"));
        }
        let mut frame = self.buf.split_off(4);
        self.buf.clear();
        if frame.len() > len {
            self.buf = frame.split_off(len);
        }
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::{
        Error, head_end, parse_head, read_ok_head, read_post, write_frame,
        write_ok_head,
    };

    #[test]
    fn parse_head_reads_first_line_and_length() {
        let head =
            parse_head(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\n")
                .unwrap();
        assert_eq!(head.first_line, "POST / HTTP/1.1");
        assert_eq!(head.content_length, Some(12));
    }

    #[test]
    fn parse_head_rejects_bad_length() {
        let err = parse_head(b"POST / HTTP/1.1\r\ncontent-length: x\r\n")
            .unwrap_err();
        assert!(matches!(err, Error::Http("bad content-length")));
    }

    #[test]
    fn head_end_finds_terminator() {
        assert_eq!(head_end(b"a\r\n\r\nbody"), Some(1));
        assert_eq!(head_end(b"a\r\nb"), None);
    }

    #[tokio::test]
    async fn post_and_frames_roundtrip_over_a_duplex_pipe() {
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Client writes a request by hand; the server side reads it.
        client
            .write_all(b"POST / HTTP/1.1\r\ncontent-length: 5\r\n\r\nhello")
            .await
            .unwrap();
        let body = read_post(&mut server).await.unwrap().unwrap();
        assert_eq!(body, b"hello");

        // Server answers with two frames, then closes; the client reads
        // them back one at a time, then sees the clean end.
        write_ok_head(&mut server).await.unwrap();
        write_frame(&mut server, b"one").await.unwrap();
        write_frame(&mut server, b"world").await.unwrap();
        drop(server);

        let mut frames = read_ok_head(&mut client).await.unwrap();
        assert_eq!(frames.next_frame().await.unwrap().unwrap(), b"one");
        assert_eq!(frames.next_frame().await.unwrap().unwrap(), b"world");
        assert!(frames.next_frame().await.unwrap().is_none(), "clean end");
    }

    #[tokio::test]
    async fn close_mid_frame_is_a_transport_fault() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_ok_head(&mut server).await.unwrap();
        // A length prefix promising 8 bytes, then only 3, then close.
        server.write_all(&8u32.to_le_bytes()).await.unwrap();
        server.write_all(b"abc").await.unwrap();
        drop(server);

        let mut frames = read_ok_head(&mut client).await.unwrap();
        let err = frames.next_frame().await.unwrap_err();
        assert!(matches!(err, Error::Http("connection closed mid-frame")));
    }

    #[tokio::test]
    async fn clean_close_reads_as_none() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        assert!(read_post(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn non_post_is_refused() {
        let (mut client, mut server) = tokio::io::duplex(256);
        client
            .write_all(b"GET / HTTP/1.1\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        let err = read_post(&mut server).await.unwrap_err();
        assert!(matches!(err, Error::Http("only POST")));
    }

    #[tokio::test]
    async fn oversized_body_is_capped_before_reading() {
        let (mut client, mut server) = tokio::io::duplex(256);
        let head = format!(
            "POST / HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            super::MAX_BODY + 1
        );
        client.write_all(head.as_bytes()).await.unwrap();
        let err = read_post(&mut server).await.unwrap_err();
        assert!(matches!(err, Error::BodyTooLarge { .. }));
    }

    #[tokio::test]
    async fn oversized_frame_is_capped_before_reading() {
        let (mut client, mut server) = tokio::io::duplex(256);
        write_ok_head(&mut server).await.unwrap();
        let too_big = u32::try_from(super::MAX_BODY + 1).unwrap();
        server.write_all(&too_big.to_le_bytes()).await.unwrap();

        let mut frames = read_ok_head(&mut client).await.unwrap();
        let err = frames.next_frame().await.unwrap_err();
        assert!(matches!(err, Error::BodyTooLarge { .. }));
    }

    #[tokio::test]
    async fn non_200_status_surfaces_as_status_error() {
        let (mut client, mut server) = tokio::io::duplex(256);
        client
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        let err = read_ok_head(&mut server).await.unwrap_err();
        assert!(matches!(err, Error::Status(404)));
    }
}
