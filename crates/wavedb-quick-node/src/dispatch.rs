//! The node-side gate + dispatch: turn one decoded [`Request`] into a
//! [`Response`].
//!
//! The gates run **before** the engine, in the order
//! [`wavedb-quick-node` README] documents:
//!
//! 1. **Identity** — `tenant` from the request. Until M8 this is the claimed
//!    tenant (the session binding); M8 replaces it with the verified HMAC
//!    access token carried in the same envelope.
//! 2. **Header** — the `struct_hash` must be listed in the registry
//!    ([`Exposure::knows`]); unlisted / excluded / unknown all refuse
//!    uniformly as [`UnknownStructHash`](wavedb_core::Error::UnknownStructHash).
//! 3. **Decode + engine** — [`Exposure::execute`] decodes the payload for the
//!    command (a `Get`/`Remove` payload is an `Id`, a `Save`/`Insert`/`Update`
//!    payload a body) and drives the storage engine.
//!
//! Gates 4–6 (permission / `validate` / `preprocess`) arrive with auth (M8)
//! and the hook wiring; the seam is here.
//!
//! [`wavedb-quick-node` README]: https://docs.rs/wavedb-quick-node

use wavedb_core::expose::Exposure;
use wavedb_core::{Error, Store};
use wavedb_net::frame::{NodeError, Request, Response};

/// Run the gates and the engine for one request, producing the wire response.
///
/// Never returns a transport error — a refusal or engine fault is the
/// [`Response::Err`] arm, so the caller always has bytes to send back.
pub async fn handle<E, S>(registry: &E, store: &S, request: Request) -> Response
where
    E: Exposure,
    S: Store,
{
    let Request { tenant, frame } = request;

    // Gate 2 — header check. `execute` would also refuse an unlisted hash,
    // but the explicit gate short-circuits and keeps the refusal uniform.
    if !registry.knows(frame.struct_hash) {
        return Response::Err(NodeError::from_core(
            frame.struct_hash,
            &Error::UnknownStructHash(frame.struct_hash),
        ));
    }

    // Gate 3 — decode (inside the generated step) + engine dispatch.
    match registry
        .execute(
            store,
            tenant,
            frame.struct_hash,
            frame.command,
            &frame.payload,
        )
        .await
    {
        Ok(reply) => Response::Ok(reply),
        Err(err) => {
            Response::Err(NodeError::from_core(frame.struct_hash, &err))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use wavedb_core::expose::{Command, Exposure, Reply};
    use wavedb_core::{Error, Id, Result, Store, U48, Write};
    use wavedb_net::frame::{CommandFrame, NodeErrorKind, Request, Response};

    use super::handle;

    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<u128, Vec<u8>>>);

    impl Store for MemStore {
        async fn get(&self, id: Id) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
        }
        async fn apply(&self, batch: &[Write]) -> Result<()> {
            let mut m = self.0.lock().unwrap();
            for w in batch {
                match w {
                    Write::Put(id, b) => {
                        m.insert(id.raw(), b.clone());
                    }
                    Write::Remove(id) => {
                        m.remove(&id.raw());
                    }
                }
            }
            drop(m);
            Ok(())
        }
    }

    /// A registry that knows exactly one hash and echoes a fixed reply.
    #[derive(Clone, Copy)]
    struct OneHash;

    impl Exposure for OneHash {
        fn knows(&self, struct_hash: u64) -> bool {
            struct_hash == 0x1234
        }
        fn decode_check(&self, struct_hash: u64, _: &[u8]) -> Result<()> {
            if struct_hash == 0x1234 {
                Ok(())
            } else {
                Err(Error::UnknownStructHash(struct_hash))
            }
        }
        async fn execute<S: Store>(
            &self,
            _: &S,
            _: U48,
            struct_hash: u64,
            _: Command,
            _: &[u8],
        ) -> Result<Reply> {
            if struct_hash == 0x1234 {
                Ok(Reply::Value(Some(vec![42])))
            } else {
                Err(Error::UnknownStructHash(struct_hash))
            }
        }
    }

    fn request(struct_hash: u64) -> Request {
        Request {
            tenant: U48::from(1u32),
            frame: CommandFrame {
                struct_hash,
                command: Command::Get,
                payload: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn known_hash_reaches_the_engine() {
        let store = MemStore::default();
        let resp = handle(&OneHash, &store, request(0x1234)).await;
        assert_eq!(resp, Response::Ok(Reply::Value(Some(vec![42]))));
    }

    #[tokio::test]
    async fn unknown_hash_refused_at_the_header_gate() {
        let store = MemStore::default();
        let Response::Err(e) = handle(&OneHash, &store, request(0x9999)).await
        else {
            panic!("must refuse");
        };
        assert_eq!(e.kind, NodeErrorKind::UnknownStructHash);
        assert_eq!(e.struct_hash, 0x9999);
    }
}
