//! 加密层错误类型。

/// 加密层的所有可能失败。
///
/// 注意：解密失败一律折叠为 [`CryptoError::Decrypt`]，不区分"密钥错误"与"密文被篡改"，
/// 避免给攻击者提供区分信号。
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// 系统随机数不可用（极少见；出现后通常每次调用都会失败）。
    #[error("系统随机数不可用: {0}")]
    Random(#[from] getrandom::Error),

    /// 密钥长度非法。
    #[error("密钥长度非法")]
    KeyLength,

    /// 加密失败（实际不会发生，保留以便上层统一处理）。
    #[error("加密失败")]
    Encrypt,

    /// 解密失败：密钥错误、密文被篡改，或 AAD 不匹配。
    #[error("解密失败（密钥错误、密文被篡改或 AAD 不匹配）")]
    Decrypt,

    /// 口令派生参数非法。
    #[error("口令派生参数非法: {0}")]
    KdfParams(String),

    /// 口令派生计算失败。
    #[error("口令派生失败")]
    Kdf,

    /// 十六进制编码/解码失败。
    #[error("编码失败: {0}")]
    Codec(String),
}
