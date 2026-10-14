//! ClipboardX 加密层。
//!
//! 三件事：
//! 1. **载荷加密**：XChaCha20-Poly1305，密钥为空间密钥，AAD = item 对象文件名（见 [`aead`]）。
//! 2. **口令包裹**：`argon2id` 派生包裹密钥来加密空间密钥，使"有对象存储访问权"不等于
//!    "能看到剪贴板内容"（见 [`kdf`]）。
//! 3. **随机与比较**：统一的系统随机数入口与常量时间比较，避免各处自行实现。
//!
//! 约定：所有密钥材料在离开本 crate 前不做日志输出（`SpaceKey` 的 `Debug` 已刻意隐藏内容），
//! 并在 `Drop` 时擦除。

mod aead;
mod error;
mod kdf;

pub use aead::Sealed;
pub use error::CryptoError;
pub use kdf::{KDF_ARGON2ID, KdfParams, SpaceKey, unwrap_space_key, wrap_space_key};

/// 载荷加密算法标识（写入 `cx-space.json` 与 item 元数据）。
pub const CIPHER_XCHACHA20POLY1305: &str = "xchacha20poly1305";

/// 空间密钥长度（字节）。
pub const SPACE_KEY_LEN: usize = 32;

/// XChaCha20-Poly1305 nonce 长度（字节）。
pub const NONCE_LEN: usize = 24;

/// Poly1305 认证标签长度（字节）。
pub const TAG_LEN: usize = 16;

/// 取 `N` 字节系统随机数。
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf)?;
    Ok(buf)
}

/// 用系统随机数填充切片（用于可变长度场景，如令牌）。
pub fn fill_random(buf: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::fill(buf)?;
    Ok(())
}

/// 取 `len_bytes` 字节随机数的十六进制表示（设备 id、item id、控制令牌、注册密钥等）。
pub fn random_hex(len_bytes: usize) -> Result<String, CryptoError> {
    let mut buf = vec![0u8; len_bytes];
    getrandom::fill(&mut buf)?;
    Ok(hex::encode(buf))
}

/// 常量时间比较。
///
/// 用于比较控制令牌、注册密钥等秘密值：长度不同直接返回 `false`（长度本身不是秘密），
/// 长度相同时按字节做常量时间比较，避免通过响应时间泄漏前缀信息。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 随机十六进制长度正确() {
        let s = random_hex(16).unwrap();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn 随机十六进制不重复() {
        let a = random_hex(16).unwrap();
        let b = random_hex(16).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn 填充随机切片() {
        let mut buf = [0u8; 48];
        fill_random(&mut buf).unwrap();
        assert!(buf.iter().any(|b| *b != 0), "填充后不应全为 0");
    }

    #[test]
    fn 类型常量与协议一致() {
        assert_eq!(SPACE_KEY_LEN, 32);
        assert_eq!(NONCE_LEN, 24);
        assert_eq!(TAG_LEN, 16);
        assert_eq!(CIPHER_XCHACHA20POLY1305, "xchacha20poly1305");
    }

    #[test]
    fn 密钥调试输出不泄漏内容() {
        let key = SpaceKey::generate().unwrap();
        let text = format!("{key:?}");
        assert!(text.contains("hidden"));
        assert!(!text.contains(&key.to_hex()));
    }
}
