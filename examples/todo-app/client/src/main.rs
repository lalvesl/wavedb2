use futures::StreamExt;
use todo_app_schema::*;
use wavedb::prelude::*;

const SERVER: &str = "ws://127.0.0.1:7700";
const SYSTEM_TENANT: u64 = 0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Step 1: connect as system tenant to call auth functions ───────────────
    let sys = Db::connect(SERVER, SYSTEM_TENANT, SYSTEM_TENANT).await?;

    let tenant_id = register(&sys, "alice".into(), "secret".into()).await?;
    println!("registered  tenant_id={tenant_id}");

    let (tenant_id, token) = login(&sys, "alice".into(), "secret".into()).await?;
    println!("logged in   tenant_id={tenant_id}  token={token}");

    // ── Step 2: reconnect as the real user tenant ─────────────────────────────
    let db = Db::connect(SERVER, tenant_id, tenant_id).await?;

    // ── Write ─────────────────────────────────────────────────────────────────
    let id_milk = add_todo(&db, "Buy milk".into()).await?;
    let _id_docs = add_todo(&db, "Write docs".into()).await?;
    let _id_rust = add_todo(&db, "Read the Rust book".into()).await?;
    println!("added 3 todos");

    // ── Read ──────────────────────────────────────────────────────────────────
    println!("\n── todos ──");
    print_todos(&db).await?;

    // ── Mutate ────────────────────────────────────────────────────────────────
    complete_todo(&db, id_milk).await?;
    delete_todo(&db, _id_docs).await?;

    println!("\n── todos after complete + delete ──");
    print_todos(&db).await?;

    Ok(())
}

async fn print_todos(db: &Db) -> anyhow::Result<()> {
    let mut stream = all_todos(db);
    while let Some(item) = stream.next().await {
        let todo = item?;
        let mark = if todo.completed { "x" } else { " " };
        println!("  [{mark}] {}", todo.title);
    }
    Ok(())
}
