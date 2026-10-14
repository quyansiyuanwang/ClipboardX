//! 局域网模块的错误类型。

/// 局域网直连的失败。
#[derive(Debug, thiserror::Error)]
pub enum LanError {
    /// 配置有问题（PSK 格式、端口等）。
    #[error("局域网配置错误：{0}")]
    Config(String),

    /// 网络 IO。
    #[error("网络错误：{0}")]
    Io(String),

    /// Noise 握手或加解密失败。
    #[error("安全层错误：{0}")]
    Noise(String),

    /// 对端行为不符合协议。
    #[error("协议错误：{0}")]
    Protocol(String),

    /// 没有新事件（用于非阻塞轮询）。
    #[error("暂无事件")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误信息可读() {
        assert!(
            LanError::Config("PSK 长度不对".to_string())
                .to_string()
                .contains("PSK")
        );
        assert!(LanError::Timeout.to_string().contains("暂无"));
    }
}
