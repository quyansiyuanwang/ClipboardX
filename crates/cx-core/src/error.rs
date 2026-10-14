//! 核心领域层的错误类型。

use std::path::PathBuf;

/// 核心层的所有可能失败。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 文件系统操作失败。
    #[error("文件操作失败 {path}: {source}")]
    Io {
        /// 相关路径。
        path: PathBuf,
        /// 底层错误。
        #[source]
        source: std::io::Error,
    },

    /// 配置解析或校验失败。
    #[error("配置错误：{0}")]
    Config(String),

    /// 数据文件格式非法（identity/space/state/history 索引）。
    #[error("{file} 格式非法：{detail}")]
    Malformed {
        /// 文件名。
        file: &'static str,
        /// 具体原因。
        detail: String,
    },

    /// 空间密钥相关失败（口令错误、密钥缺失等）。
    #[error("空间密钥错误：{0}")]
    SpaceKey(String),

    /// 找不到必要的前置数据（例如尚未初始化空间）。
    #[error("尚未初始化：{0}")]
    NotInitialized(String),

    /// 历史存储的物理损坏（帧损坏且无法恢复到可用状态）。
    #[error("历史存储损坏：{0}")]
    HistoryCorrupt(String),

    /// 加解密失败。
    #[error(transparent)]
    Crypto(#[from] cx_crypto::CryptoError),

    /// 剪贴板层错误（以字符串透传，避免 `cx-core` 反向依赖具体剪贴板实现）。
    #[error("剪贴板错误：{0}")]
    Clip(String),

    /// 协议编解码失败。
    #[error(transparent)]
    Proto(#[from] cx_proto::ProtoError),

    /// 找不到平台目录（既无环境变量也没有 HOME）。
    #[error("无法确定 {0} 目录")]
    NoDirectory(&'static str),
}

impl CoreError {
    /// 构造一个带路径的 IO 错误。
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }

    /// 构造一个数据格式错误。
    pub fn malformed(file: &'static str, detail: impl Into<String>) -> Self {
        CoreError::Malformed {
            file,
            detail: detail.into(),
        }
    }
}
