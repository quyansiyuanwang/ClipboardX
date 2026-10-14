//! `cx` —— ClipboardX 的统一入口：守护进程、运维 CLI 与 TUI。
//!
//! 这是一个薄壳：参数解析与全部实现都在 `cx-cli`，本文件只把退出码传出去。

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
    cx_cli::run(cli.command)
}
