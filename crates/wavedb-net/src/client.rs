//! The client-side transport: build a [`Request`], POST it, read the framed
//! response.
//!
//! Compiles for native and wasm32 alike — the POST and the body stream come
//! from `wavedb_platform::http` (a fresh `TcpStream` vs a browser `fetch`),
//! and everything above that seam is byte-for-byte the same protocol.
//!
//! One exchange = one fresh connection (HTTP POST, the only wired transport;
//! WebSocket with a bound identity is M7). Each call re-sends the identity —
//! plain HTTP has no connection to bind identity to. A response is a
//! sequence of [`StreamFrame`]s: a scalar command answers with a bare
//! [`End`](StreamFrame::End); a walk streams `Item`s as the node produces
//! them, so [`call_stream`](NetClient::call_stream) yields records without
//! waiting for the whole collection.

use futures::Stream;
use wavedb_core::expose::{Command, Reply};
use wavedb_wire::{from_wire, to_wire};

use crate::error::{Error, Result};
use crate::frame::{
    Auth, CommandFrame, NodeError, Request, Response, StreamFrame,
};
use crate::frames::FrameReader;

/// A thin client bound to one node address. Cheap to clone/rebuild — it holds
/// no connection (each call dials fresh).
#[derive(Debug, Clone)]
pub struct NetClient {
    addr: String,
}

/// The outcome of a transported command: either the node's [`Reply`], or the
/// structured [`NodeError`] it shipped inside a 200. A transport-level
/// failure (socket, framing) is the outer [`Error`] instead.
pub type Executed = core::result::Result<Reply, NodeError>;

impl NetClient {
    /// Bind to a node at `host:port` (e.g. `"127.0.0.1:7700"`).
    #[must_use]
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    /// POST the request and return the response's frame reader.
    async fn exchange(
        &self,
        auth: Auth,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<FrameReader> {
        let request = Request {
            auth,
            frame: CommandFrame {
                struct_hash,
                command,
                payload,
            },
        };
        let body =
            wavedb_platform::http::post(&self.addr, &to_wire(&request)).await?;
        Ok(FrameReader::new(body))
    }

    /// Send one scalar command under `auth` and await the node's answer.
    ///
    /// # Errors
    /// A transport [`Error`] (socket, HTTP framing, an undecodable frame, or
    /// an `Item` where a scalar answer was due). A node-side refusal is
    /// **not** an error — it is the `Err` arm of the returned [`Executed`].
    pub async fn call(
        &self,
        auth: Auth,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<Executed> {
        let mut frames =
            self.exchange(auth, struct_hash, command, payload).await?;
        let bytes = frames
            .next_frame()
            .await?
            .ok_or(Error::Http("response ended before its End frame"))?;
        match from_wire::<StreamFrame>(&bytes)? {
            StreamFrame::End(response) => Ok(response.into_result()),
            StreamFrame::Item(_) => {
                Err(Error::Http("item frame on a scalar command"))
            }
        }
    }

    /// [`call`](Self::call), flattening a node refusal into the transport
    /// [`Error`] so a caller that treats every failure alike gets one type.
    ///
    /// # Errors
    /// [`Error::Node`] on a node-side refusal, else a transport fault.
    pub async fn call_ok(
        &self,
        auth: Auth,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<Reply> {
        self.call(auth, struct_hash, command, payload)
            .await?
            .map_err(Error::Node)
    }

    /// Send a walk-shaped command and stream the item frames back as the
    /// node writes them. Each item is one record's wire bytes; the stream
    /// ends at the node's `End` frame — a node-side fault mid-walk surfaces
    /// as a trailing [`Error::Node`] item after the records already shipped.
    ///
    /// # Errors
    /// A transport fault establishing the exchange; later faults ride the
    /// stream.
    pub async fn call_stream(
        &self,
        auth: Auth,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<impl Stream<Item = Result<Vec<u8>>> + use<>> {
        let frames = self.exchange(auth, struct_hash, command, payload).await?;
        Ok(futures::stream::unfold(Some(frames), |state| async move {
            let mut frames = state?;
            let item = match frames.next_frame().await {
                Ok(Some(bytes)) => match from_wire::<StreamFrame>(&bytes) {
                    Ok(StreamFrame::Item(item)) => {
                        return Some((Ok(item), Some(frames)));
                    }
                    Ok(StreamFrame::End(Response::Ok(_))) => return None,
                    Ok(StreamFrame::End(Response::Err(e))) => {
                        Err(Error::Node(e))
                    }
                    Err(e) => Err(e.into()),
                },
                Ok(None) => {
                    Err(Error::Http("response ended before its End frame"))
                }
                Err(e) => Err(e),
            };
            // Terminal: yield the fault once, then end the stream.
            Some((item, None))
        }))
    }
}
