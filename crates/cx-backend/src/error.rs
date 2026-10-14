//! 同步后端的错误类型。
//!
//! 分类原则：**调用方要能据此决定"重试还是放弃"**。
//! - [`BackendError::is_retryable`]：网络抖动、5xx、429 —— 退避后重试；
//! - [`BackendError::is_auth`]：401/403 —— 需要用户重新配置凭据，重试没意义。

/// 后端操作的失败。
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// 传输层失败（DNS、连接、TLS、超时）。
    #[error("传输失败：{0}")]
    Transport(String),

    /// 服务端返回了非 2xx 状态且不是"预期内的 404/412"。
    #[error("服务端返回 {status}：{detail}")]
    Status {
        /// HTTP 状态码。
        status: u16,
        /// 服务端给的信息（尽量精简）。
        detail: String,
    },

    /// 认证失败（401/403）。
    #[error("认证失败（{status}）：{detail}")]
    Unauthorized {
        /// HTTP 状态码。
        status: u16,
        /// 说明。
        detail: String,
    },

    /// 目标不存在（404：对象缺失、桶缺失、路径不对）。
    #[error("不存在：{0}")]
    NotFound(String),

    /// 与远端状态冲突（409：桶不存在、目录不存在、并发写冲突）。
    #[error("冲突：{0}")]
    Conflict(String),

    /// XML 解析失败（PROPFIND/LIST 响应）。
    #[error("XML 解析失败：{0}")]
    Xml(String),

    /// 响应结构与预期不符。
    #[error("响应结构非法：{0}")]
    Malformed(String),

    /// 后端配置有问题（缺字段、URL 非法）。
    #[error("后端配置错误：{0}")]
    Config(String),

    /// 凭据读取失败。
    #[error("凭据读取失败：{0}")]
    Credentials(String),

    /// 本地 IO。
    #[error("IO 错误：{0}")]
    Io(String),
}

impl BackendError {
    /// 是否值得退避后重试。
    pub fn is_retryable(&self) -> bool {
        match self {
            BackendError::Transport(_) => true,
            BackendError::Status { status, .. } => *status == 429 || *status >= 500,
            BackendError::Io(_) => true,
            _ => false,
        }
    }

    /// 是否属于认证/权限问题（需要用户介入）。
    pub fn is_auth(&self) -> bool {
        matches!(self, BackendError::Unauthorized { .. })
    }

    /// 便于日志排查的简短分类名。
    pub fn kind(&self) -> &'static str {
        match self {
            BackendError::Transport(_) => "transport",
            BackendError::Status { .. } => "status",
            BackendError::Unauthorized { .. } => "auth",
            BackendError::NotFound(_) => "not_found",
            BackendError::Conflict(_) => "conflict",
            BackendError::Xml(_) => "xml",
            BackendError::Malformed(_) => "malformed",
            BackendError::Config(_) => "config",
            BackendError::Credentials(_) => "credentials",
            BackendError::Io(_) => "io",
        }
    }
}
