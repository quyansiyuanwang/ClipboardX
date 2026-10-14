//! 加密层。
//!
//! - 载荷：XChaCha20-Poly1305，密钥为空间密钥 `K`，随机 24 字节 nonce，AAD 为 item 文件名
//! - 空间密钥：`K` 由口令经 argon2id 派生的包裹密钥加密后写入 `cx-space.json`
//! - 所有密钥材料使用 `zeroize` 擦除；比较使用常量时间实现

/// 载荷加密算法标识，写入 item 元数据与 `cx-space.json`。
pub const CIPHER_XCHACHA20POLY1305: &str = "xchacha20poly1305";

/// 空间密钥长度（字节）。
pub const SPACE_KEY_LEN: usize = 32;

/// XChaCha20-Poly1305 nonce 长度（字节）。
pub const NONCE_LEN: usize = 24;
