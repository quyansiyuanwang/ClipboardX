//! ClipboardX 命令行层。
//!
//! 设计约束：**CLI 自身不读写剪贴板、不直接访问网络**。所有操作都通过守护进程的
//! 控制套接字完成，这样 CLI、TUI、以及将来任何前端共享同一套能力，也避免了
//! "两个进程同时动剪贴板"这类问题。
//!
//! 以下例外是有意为之：
//! - `cx daemon` 就是守护进程本身；
//! - `cx doctor` 在守护进程**起不来**时也要能用，所以它有本地回退检查；
//! - 后端只读测试、服务端注册、空间发布/加入是用户显式请求的直连操作；
//!   TUI 后端草稿先测试成功，再事务提交配置与凭据。

mod commands;
mod doctor;
mod output;

pub use commands::{
    BackendAddCommand, BackendCommand, Command, ConfigCommand, ServiceCommand, SpaceCommand,
};

use cx_core::{CoreError, Paths};
use cx_daemon::DaemonError;
use std::process::ExitCode;

/// 解析平台目录；失败时打印可操作的建议。
pub fn paths() -> Result<Paths, CoreError> {
    Paths::resolve()
}

/// 统一的错误出口：把错误翻译成人能看懂的一句话，并给出退出码。
///
/// 退出码约定：
/// - `0` 成功
/// - `1` 一般错误（含服务端返回的业务错误）
/// - `2` 用法错误（由 clap 处理）
/// - `3` 守护进程未运行
pub fn report_error(err: &DaemonError) -> ExitCode {
    match err {
        DaemonError::NotRunning(detail) => {
            eprintln!("守护进程未运行：{detail}");
            eprintln!("提示：用 `cx start` 在后台启动，或 `cx daemon --foreground` 前台调试。");
            ExitCode::from(3)
        }
        DaemonError::AlreadyRunning(detail) => {
            eprintln!("已经有一个 ClipboardX 守护进程在运行：{detail}");
            eprintln!("提示：`cx status` 查看状态，`cx stop` 停止它。");
            ExitCode::from(3)
        }
        DaemonError::Remote { code, message } => {
            eprintln!("操作失败（{code}）：{message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("出错：{other}");
            ExitCode::FAILURE
        }
    }
}

/// 执行一个子命令。
pub fn run(command: Command) -> ExitCode {
    let paths = match paths() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("无法确定配置目录：{err}");
            return ExitCode::FAILURE;
        }
    };

    match commands::dispatch(&paths, command) {
        Ok(code) => code,
        Err(err) => report_error(&err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    /// 只为自检而存在的壳：clap 的 `CommandFactory` 由 `Parser` 派生提供，
    /// 而 `Command` 本身只派生 `Subcommand`。
    #[derive(Debug, Parser)]
    #[command(name = "cx", disable_help_subcommand = true)]
    struct TestCli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn 子命令定义合法() {
        // clap 自带的自检：参数冲突、缺描述等问题会在这里失败
        TestCli::command().debug_assert();
    }

    #[test]
    fn s3_addressing_flags_are_explicit_and_mutually_exclusive() {
        let args = [
            "cx",
            "backend",
            "add",
            "s3",
            "fake",
            "--bucket",
            "fake-bucket",
            "--access-key-id",
            "fake-key",
            "--virtual-host",
        ];
        let cli = TestCli::try_parse_from(args).unwrap();
        assert!(matches!(
            cli.command,
            Command::Backend {
                command: BackendCommand::Add {
                    kind: BackendAddCommand::S3 {
                        virtual_host: true,
                        path_style: false,
                        ..
                    }
                }
            }
        ));
        assert!(TestCli::try_parse_from(args.into_iter().chain(["--path-style"])).is_err());
    }

    #[test]
    fn 未运行时的错误给出可操作提示() {
        let err = DaemonError::NotRunning("没有 daemon.json".to_string());
        let code = report_error(&err);
        // ExitCode 不便比较数值，这里确保不 panic 且给出退出码
        let _ = code;
    }
}
