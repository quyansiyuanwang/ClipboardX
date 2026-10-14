//! `cx` —— ClipboardX 的统一入口：守护进程、运维 CLI 与 TUI。
//!
//! 这是一个薄壳：参数解析放在 `cx-cli`，实际能力由 `cx-daemon` / `cx-tui` 提供。

use clap::Parser;
use cx_cli::Command;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "cx",
    version,
    about = "ClipboardX：极度轻量的三端剪贴板同步（守护进程 + CLI + TUI）",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
            "name": "cx",
            "version": version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "store_layout": cx_core::STORE_LAYOUT_VERSION,
            "control_protocol": cx_core::CONTROL_PROTOCOL_VERSION,
            "space_format": cx_core::SPACE_FORMAT_VERSION,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("cx: 无法序列化版本信息: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("cx {version}");
        println!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("store layout: v{}", cx_core::STORE_LAYOUT_VERSION);
        println!("control protocol: v{}", cx_core::CONTROL_PROTOCOL_VERSION);
        println!("space format: v{}", cx_core::SPACE_FORMAT_VERSION);
    }
    ExitCode::SUCCESS
}
