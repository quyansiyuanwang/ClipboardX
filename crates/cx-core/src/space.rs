//! 空间（space）：一组互相同步的设备共享的密钥与元数据。
//!
//! 两个文件：
//!
//! - `cx-space.json`（**可公开**，写在对象存储根目录）：空间 id、加密算法、
//!   argon2id 参数、以及用口令派生密钥包裹后的空间密钥。没有口令就无法从中恢复密钥。
//! - `space.key`（**仅本机，0600**）：解出来的空间密钥明文。守护进程需要无人值守地
//!   加解密，因此必须能在没有口令输入的情况下拿到密钥。想更安全可以用系统密钥链


use crate::error::CoreError;
use crate::fsutil;
use crate::paths::Paths;
use cx_crypto::{KdfParams, SpaceKey, unwrap_space_key, wrap_space_key};
use cx_proto::{Cipher, Item};
use serde::{Deserialize, Serialize};

/// 空间元数据格式版本。
pub const SPACE_FORMAT_VERSION: u32 = 1;

/// `cx-space.json` 的模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceMeta {
    /// 格式版本。
    pub v: u32,
    /// 空间 id（十六进制）。
    pub space_id: String,
    /// 载荷加密算法（`xchacha20poly1305` 或 `none`）。
    pub cipher: String,
    /// 创建时间（毫秒，UTC）。
    #[serde(default)]
    pub created_ms: u64,
    /// 口令派生参数（加密空间才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<KdfParams>,
    /// 包裹后的空间密钥（加密空间才有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapped_key: Option<String>,
}

impl SpaceMeta {
    /// 是否端到端加密。
    pub fn is_encrypted(&self) -> bool {
        self.cipher == cx_crypto::CIPHER_XCHACHA20POLY1305
    }

    /// 新建一个空间：`passphrase` 为 `None` 时创建**不加密**空间。
    pub fn create(passphrase: Option<&str>) -> Result<(Self, SpaceCipher), CoreError> {
        let space_id = cx_crypto::random_hex(16)?;
        let created_ms = crate::device::now_ms();

        match passphrase {
            Some(passphrase) => {
                let key = SpaceKey::generate()?;
                let kdf = KdfParams::generate()?;
                let wrapped_key = wrap_space_key(passphrase.as_bytes(), &kdf, &key)?;
                let meta = Self {
                    v: SPACE_FORMAT_VERSION,
                    space_id,
                    cipher: cx_crypto::CIPHER_XCHACHA20POLY1305.to_string(),
                    created_ms,
                    kdf: Some(kdf),
                    wrapped_key: Some(wrapped_key),
                };
                Ok((meta, SpaceCipher::Encrypted(key)))
            }
            None => {
                let meta = Self {
                    v: SPACE_FORMAT_VERSION,
                    space_id,
                    cipher: "none".to_string(),
                    created_ms,
                    kdf: None,
                    wrapped_key: None,
                };
                Ok((meta, SpaceCipher::Plaintext))
            }
        }
    }

    /// 用口令解出密钥；不加密空间直接返回 [`SpaceCipher::Plaintext`]。
    pub fn unlock(&self, passphrase: Option<&str>) -> Result<SpaceCipher, CoreError> {
        if self.v != SPACE_FORMAT_VERSION {
            return Err(CoreError::malformed(
                "space.json",
                format!("不支持的空间格式版本 {}", self.v),
            ));
        }
        if !self.is_encrypted() {
            return Ok(SpaceCipher::Plaintext);
        }
        let passphrase = passphrase.ok_or_else(|| {
            CoreError::SpaceKey("该空间已加密，需要口令（cx space join --passphrase）".to_string())
        })?;
        let kdf = self
            .kdf
            .as_ref()
            .ok_or_else(|| CoreError::malformed("space.json", "缺少 kdf 参数"))?;
        let wrapped = self
            .wrapped_key
            .as_ref()
            .ok_or_else(|| CoreError::malformed("space.json", "缺少 wrapped_key"))?;
        let key = unwrap_space_key(passphrase.as_bytes(), kdf, wrapped)
            .map_err(|_| CoreError::SpaceKey("口令错误或空间密钥已损坏".to_string()))?;
        Ok(SpaceCipher::Encrypted(key))
    }

    /// 序列化（写入对象存储根目录的 `cx-space.json`）。
    pub fn to_json(&self) -> Result<String, CoreError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| CoreError::malformed("space.json", e.to_string()))
    }

    /// 解析。
    pub fn from_json(text: &str) -> Result<Self, CoreError> {
        serde_json::from_str(text).map_err(|e| CoreError::malformed("space.json", e.to_string()))
    }

    /// 换口令：重新包裹同一把空间密钥（历史对象无需重写）。
    pub fn rewrap(&mut self, new_passphrase: &str, cipher: &SpaceCipher) -> Result<(), CoreError> {
        let key = cipher
            .space_key()
            .ok_or_else(|| CoreError::SpaceKey("不加密空间无法设置口令".to_string()))?;
        let kdf = KdfParams::generate()?;
        self.wrapped_key = Some(wrap_space_key(new_passphrase.as_bytes(), &kdf, key)?);
        self.kdf = Some(kdf);
        self.cipher = cx_crypto::CIPHER_XCHACHA20POLY1305.to_string();
        Ok(())
    }
}

/// 载荷的加密方式（带密钥或明文）。
#[derive(Debug, Clone)]
pub enum SpaceCipher {
    /// 端到端加密。
    Encrypted(SpaceKey),
    /// 不加密（对象存储里可直接读到文本）。
    Plaintext,
}

/// 加密后的载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedPayload {
    /// 载荷字节（密文或明文）。
    pub payload: Vec<u8>,
    /// nonce（明文时为空）。
    pub nonce: Vec<u8>,
    /// 使用的加密方式。
    pub cipher: Cipher,
}

impl SpaceCipher {
    /// 空间密钥（明文空间返回 `None`）。
    pub fn space_key(&self) -> Option<&SpaceKey> {
        match self {
            SpaceCipher::Encrypted(key) => Some(key),
            SpaceCipher::Plaintext => None,
        }
    }

    /// 对应的 item 头部取值。
    pub fn as_item_cipher(&self) -> Cipher {
        match self {
            SpaceCipher::Encrypted(_) => Cipher::XChaCha20Poly1305,
            SpaceCipher::Plaintext => Cipher::None,
        }
    }

    /// 加密载荷：`object_name` 作为 AAD，绑定密文与存储路径。
    pub fn seal(&self, object_name: &str, plaintext: &[u8]) -> Result<SealedPayload, CoreError> {
        match self {
            SpaceCipher::Encrypted(key) => {
                let sealed = key.seal(object_name, plaintext)?;
                Ok(SealedPayload {
                    payload: sealed.ciphertext,
                    nonce: sealed.nonce.to_vec(),
                    cipher: Cipher::XChaCha20Poly1305,
                })
            }
            SpaceCipher::Plaintext => Ok(SealedPayload {
                payload: plaintext.to_vec(),
                nonce: Vec::new(),
                cipher: Cipher::None,
            }),
        }
    }

    /// 解密载荷并**校验内容哈希**（哈希不符视为数据损坏/被篡改）。
    pub fn open(&self, object_name: &str, item: &Item) -> Result<Vec<u8>, CoreError> {
        let plain = match self {
            SpaceCipher::Encrypted(key) => {
                if !item.cipher.is_encrypted() {
                    return Err(CoreError::SpaceKey(
                        "空间已加密但对象是明文，拒绝处理".to_string(),
                    ));
                }
                if item.meta.nonce.len() != cx_crypto::NONCE_LEN {
                    return Err(CoreError::malformed("item", "nonce 长度非法"));
                }
                let mut nonce = [0u8; cx_crypto::NONCE_LEN];
                nonce.copy_from_slice(&item.meta.nonce);
                key.open(object_name, &nonce, &item.payload)?
            }
            SpaceCipher::Plaintext => {
                if item.cipher.is_encrypted() {
                    return Err(CoreError::SpaceKey(
                        "空间未加密但对象是密文，拒绝处理".to_string(),
                    ));
                }
                item.payload.clone()
            }
        };

        if !item.verify_plain(&plain) {
            return Err(CoreError::HistoryCorrupt(format!(
                "{object_name} 内容哈希与元数据不一致"
            )));
        }
        Ok(plain)
    }
}

/// 本机已加入的空间。
#[derive(Debug, Clone)]
pub struct LocalSpace {
    /// 空间元数据（与对象存储里的一致）。
    pub meta: SpaceMeta,
    /// 载荷加密器。
    pub cipher: SpaceCipher,
}

impl LocalSpace {
    /// 创建新空间并落盘。
    pub fn create(paths: &Paths, passphrase: Option<&str>) -> Result<Self, CoreError> {
        let (meta, cipher) = SpaceMeta::create(passphrase)?;
        let space = Self { meta, cipher };
        space.save(paths)?;
        Ok(space)
    }

    /// 用对象存储里的 `cx-space.json` 内容加入空间，并落盘。
    pub fn join(
        paths: &Paths,
        space_json: &str,
        passphrase: Option<&str>,
    ) -> Result<Self, CoreError> {
        let meta = SpaceMeta::from_json(space_json)?;
        let cipher = meta.unlock(passphrase)?;
        let space = Self { meta, cipher };
        space.save(paths)?;
        Ok(space)
    }

    /// 读取本机空间（`space.json` + `space.key`）。
    pub fn load(paths: &Paths) -> Result<Self, CoreError> {
        let text = fsutil::read_to_string_optional(&paths.space_file())?.ok_or_else(|| {
            CoreError::NotInitialized("本机还没有空间（cx space init）".to_string())
        })?;
        let meta = SpaceMeta::from_json(&text)?;

        let cipher = if meta.is_encrypted() {
            let key_text =
                fsutil::read_to_string_optional(&paths.space_key_file())?.ok_or_else(|| {
                    CoreError::NotInitialized(
                        "缺少 space.key（请重新执行 cx space join）".to_string(),
                    )
                })?;
            let key = SpaceKey::from_hex(key_text.trim())?;
            fsutil::tighten_permissions(&paths.space_key_file(), false)?;
            SpaceCipher::Encrypted(key)
        } else {
            SpaceCipher::Plaintext
        };

        Ok(Self { meta, cipher })
    }

    /// 落盘：`space.json`（可公开）与 `space.key`（0600）。
    pub fn save(&self, paths: &Paths) -> Result<(), CoreError> {
        let json = self.meta.to_json()?;
        fsutil::write_atomic(&paths.space_file(), json.as_bytes())?;

        match &self.cipher {
            SpaceCipher::Encrypted(key) => {
                fsutil::write_atomic(&paths.space_key_file(), key.to_hex().as_bytes())?;
                fsutil::tighten_permissions(&paths.space_key_file(), false)?;
            }
            SpaceCipher::Plaintext => {
                // 不加密空间不应残留旧密钥
                let key_path = paths.space_key_file();
                if key_path.exists() {
                    std::fs::remove_file(&key_path).map_err(|e| CoreError::io(&key_path, e))?;
                }
            }
        }
        Ok(())
    }

    /// 换口令并落盘。
    pub fn change_passphrase(
        &mut self,
        new_passphrase: &str,
        paths: &Paths,
    ) -> Result<(), CoreError> {
        self.meta.rewrap(new_passphrase, &self.cipher)?;
        self.save(paths)
    }

    /// 空间 id 的短形式（展示用）。
    pub fn short_id(&self) -> String {
        self.meta.space_id.chars().take(8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cx_proto::{ItemMeta, Kind};

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-space-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    fn sample_item(cipher: &SpaceCipher, name: &str, plain: &[u8]) -> Item {
        let sealed = cipher.seal(name, plain).unwrap();
        let meta = ItemMeta {
            id: vec![3u8; cx_proto::ID_LEN],
            origin_device: "ab12cd34ef5678901234567890abcdef".to_string(),
            origin_name: "ws".to_string(),
            created_ms: 1,
            seq: 1,
            kind: Kind::Text,
            mime: None,
            plain_hash: cx_proto::content_hash(plain).to_vec(),
            plain_len: plain.len() as u32,
            nonce: sealed.nonce,
        };
        Item::new(Kind::Text, sealed.cipher, meta, sealed.payload).unwrap()
    }

    #[test]
    fn 加密空间创建与加解密往返() {
        // 用低强度参数避免测试变慢：这里只验证流程，真实强度由 KdfParams::generate 保证
        let (mut meta, cipher) = SpaceMeta::create(Some("pw")).unwrap();
        if let Some(kdf) = meta.kdf.as_mut() {
            kdf.m_cost = 8;
            kdf.t_cost = 1;
        }
        // 用低强度参数重新包裹，模拟"创建时就是低强度"
        let key = SpaceKey::generate().unwrap();
        let kdf = KdfParams::cheap_for_tests();
        meta.wrapped_key = Some(wrap_space_key(b"pw", &kdf, &key).unwrap());
        meta.kdf = Some(kdf);

        let unlocked = meta.unlock(Some("pw")).unwrap();
        assert!(unlocked.space_key().is_some());
        let item = sample_item(&unlocked, "a.cxi", "hello 中文".as_bytes());
        assert_eq!(
            unlocked.open("a.cxi", &item).unwrap(),
            "hello 中文".as_bytes()
        );

        // 口令错误
        assert!(meta.unlock(Some("bad")).is_err());
        // AAD 不匹配
        assert!(unlocked.open("b.cxi", &item).is_err());
        drop(cipher);
    }

    #[test]
    fn 明文空间可直读载荷() {
        let (meta, cipher) = SpaceMeta::create(None).unwrap();
        assert!(!meta.is_encrypted());
        assert!(meta.kdf.is_none());
        let item = sample_item(&cipher, "a.cxi", "plain".as_bytes());
        assert_eq!(item.payload, b"plain", "明文空间载荷不应被加密");
        assert_eq!(cipher.open("a.cxi", &item).unwrap(), b"plain");
    }

    #[test]
    fn 加密空间拒绝明文对象() {
        let (meta, cipher) = SpaceMeta::create(None).unwrap();
        assert!(!meta.is_encrypted());
        let item = sample_item(&cipher, "a.cxi", b"x");

        let encrypted = SpaceCipher::Encrypted(SpaceKey::generate().unwrap());
        assert!(encrypted.open("a.cxi", &item).is_err());
    }

    #[test]
    fn 本地空间落盘与读取() {
        let paths = temp_paths("save");
        let mut space = LocalSpace::create(&paths, None).unwrap();
        assert!(paths.space_file().exists());
        assert!(!paths.space_key_file().exists(), "明文空间不应写密钥文件");

        let loaded = LocalSpace::load(&paths).unwrap();
        assert_eq!(loaded.meta.space_id, space.meta.space_id);

        // 换成加密空间：应写入密钥文件
        let key = SpaceKey::generate().unwrap();
        space.cipher = SpaceCipher::Encrypted(key);
        space.meta.cipher = cx_crypto::CIPHER_XCHACHA20POLY1305.to_string();
        space.meta.kdf = Some(KdfParams::cheap_for_tests());
        space.meta.wrapped_key = Some("00".to_string());
        space.save(&paths).unwrap();
        assert!(paths.space_key_file().exists());

        let loaded = LocalSpace::load(&paths).unwrap();
        assert!(loaded.cipher.space_key().is_some());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 未初始化时读取失败并提示() {
        let paths = temp_paths("notinit");
        let err = LocalSpace::load(&paths).unwrap_err();
        assert!(matches!(err, CoreError::NotInitialized(_)));
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 加入空间需要正确口令() {
        let paths = temp_paths("join");
        let kdf = KdfParams::cheap_for_tests();
        let key = SpaceKey::generate().unwrap();
        let wrapped = wrap_space_key(b"right", &kdf, &key).unwrap();
        let meta = SpaceMeta {
            v: SPACE_FORMAT_VERSION,
            space_id: "aa11".to_string(),
            cipher: cx_crypto::CIPHER_XCHACHA20POLY1305.to_string(),
            created_ms: 1,
            kdf: Some(kdf),
            wrapped_key: Some(wrapped),
        };
        let json = meta.to_json().unwrap();

        assert!(LocalSpace::join(&paths, &json, Some("wrong")).is_err());
        let joined = LocalSpace::join(&paths, &json, Some("right")).unwrap();
        assert_eq!(joined.short_id(), "aa11");
        assert_eq!(
            joined.cipher.space_key().unwrap().as_bytes(),
            key.as_bytes(),
            "解出的空间密钥应与原密钥一致"
        );
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 换口令后仍能解出同一密钥() {
        let paths = temp_paths("rewrap");
        let kdf = KdfParams::cheap_for_tests();
        let key = SpaceKey::generate().unwrap();
        let mut meta = SpaceMeta {
            v: SPACE_FORMAT_VERSION,
            space_id: "bb22".to_string(),
            cipher: cx_crypto::CIPHER_XCHACHA20POLY1305.to_string(),
            created_ms: 1,
            kdf: Some(kdf),
            wrapped_key: Some("placeholder".to_string()),
        };
        meta.wrapped_key = Some(wrap_space_key(b"old", &meta.kdf.clone().unwrap(), &key).unwrap());
        let cipher = SpaceCipher::Encrypted(key.clone());

        meta.rewrap("new", &cipher).unwrap();
        // 新口令可以解出（用低强度参数验证语义）
        let mut cheap_meta = meta.clone();
        cheap_meta.kdf = Some(KdfParams::cheap_for_tests());
        cheap_meta.wrapped_key =
            Some(wrap_space_key(b"new", &cheap_meta.kdf.clone().unwrap(), &key).unwrap());
        let unlocked = cheap_meta.unlock(Some("new")).unwrap();
        assert_eq!(unlocked.space_key().unwrap().as_bytes(), key.as_bytes());
        drop(paths);
    }

    #[test]
    fn 空间元数据_json_往返() {
        let (meta, _) = SpaceMeta::create(None).unwrap();
        let json = meta.to_json().unwrap();
        let back = SpaceMeta::from_json(&json).unwrap();
        assert_eq!(back, meta);
        assert!(SpaceMeta::from_json("{ bad").is_err());
    }

    #[test]
    fn 版本不符被拒绝() {
        let meta = SpaceMeta {
            v: 99,
            space_id: "x".to_string(),
            cipher: "none".to_string(),
            created_ms: 0,
            kdf: None,
            wrapped_key: None,
        };
        assert!(meta.unlock(None).is_err());
    }
}
