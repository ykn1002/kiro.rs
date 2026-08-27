//! kiro-rs 二进制入口。
//!
//! 仅负责解析 CLI 参数、初始化日志，随后把启动工作交给 [`kiro_rs::app`]。
//! 服务器模式（含 Docker）沿用相对路径与 `config.host:port` 绑定，行为与重构前一致。

use clap::Parser;

use kiro_rs::app::{self, RunOptions, RuntimeMode};
use kiro_rs::model::arg::Args;

#[tokio::main]
async fn main() {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let opts = RunOptions {
        mode: RuntimeMode::Server,
        config_path: args.config,
        credentials_path: args.credentials,
    };

    if let Err(e) = app::run(opts).await {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}
