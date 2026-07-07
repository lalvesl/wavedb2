//! The todo-app node: a generic quick-node made into *this* backend by
//! attaching the schema crate's `expose_server!` output. The registry is both
//! the dispatch surface (six functions) and the storage surface (the `store`
//! entries' engine slots).

use todo_app_schema::REGISTRY;
use wavedb_quick_node::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Server::new(REGISTRY)
        .data_dir("./data")
        .serve("127.0.0.1:7700")
        .await?;
    Ok(())
}
