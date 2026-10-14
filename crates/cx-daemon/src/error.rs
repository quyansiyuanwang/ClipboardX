//! 守护进程错误类型。

/// 守护进程运行时的失败。
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// 核心层错误。
    #[error(transparent)]
    Core(#[from] cx_core::CoreError),

    /// 加密层错误（生成控制令牌等场景）。
    #[error(transparent)]
    Crypto(#[from] cx_crypto::CryptoError),

    /// 已有实例在运行（单实例锁被占用）。
    #[error("已有 ClipboardX 守护进程在运行（{0}）")]
    AlreadyRunning(String),

    /// 守护进程未运行（控制套接字不可达）。
    #[error("守护进程未运行：{0}")]
    NotRunning(String),

    /// 控制协议层面的问题（握手失败、版本不匹配、响应无法解析）。
    #[error("控制协议错误：{0}")]
    Protocol(String),

    /// 服务端返回了错误。
    #[error("{code}：{message}")]
    Remote {
        /// 错误码。
        code: String,
        /// 说明。
        message: String,
    },

    /// 网络/IO。
    #[error("IO 错误：{0}")]
    Io(String),

    /// 后台挂载失败。
    #[error("无法在后台启动：{0}")]
    Detach(String),

    /// 自启注册失败。
    #[error("自启注册失败：{0}")]
    Service(String),
}

impl DaemonError {
    /// 构造 IO 错误。
    pub fn io(err: impl std::fmt::Display) -> Self {
        DaemonError::Io(err.to_string())
    }

    /// 构造协议错误。
    pub fn protocol(err: impl std::fmt::Display) -> Self {
        DaemonError::Protocol(err.to_string())
    }
}
