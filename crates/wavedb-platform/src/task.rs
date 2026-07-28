//! Background tasks — the seam the client's connection manager runs on.
//!
//! Two primitives, cfg-switched like every platform seam:
//!
//! - [`spawn_detached`] boots a **never-ending** task: natively a dedicated
//!   thread carrying its own current-thread runtime + `LocalSet` (the
//!   manager must outlive any particular caller runtime); in the browser
//!   `wasm_bindgen_futures::spawn_local` — no tokio in wasm.
//! - [`spawn_local`] spawns a subtask **inside** a detached task's world
//!   (per-request work, per-connection actors). Single-threaded on both
//!   targets, so subtask futures may hold `Rc`/`RefCell` state.
//!
//! The factory shape on [`spawn_detached`] exists for the native half: the
//! future is built *on* the new thread, so it may own non-`Send` state
//! (channels into it are the only thing that crosses threads).

use std::future::Future;

use crate::error::Result;

/// Boot a detached, long-lived task; returns once it is scheduled. The
/// task ends only when its future completes — a connection manager's never
/// does.
///
/// # Errors
/// Natively, the carrier thread failing to spawn ([`Error::Io`]); the
/// browser spawn cannot fail.
///
/// [`Error::Io`]: crate::Error::Io
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_detached<F, Fut>(name: &str, factory: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new().name(name.into()).spawn(move || {
        // A runtime that cannot build leaves the factory unconsumed: its
        // channels drop, so every handle into this task errors instead of
        // hanging — the failure is loud at the callers, not here.
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let local = tokio::task::LocalSet::new();
        rt.block_on(local.run_until(async move { factory().await }));
    })?;
    Ok(())
}

/// Boot a detached, long-lived task; returns once it is scheduled.
///
/// # Errors
/// None in the browser — fallible for parity with the native half.
#[cfg(target_arch = "wasm32")]
pub fn spawn_detached<F, Fut>(name: &str, factory: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let _ = name;
    wasm_bindgen_futures::spawn_local(factory());
    Ok(())
}

/// Spawn a subtask on the current single-threaded task world.
///
/// Natively this is `tokio::task::spawn_local`, whose **precondition** is
/// running inside a `LocalSet` — i.e. inside a [`spawn_detached`] task (or
/// a caller-owned `LocalSet`, like a node's serve loop). Calling it from
/// anywhere else is a caller bug and panics in tokio.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_local<Fut: Future<Output = ()> + 'static>(future: Fut) {
    drop(tokio::task::spawn_local(future));
}

/// Spawn a subtask on the current single-threaded task world.
#[cfg(target_arch = "wasm32")]
pub fn spawn_local<Fut: Future<Output = ()> + 'static>(future: Fut) {
    wasm_bindgen_futures::spawn_local(future);
}
