//! The node-side gate + dispatch: turn one decoded [`Request`] into a
//! [`Response`].
//!
//! The gates run **before** the engine, in the order
//! [`wavedb-quick-node` README] documents:
//!
//! 1. **Identity** — [`Auth::Token`] verifies against the node secret
//!    (HMAC, expiry, `Access` purpose); `user`/`tenant` come from the
//!    claims, never from a client field. [`Auth::Anonymous`] becomes the
//!    unauthenticated tier (`user = U48::MAX`) under the claimed tenant —
//!    only `#[server(public)]` functions accept it (the generated guard
//!    refuses everything else, and struct commands refuse it here).
//! 2. **Header** — the `struct_hash` must be listed in the registry
//!    ([`Exposure::knows`]); unlisted / excluded / unknown all refuse
//!    uniformly as [`UnknownStructHash`](wavedb_core::Error::UnknownStructHash).
//! 3. **Decode + engine** — [`Exposure::execute`] decodes the payload for the
//!    command (a `Get`/`Remove` payload is an `Id`, a `Save`/`Insert`/`Update`
//!    payload a body) and drives the storage engine.
//!
//! Gates 5–6 (`validate` / `preprocess`) are still a later seam; gate 4's
//! record-level `Metadata.permission` grants ride with the cross-tenant
//! read path (deferred) — tenant isolation is enforced by the token
//! binding itself.
//!
//! [`wavedb-quick-node` README]: https://docs.rs/wavedb-quick-node

use wavedb_core::expose::{Caller, Exposure};
use wavedb_core::{Error, Store};
use wavedb_net::auth::{self, TokenPurpose};
use wavedb_net::frame::{Auth, NodeError, NodeErrorKind, Request, Response};

/// The uniform identity refusal — which check failed stays server-side.
fn unauthorized(struct_hash: u64) -> Response {
    Response::Err(NodeError {
        kind: NodeErrorKind::Unauthorized,
        struct_hash,
        message: "unauthorized".into(),
    })
}

/// Gate 1: resolve the request's identity claim into the [`Caller`] the
/// engine executes as. `Err(())` = refuse (bad/expired/foreign token, or a
/// token before the node has a secret).
fn identify(auth: &Auth, secret: &[u8; 32]) -> Result<Caller, ()> {
    match auth {
        Auth::Anonymous { tenant } => Ok(Caller::anonymous(*tenant)),
        Auth::Token(token) => {
            let claims = auth::verify(
                secret,
                token,
                auth::unix_now(),
                TokenPurpose::Access,
            )
            .map_err(|_| ())?;
            Ok(Caller {
                user: claims.user,
                tenant: claims.tenant,
            })
        }
    }
}

/// Run the gates and the engine for one request, producing the wire response.
///
/// Never returns a transport error — a refusal or engine fault is the
/// [`Response::Err`] arm, so the caller always has bytes to send back.
pub async fn handle<E, S>(
    registry: &E,
    store: &S,
    secret: &[u8; 32],
    request: Request,
) -> Response
where
    E: Exposure,
    S: Store,
{
    let Request { auth, frame } = request;

    // Gate 1 — identity.
    let Ok(caller) = identify(&auth, secret) else {
        return unauthorized(frame.struct_hash);
    };

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
            caller,
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

    use wavedb_core::expose::{Caller, Command, Exposure, Reply};
    use wavedb_core::{Error, Id, Result, Store, U48, Write};
    use wavedb_net::auth::{AccessClaims, TokenPurpose, sign, unix_now};
    use wavedb_net::frame::{
        Auth, CommandFrame, NodeErrorKind, Request, Response,
    };

    use super::handle;

    const SECRET: [u8; 32] = [9; 32];

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
            caller: Caller,
            struct_hash: u64,
            _: Command,
            _: &[u8],
        ) -> Result<Reply> {
            if struct_hash == 0x1234 {
                // Echo the resolved user so tests can see gate 1's output.
                Ok(Reply::Value(Some(vec![
                    u8::try_from(caller.user.get() & 0xFF).unwrap(),
                ])))
            } else {
                Err(Error::UnknownStructHash(struct_hash))
            }
        }
    }

    fn request(auth: Auth, struct_hash: u64) -> Request {
        Request {
            auth,
            frame: CommandFrame {
                struct_hash,
                command: Command::Get,
                payload: Vec::new(),
            },
        }
    }

    fn token(user: u32, expires_at: u64, purpose: TokenPurpose) -> Vec<u8> {
        sign(
            &SECRET,
            &AccessClaims {
                user: U48::from(user),
                tenant: U48::from(user),
                expires_at,
                purpose,
                session: 0,
                nonce: 0,
            },
        )
    }

    fn anon() -> Auth {
        Auth::Anonymous {
            tenant: U48::from(1u32),
        }
    }

    #[tokio::test]
    async fn verified_token_reaches_the_engine_as_its_user() {
        let store = MemStore::default();
        let auth =
            Auth::Token(token(42, unix_now() + 60, TokenPurpose::Access));
        let resp =
            handle(&OneHash, &store, &SECRET, request(auth, 0x1234)).await;
        assert_eq!(resp, Response::Ok(Reply::Value(Some(vec![42]))));
    }

    #[tokio::test]
    async fn anonymous_reaches_the_engine_as_the_max_user() {
        // The tier itself passes gate 1; refusing non-public work is the
        // engine arms' guard (proven in the macro/e2e layers).
        let store = MemStore::default();
        let resp =
            handle(&OneHash, &store, &SECRET, request(anon(), 0x1234)).await;
        assert_eq!(resp, Response::Ok(Reply::Value(Some(vec![0xFF]))));
    }

    #[tokio::test]
    async fn expired_token_is_unauthorized() {
        let store = MemStore::default();
        let auth = Auth::Token(token(42, unix_now() - 1, TokenPurpose::Access));
        let Response::Err(e) =
            handle(&OneHash, &store, &SECRET, request(auth, 0x1234)).await
        else {
            panic!("must refuse");
        };
        assert_eq!(e.kind, NodeErrorKind::Unauthorized);
    }

    #[tokio::test]
    async fn refresh_token_is_not_an_access_token() {
        let store = MemStore::default();
        let auth =
            Auth::Token(token(42, unix_now() + 60, TokenPurpose::Refresh));
        let Response::Err(e) =
            handle(&OneHash, &store, &SECRET, request(auth, 0x1234)).await
        else {
            panic!("must refuse");
        };
        assert_eq!(e.kind, NodeErrorKind::Unauthorized);
    }

    #[tokio::test]
    async fn foreign_secret_token_is_unauthorized() {
        let store = MemStore::default();
        let forged = sign(
            &[1; 32],
            &AccessClaims {
                user: U48::from(42u32),
                tenant: U48::from(42u32),
                expires_at: unix_now() + 60,
                purpose: TokenPurpose::Access,
                session: 0,
                nonce: 0,
            },
        );
        let Response::Err(e) = handle(
            &OneHash,
            &store,
            &SECRET,
            request(Auth::Token(forged), 0x1234),
        )
        .await
        else {
            panic!("must refuse");
        };
        assert_eq!(e.kind, NodeErrorKind::Unauthorized);
    }

    #[tokio::test]
    async fn unknown_hash_refused_at_the_header_gate() {
        let store = MemStore::default();
        let Response::Err(e) =
            handle(&OneHash, &store, &SECRET, request(anon(), 0x9999)).await
        else {
            panic!("must refuse");
        };
        assert_eq!(e.kind, NodeErrorKind::UnknownStructHash);
        assert_eq!(e.struct_hash, 0x9999);
    }
}
