//! 同步后端。
//!
//! 所有后端实现同一个 [`RemoteBackend`] 契约，共用 `v1/` 对象布局：
//! `cx-server`（REST）、WebDAV、S3，以及测试用的内存实现。
//!
//! [`RemoteBackend`]: https://example.invalid

/// 后端种类标识，用于配置与 `cx status` 展示。
pub const KIND_SERVER: &str = "server";
/// WebDAV 后端种类标识。
pub const KIND_WEBDAV: &str = "webdav";
/// S3 后端种类标识。
pub const KIND_S3: &str = "s3";
/// 内存后端种类标识（仅供测试）。
pub const KIND_MEMORY: &str = "memory";
