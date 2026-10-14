//! 可自建的同步服务端实现（无剪贴板、无 TUI 依赖）。
//!
//! 存储为文件系统对象库，落盘布局与客户端 `v1/` 一致；服务端只见密文。
//! HTTP 由 `tiny_http` 提供（阻塞、无 async），长轮询用于近实时通知。
//! TLS 默认由反向代理终止（Caddy/Nginx），自带 TLS 作为可选 feature。

/// 默认 HTTP 监听地址。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8688";

/// 服务端数据目录默认子路径（相对于平台数据目录）。
pub const DEFAULT_DATA_SUBDIR: &str = "cx-server";

/// 长轮询最大保持时长（秒）。
pub const LONG_POLL_MAX_SECS: u64 = 25;
