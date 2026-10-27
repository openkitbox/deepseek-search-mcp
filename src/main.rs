pub mod config;
pub mod deepseek;
pub mod error;
pub mod mcp;
pub mod tools;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    match config::Config::from_env() {
        Ok(config) => match deepseek::DeepSeekClient::new(&config) {
            Ok(client) => {
                if let Err(error) = mcp::McpServer::new(client)
                    .run(tokio::io::stdin(), tokio::io::stdout())
                    .await
                {
                    tracing::error!(category = ?error.category(), "server stopped: {error}");
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                tracing::error!(category = ?error.category(), "server startup failed: {}", error.model_message());
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            tracing::error!(category = ?error.category(), "server startup failed: {}", error.model_message());
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish();

    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("failed to initialize stderr logging");
    }
}
