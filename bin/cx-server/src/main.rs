//! `cx-server` —— 可自建的 ClipboardX 同步服务端。
//!
//! 与客户端共用 `v1/` 对象布局，只保存密文；不依赖剪贴板与终端界面，
//! 可部署在无图形会话的服务器/容器中。

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "cx-server",
    version,
    about = "ClipboardX 同步服务端（可自建）",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 打印版本与服务端默认参数
    Version {
        /// 以 JSON 输出，便于脚本消费
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Version { json } => print_version(json),
    }
}

fn print_version(json: bool) -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    if json {
        let payload = serde_json::json!({
            "name": "cx-server",
            "version": version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "store_layout": cx_core::STORE_LAYOUT_VERSION,
            "default_listen": cx_server::DEFAULT_LISTEN,
            "long_poll_max_secs": cx_server::LONG_POLL_MAX_SECS,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("cx-server: 无法序列化版本信息: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("cx-server {version}");
        println!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("store layout: v{}", cx_core::STORE_LAYOUT_VERSION);
        println!("default listen: {}", cx_server::DEFAULT_LISTEN);
    }
    ExitCode::SUCCESS
}
