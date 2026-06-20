use futures::StreamExt;
use todo_app_schema::*;
use wavedb::prelude::*;

const SERVER: &str = "ws://127.0.0.1:7700";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Derive tenant from username: B2C pattern — tenant == user.
    let tenant = tenant_for("alice");
    let db = Db::connect(SERVER, tenant, tenant).await?;

    // ── Auth ─────────────────────────────────────────────────────────────────

    register(&db, "alice".into(), "secret".into()).await?;
    println!("registered");

    let token = login(&db, "secret".into()).await?;
    println!("logged in  token={token}");

    // ── Write todos ──────────────────────────────────────────────────────────

    let id_milk  = add_todo(&db, "Buy milk".into()).await?;
    let id_docs  = add_todo(&db, "Write docs".into()).await?;
    let _id_rust = add_todo(&db, "Read the Rust book".into()).await?;
    println!("added 3 todos");

    // ── Read todos ───────────────────────────────────────────────────────────

    println!("\n── all todos ──");
    let mut stream = all_todos(&db);
    while let Some(item) = stream.next().await {
        let todo = item?;
        let mark = if todo.completed { "x" } else { " " };
        println!("  [{mark}] {}", todo.title);
    }

    // ── Complete + delete ────────────────────────────────────────────────────

    complete_todo(&db, id_milk).await?;
    println!("\ncompleted 'Buy milk'");

    delete_todo(&db, id_docs).await?;
    println!("deleted 'Write docs'");

    println!("\n── all todos after mutations ──");
    let mut stream = all_todos(&db);
    while let Some(item) = stream.next().await {
        let todo = item?;
        let mark = if todo.completed { "x" } else { " " };
        println!("  [{mark}] {}", todo.title);
    }

    Ok(())
}

/// Derive a 48-bit tenant id from a username.
/// B2C pattern: tenant == user, computed client-side without a round-trip.
fn tenant_for(username: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    username.hash(&mut h);
    h.finish() & 0x0000_FFFF_FFFF_FFFF // mask to 48 bits (TENANT field width)
}
