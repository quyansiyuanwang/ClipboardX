//! 空间密钥与其口令包裹。
//!
//! 模型：
//! - 空间密钥 `K`（32 字节随机）用于加密所有 item 载荷，永远不上传到对象存储。
//! - `K` 由 `argon2id(口令, salt, 参数)` 派生的包裹密钥加密后写进 `cx-space.json`，
//!   因此服务端与对象存储只见包裹后的密文，且**没有口令就无法恢复 `K`**（零知识）。
//! - 换口令只需重新包裹一次，不必重写历史对象。

use crate::error::CryptoError;
use crate::{SPACE_KEY_LEN, aead, random_bytes};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 包裹 `K` 时使用的 AAD，避免包裹密文被跨用途重放。
const WRAP_AAD: &[u8] = b"cx-space-key-v1";

/// 当前唯一的 KDF 标识。
pub const KDF_ARGON2ID: &str = "argon2id";

/// 口令派生参数（与 `cx-space.json` 中的 `kdf` 字段一一对应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// 算法标识，目前只有 `argon2id`。
    pub algo: String,
    /// 盐（十六进制，16 字节）。
    pub salt: String,
    /// 内存开销，单位 KiB。
    pub m_cost: u32,
    /// 迭代次数。
    pub t_cost: u32,
    /// 并行度。
    pub p_cost: u32,
}

impl KdfParams {
    /// 生成一份默认强度的参数（19 MiB / 2 轮 / 并行度 1，约 50 ms 量级）。
    pub fn generate() -> Result<Self, CryptoError> {
        let salt: [u8; 16] = random_bytes()?;
        Ok(Self {
            algo: KDF_ARGON2ID.to_string(),
            salt: hex::encode(salt),
            m_cost: 19_456,
            t_cost: 2,
            p_cost: 1,
        })
    }

    /// 低强度参数：仅用于测试与算力极弱的设备（**不要**用于真实口令）。
    ///
    /// 生产代码不应调用；调用点会被显式标注，便于审计。
    #[doc(hidden)]
    pub fn cheap_for_tests() -> Self {
        Self {
            algo: KDF_ARGON2ID.to_string(),
            salt: "00000000000000000000000000000000".to_string(),
            m_cost: 8,
            t_cost: 1,
            p_cost: 1,
        }
    }

    /// 参数自检：算法受支持、盐长度正确、开销在合理区间。
    pub fn validate(&self) -> Result<(), CryptoError> {
        if self.algo != KDF_ARGON2ID {
            return Err(CryptoError::KdfParams(format!(
                "不支持的算法 {}",
                self.algo
            )));
        }
        let salt = hex::decode(&self.salt).map_err(|e| CryptoError::Codec(e.to_string()))?;
        if salt.len() < 8 {
            return Err(CryptoError::KdfParams(format!(
                "盐过短：{} 字节",
                salt.len()
            )));
        }
        if self.m_cost < 8 || self.t_cost < 1 || self.p_cost < 1 {
            return Err(CryptoError::KdfParams(
                "内存开销/迭代次数/并行度必须为正".to_string(),
            ));
        }
        Ok(())
    }
}

/// 空间密钥。Drop 时擦除内存。
#[derive(Clone)]
pub struct SpaceKey([u8; SPACE_KEY_LEN]);

impl SpaceKey {
    /// 生成一份新的随机空间密钥。
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self(random_bytes()?))
    }

    /// 由已有字节构造（用于从密钥文件读取）。
    pub fn from_bytes(bytes: [u8; SPACE_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// 由十六进制字符串构造。
    pub fn from_hex(text: &str) -> Result<Self, CryptoError> {
        let bytes = hex::decode(text.trim()).map_err(|e| CryptoError::Codec(e.to_string()))?;
        if bytes.len() != SPACE_KEY_LEN {
            return Err(CryptoError::KeyLength);
        }
        let mut arr = [0u8; SPACE_KEY_LEN];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// 以十六进制导出（用于密钥文件与 `cx space key export`）。
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// 原始字节视图。
    pub fn as_bytes(&self) -> &[u8; SPACE_KEY_LEN] {
        &self.0
    }

    /// 加密载荷：`aad_name` 为 item 对象文件名，绑定密文与存储路径（改名即解密失败）。
    pub fn seal(&self, aad_name: &str, plaintext: &[u8]) -> Result<aead::Sealed, CryptoError> {
        aead::seal_with_key(&self.0, aad_name.as_bytes(), plaintext)
    }

    /// 解密载荷。
    pub fn open(
        &self,
        aad_name: &str,
        nonce: &[u8; crate::NONCE_LEN],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        aead::open_with_key(&self.0, aad_name.as_bytes(), nonce, ciphertext)
    }
}

impl std::fmt::Debug for SpaceKey {
    /// 刻意不打印密钥材料。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpaceKey(<hidden>)")
    }
}

impl Drop for SpaceKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl ZeroizeOnDrop for SpaceKey {}

/// 用口令包裹空间密钥，返回可写入 `cx-space.json` 的十六进制字符串。
pub fn wrap_space_key(
    passphrase: &[u8],
    params: &KdfParams,
    key: &SpaceKey,
) -> Result<String, CryptoError> {
    params.validate()?;
    let wrapping_key = derive_key(passphrase, params)?;
    let sealed = aead::seal_with_key(&wrapping_key, WRAP_AAD, key.as_bytes())?;
    let mut blob = Vec::with_capacity(crate::NONCE_LEN + sealed.ciphertext.len());
    blob.extend_from_slice(&sealed.nonce);
    blob.extend_from_slice(&sealed.ciphertext);
    Ok(hex::encode(blob))
}

/// 用口令解出空间密钥。口令错误、密文被改动或参数不一致都会返回
/// [`CryptoError::Decrypt`]。
pub fn unwrap_space_key(
    passphrase: &[u8],
    params: &KdfParams,
    wrapped_hex: &str,
) -> Result<SpaceKey, CryptoError> {
    params.validate()?;
    let blob = hex::decode(wrapped_hex.trim()).map_err(|e| CryptoError::Codec(e.to_string()))?;
    if blob.len() <= crate::NONCE_LEN {
        return Err(CryptoError::Decrypt);
    }
    let (nonce_bytes, ciphertext) = blob.split_at(crate::NONCE_LEN);
    let mut nonce = [0u8; crate::NONCE_LEN];
    nonce.copy_from_slice(nonce_bytes);

    let wrapping_key = derive_key(passphrase, params)?;
    let plain = aead::open_with_key(&wrapping_key, WRAP_AAD, &nonce, ciphertext)?;
    if plain.len() != SPACE_KEY_LEN {
        return Err(CryptoError::KeyLength);
    }
    let mut key = [0u8; SPACE_KEY_LEN];
    key.copy_from_slice(&plain);
    let key = SpaceKey(key);
    drop(plain);
    Ok(key)
}

/// argon2id 派生 32 字节密钥。
fn derive_key(
    passphrase: &[u8],
    params: &KdfParams,
) -> Result<zeroize::Zeroizing<[u8; 32]>, CryptoError> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let salt = hex::decode(&params.salt).map_err(|e| CryptoError::Codec(e.to_string()))?;
    let argon_params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| CryptoError::KdfParams(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);

    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(passphrase, &salt, out.as_mut())
        .map_err(|_| CryptoError::Kdf)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> KdfParams {
        KdfParams::cheap_for_tests()
    }

    #[test]
    fn 随机字节应当各不相同() {
        let a: [u8; 32] = random_bytes().unwrap();
        let b: [u8; 32] = random_bytes().unwrap();
        assert_ne!(a, b, "两次随机结果不应相同");
    }

    #[test]
    fn 载荷加解密往返() {
        let key = SpaceKey::generate().unwrap();
        let name = "1759299998712-ab12cd34-9f3c1e77.cxi";
        let plain = "剪贴板内容 with emoji 🎉".as_bytes();

        let sealed = key.seal(name, plain).unwrap();
        assert_ne!(sealed.ciphertext, plain, "密文不应等于明文");
        assert_eq!(sealed.ciphertext.len(), plain.len() + crate::TAG_LEN);

        let back = key.open(name, &sealed.nonce, &sealed.ciphertext).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn aad_不匹配时解密失败() {
        let key = SpaceKey::generate().unwrap();
        let sealed = key.seal("a.cxi", b"hello").unwrap();
        assert!(
            key.open("b.cxi", &sealed.nonce, &sealed.ciphertext)
                .is_err()
        );
    }

    #[test]
    fn 密文被篡改时解密失败() {
        let key = SpaceKey::generate().unwrap();
        let mut sealed = key.seal("a.cxi", b"hello world").unwrap();
        sealed.ciphertext[0] ^= 0x01;
        assert!(
            key.open("a.cxi", &sealed.nonce, &sealed.ciphertext)
                .is_err()
        );
    }

    #[test]
    fn 错误密钥解密失败() {
        let key1 = SpaceKey::generate().unwrap();
        let key2 = SpaceKey::generate().unwrap();
        let sealed = key1.seal("a.cxi", b"hello").unwrap();
        assert!(
            key2.open("a.cxi", &sealed.nonce, &sealed.ciphertext)
                .is_err()
        );
    }

    #[test]
    fn 口令包裹往返() {
        let key = SpaceKey::generate().unwrap();
        let params = cheap();
        let wrapped = wrap_space_key(b"correct horse", &params, &key).unwrap();
        let back = unwrap_space_key(b"correct horse", &params, &wrapped).unwrap();
        assert_eq!(back.as_bytes(), key.as_bytes());
    }

    #[test]
    fn 错误口令无法解出密钥() {
        let key = SpaceKey::generate().unwrap();
        let params = cheap();
        let wrapped = wrap_space_key(b"right", &params, &key).unwrap();
        assert!(unwrap_space_key(b"wrong", &params, &wrapped).is_err());
    }

    #[test]
    fn 包裹密文被篡改时失败() {
        let key = SpaceKey::generate().unwrap();
        let params = cheap();
        let wrapped = wrap_space_key(b"pw", &params, &key).unwrap();
        let mut bytes = hex::decode(&wrapped).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        assert!(unwrap_space_key(b"pw", &params, &hex::encode(bytes)).is_err());
    }

    #[test]
    fn 盐不一致时无法解出() {
        let key = SpaceKey::generate().unwrap();
        let params = cheap();
        let wrapped = wrap_space_key(b"pw", &params, &key).unwrap();
        let mut other = params.clone();
        other.salt = "ffffffffffffffffffffffffffffffff".to_string();
        assert!(unwrap_space_key(b"pw", &other, &wrapped).is_err());
    }

    #[test]
    fn 十六进制往返() {
        let key = SpaceKey::generate().unwrap();
        let back = SpaceKey::from_hex(&key.to_hex()).unwrap();
        assert_eq!(back.as_bytes(), key.as_bytes());
    }

    #[test]
    fn 十六进制长度错误应报错() {
        assert!(matches!(
            SpaceKey::from_hex("00ff"),
            Err(CryptoError::KeyLength)
        ));
        assert!(SpaceKey::from_hex("zz").is_err());
    }

    #[test]
    fn 参数校验拒绝非法值() {
        let mut p = cheap();
        p.algo = "argon2d".to_string();
        assert!(p.validate().is_err());

        let mut p = cheap();
        p.m_cost = 0;
        assert!(p.validate().is_err());

        let mut p = cheap();
        p.salt = "00".to_string();
        assert!(p.validate().is_err());
    }

    #[test]
    fn 常量时间比较语义正确() {
        assert!(crate::ct_eq(b"abc", b"abc"));
        assert!(!crate::ct_eq(b"abc", b"abd"));
        assert!(!crate::ct_eq(b"abc", b"abcd"));
        assert!(crate::ct_eq(b"", b""));
    }
}
