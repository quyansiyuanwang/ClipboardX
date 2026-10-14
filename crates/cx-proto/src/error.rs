//! 线协议与落盘格式的错误类型。
//!
//! 解码器对**任何**输入都必须返回 `Err` 而不是 panic：对象存储里的东西可能被截断、
//! 被别的程序覆盖、甚至是攻击者放的垃圾文件。

/// 协议层的所有可能失败。
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// 数据长度不足。
    #[error("数据过短：需要至少 {need} 字节，实际 {got}")]
    Truncated {
        /// 需要的最小字节数。
        need: usize,
        /// 实际字节数。
        got: usize,
    },

    /// 魔数不匹配（不是本程序的 item 容器）。
    #[error("魔数不匹配：不是 CXI 容器")]
    BadMagic,

    /// 容器版本不受支持。
    #[error("不支持的容器版本 {0}")]
    UnsupportedVersion(u8),

    /// kind/cipher 取值非法。
    #[error("非法的 kind/cipher：kind={kind} cipher={cipher}")]
    BadFlags {
        /// 原始 kind 字节。
        kind: u8,
        /// 原始 cipher 字节。
        cipher: u8,
    },

    /// 保留字段必须为 0。
    #[error("保留字段非零")]
    BadReserved,

    /// 元数据长度非法（过大或超出实际数据）。
    #[error("元数据长度非法：{0}")]
    BadMetaLen(usize),

    /// 元数据（postcard）解码失败。
    #[error("元数据解码失败：{0}")]
    MetaDecode(String),

    /// 元数据字段自检失败（长度、取值、与头部不一致等）。
    #[error("元数据字段非法：{0}")]
    MetaInvalid(String),

    /// 对象名格式非法。
    #[error("对象名格式非法：{0}")]
    BadName(String),

    /// JSON 编解码失败（manifest）。
    #[error("JSON 编解码失败：{0}")]
    Json(String),
}
