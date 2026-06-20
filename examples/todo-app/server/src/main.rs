use todo_app_schema::REGISTRY;
use wavedb_quick_node::QuickNode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    QuickNode::builder()
        .bind("0.0.0.0:7700")
        .data_dir("./data")
        .registry(REGISTRY)
        .serve()
        .await?;
    Ok(())
}
