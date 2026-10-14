//! 可自建的同步服务端实现（无剪贴板、无 TUI 依赖、无 async 运行时）。
//!
//! 定位：给不想用 WebDAV/S3 的人一个"一条命令就能跑"的自建方案。它做三件事：
//!
//! 1. **对象存储**：按客户端既有的 `v1/` 布局存对象（服务端只见密文）；
//! 2. **空间与设备令牌**：空间隔离 + 每设备一个令牌（磁盘上只存令牌哈希）；
//! 3. **配额与长轮询**：超配额返回 507；`GET /v1/items?since=&wait=` 让客户端近实时拿到新对象。
//!
//! 有意为之的边界：
//! - **不做端到端加密**：加密在客户端完成，服务端拿到的是密文（见 `docs/protocol.md` §5）；
//! - **不管剪贴板**：这台机器上可能根本没有图形会话，服务端只管存；
//! - **TLS 交给反向代理**（Caddy/Nginx 一行配置），自带 TLS 见 `docs/packaging.md`；
//! - **密封下发空间密钥（x25519 密封）尚未实现**，`GET /v1/spaces/{id}/key` 明确返回 501，
//!   当前请用口令 + `cx space join` 的路径。宁可在文档里说清楚，也不要给一个半成品的安全承诺。

pub mod error;
pub mod http;
pub mod logging;
pub mod store;

pub use error::ServerError;
pub use http::{DEFAULT_LONG_POLL_SECS, DEFAULT_MAX_CONNECTIONS, Server, ServerConfig};
pub use store::{
    DEFAULT_QUOTA_BYTES, DeviceRecord, ListQuery, MAX_OBJECT_BYTES, ObjectMeta, PutResult,
    SpaceMeta, Store,
};

/// 默认 HTTP 监听地址。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8688";

/// 服务端数据目录默认子路径（相对于平台数据目录）。
pub const DEFAULT_DATA_SUBDIR: &str = "cx-server";

/// 长轮询最大保持时长（秒）。
pub const LONG_POLL_MAX_SECS: u64 = DEFAULT_LONG_POLL_SECS;
