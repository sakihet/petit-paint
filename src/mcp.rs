use anyhow::Result;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt};

/// Draws images for MCP clients such as coding agents, without a window.
pub struct PaintServer;

impl ServerHandler for PaintServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("petit-paint", env!("CARGO_PKG_VERSION")),
        )
    }
}

/// Serves MCP over stdin and stdout until the client disconnects.
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = PaintServer.serve(rmcp::transport::stdio()).await?;
        server.waiting().await?;
        Ok(())
    })
}
