#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mcp_github::serve(std::env::args().skip(1).collect()).await
}
