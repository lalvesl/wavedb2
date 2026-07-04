//! The client-side transport: build a [`Request`], POST it, decode the
//! [`Response`] (native only).
//!
//! One exchange = one fresh connection (HTTP POST, the only wired transport;
//! WebSocket with a bound identity is M7). Each call re-sends the tenant —
//! plain HTTP has no connection to bind identity to.

use wavedb_core::U48;
use wavedb_core::expose::{Command, Reply};
use wavedb_wire::{from_wire, to_wire};

use crate::error::{Error, Result};
use crate::frame::{CommandFrame, NodeError, Request, Response};
use crate::http;

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

    /// Send one command as `tenant` and await the node's answer.
    ///
    /// # Errors
    /// A transport [`Error`] (socket, HTTP framing, or an undecodable
    /// envelope). A node-side refusal is **not** an error — it is the `Err`
    /// arm of the returned [`Executed`].
    pub async fn call(
        &self,
        tenant: U48,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<Executed> {
        let request = Request {
            tenant,
            frame: CommandFrame {
                struct_hash,
                command,
                payload,
            },
        };
        let reply_bytes = http::post(&self.addr, &to_wire(&request)).await?;
        let response: Response = from_wire(&reply_bytes)?;
        Ok(response.into_result())
    }

    /// [`call`](Self::call), flattening a node refusal into the transport
    /// [`Error`] so a caller that treats every failure alike gets one type.
    ///
    /// # Errors
    /// [`Error::Node`] on a node-side refusal, else a transport fault.
    pub async fn call_ok(
        &self,
        tenant: U48,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) -> Result<Reply> {
        self.call(tenant, struct_hash, command, payload)
            .await?
            .map_err(Error::Node)
    }
}
