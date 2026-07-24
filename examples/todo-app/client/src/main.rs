//! The todo-app client: the full M4 flow over the wire — register + login on
//! the system tenant, reconnect as the assigned tenant, then drive the todo
//! functions. Every call here is a `#[server]` stub; no struct is
//! wire-addressable.

use todo_app_schema::{
    add_todo, all_todos, complete_todo, delete_todo, login, register,
};
use wavedb::prelude::*;

const SERVER: &str = "127.0.0.1:7700";
const SYSTEM_TENANT: U48 = U48::ZERO;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Step 1: connect as the system tenant to call the auth functions ───
    let sys = Db::connect(SERVER, SYSTEM_TENANT, SYSTEM_TENANT).await?;

    let tenant_id = register(&sys, "alice".into(), "secret".into()).await?;
    println!("registered  tenant_id={tenant_id}");

    let (tenant_id, pair) =
        login(&sys, "alice".into(), "secret".into()).await?;
    println!("logged in   tenant_id={tenant_id}");

    // ── Step 2: reconnect as the real user tenant ──────────────────────────
    let tenant = U48::try_from(tenant_id)?;
    let db = Db::connect(SERVER, tenant, tenant)
        .await?
        .with_access_token(pair.access.clone());

    // ── Write ──────────────────────────────────────────────────────────────
    let id_milk = add_todo(&db, "Buy milk".into()).await?;
    let id_docs = add_todo(&db, "Write docs".into()).await?;
    let _id_rust = add_todo(&db, "Read the Rust book".into()).await?;
    println!("added 3 todos");

    // ── Read ───────────────────────────────────────────────────────────────
    println!("\n── todos ──");
    print_todos(&db).await?;

    // ── Mutate ─────────────────────────────────────────────────────────────
    complete_todo(&db, id_milk).await?;
    delete_todo(&db, id_docs).await?;

    println!("\n── todos after complete + delete ──");
    print_todos(&db).await?;

    Ok(())
}

async fn print_todos(db: &Db) -> anyhow::Result<()> {
    // The walk is an async iterator — each todo arrives as its own frame.
    let mut todos = std::pin::pin!(all_todos(db));
    while let Some(todo) = todos.next().await {
        let todo = todo?;
        let mark = if todo.completed { "x" } else { " " };
        println!("  [{mark}] {}", todo.title);
    }
    Ok(())
}
