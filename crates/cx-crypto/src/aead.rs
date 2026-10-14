//! XChaCha20-Poly1305 载荷加密。
//!
//! 为什么选 XChaCha20-Poly1305：
//! - 192 位随机 nonce，随机生成碰撞概率可以忽略，无需维护计数器状态；
//! - 纯软件实现快、无时序侧信道特性问题、无需硬件 AES 支持；
//! - AAD 用来把密文绑定到它的存储路径（item 文件名），因此对象不能被改名或搬到别的空间。

use crate::error::CryptoError;
use crate::{NONCE_LEN, TAG_LEN};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

/// 加密结果：随机 nonce + 密文（密文尾部含 16 字节认证标签）。
#[derive(Debug, Clone)]
pub struct Sealed {
    /// 24 字节随机 nonce。
    pub nonce: [u8; NONCE_LEN],
    /// 密文（长度 = 明文长度 + 16）。
    pub ciphertext: Vec<u8>,
}

/// 用 32 字节密钥加密；AAD 参与认证但不加密。
pub fn seal_with_key(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Sealed, CryptoError> {
    let nonce_bytes: [u8; NONCE_LEN] = crate::random_bytes()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoError::KeyLength)?;
    let nonce = XNonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;
    Ok(Sealed {
        nonce: nonce_bytes,
        ciphertext,
    })
}

/// 用 32 字节密钥解密；任何不匹配（密钥、AAD、密文、标签）都返回
/// [`CryptoError::Decrypt`]，不区分原因。
pub fn open_with_key(
    key: &[u8; 32],
    aad: &[u8],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < TAG_LEN {
        return Err(CryptoError::Decrypt);
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoError::KeyLength)?;
    let nonce = XNonce::from(*nonce);
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
}
