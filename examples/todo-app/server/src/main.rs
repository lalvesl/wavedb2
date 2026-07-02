use todo_app_schema::REGISTRY;
use wavedb_quick_node::QuickNode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    QuickNode::builder()
        .bind("0.0.0.0:7700")
        .data_dir("./data")
        // The dispatch surface emitted by the schema crate's `expose_server!`
        // declaration — attaching it turns the generic node into this backend.
        .registry(REGISTRY)
        .serve()
        .await?;
    Ok(())
}
