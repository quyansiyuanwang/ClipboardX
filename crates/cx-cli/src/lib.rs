//! 命令行层。
//!
//! 子命令定义与控制套接字客户端。CLI 自身不读写剪贴板、不直接访问网络，
//! 一切操作都通过守护进程的控制套接字完成，这样 `cx` 才能同时作为
//! 运维工具与 TUI 的后端接口。

use clap::Subcommand;


#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// 打印版本、平台与协议版本信息
    Version {
        /// 以 JSON 输出，便于脚本消费
        #[arg(long)]
        json: bool,
    },
}
