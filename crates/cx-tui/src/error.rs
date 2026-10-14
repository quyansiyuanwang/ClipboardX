//! TUI 的错误类型。

/// 终端界面可能遇到的失败。
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// 与守护进程通信失败。
    #[error("守护进程通信失败：{0}")]
    Daemon(#[from] cx_daemon::DaemonError),

    /// 终端读写失败。
    #[error("{0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误信息可读() {
        let err = TuiError::Io("无法进入 raw 模式：测试".to_string());
        assert!(err.to_string().contains("raw 模式"));
    }
}
