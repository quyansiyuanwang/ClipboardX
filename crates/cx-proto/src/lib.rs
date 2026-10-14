//! ClipboardX 的线协议与落盘格式。
//!
//! 这个 crate 是各实现之间的**冻结契约**（详见 `docs/protocol.md`）：
//!
//! - [`item`]：不可变内容对象 `v1/items/<day>/<ts>-<dev8>-<hash8>.cxi` 的容器格式；
//! - [`manifest`]：每个设备自己的变更清单 `v1/manifests/<device_id>.json`；
//! - [`name`]：对象命名规则（不可变、可排序、同时充当加密 AAD）；
//! - [`civil`]：毫秒时间戳与日期目录之间的换算（不依赖 chrono）。
//!
//! 所有解码入口都必须满足：**任意输入返回 `Err` 而不是 panic**。

pub mod civil;
pub mod error;
pub mod item;
pub mod manifest;
pub mod name;

pub use error::ProtoError;
pub use item::{
    CONTAINER_VERSION, Cipher, HASH_LEN, ID_LEN, Item, ItemMeta, Kind, MAGIC, NONCE_LEN, TAG_LEN,
    content_hash, content_hash_hex,
};
pub use manifest::{MANIFEST_VERSION, Manifest, ManifestEntry};
pub use name::ItemName;

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// 端到端：构造 item → 命名 → 编码 → 解码 → 校验哈希 → 写 manifest → JSON 往返。
    #[test]
    fn 从内容到_manifest_的完整链路() {
        let device_id = "ab12cd34ef5678901234567890abcdef";
        let plaintext = "跨设备同步的一段中文文本 🎉".as_bytes();
        let created_ms = 1_759_299_998_712u64;
        let hash = content_hash_hex(plaintext);

        let name = ItemName::new(created_ms, device_id, &hash).unwrap();
        let meta = ItemMeta {
            id: vec![1u8; ID_LEN],
            origin_device: device_id.to_string(),
            origin_name: "workstation".to_string(),
            created_ms,
            seq: 1,
            kind: Kind::Text,
            mime: Some("text/plain; charset=utf-8".to_string()),
            plain_hash: content_hash(plaintext).to_vec(),
            plain_len: plaintext.len() as u32,
            nonce: Vec::new(),
        };
        let item = Item::new(Kind::Text, Cipher::None, meta, plaintext.to_vec()).unwrap();
        let bytes = item.encode().unwrap();

        // 落盘后再读回来
        let decoded = Item::decode(&bytes).unwrap();
        assert!(decoded.verify_plain(plaintext));
        assert_eq!(decoded.hash_hex(), hash);
        assert_eq!(
            name.object_path().split('/').next_back().unwrap(),
            name.file_name()
        );

        // 写进 manifest 并能被解析
        let mut manifest = Manifest::new(device_id, "workstation", created_ms);
        assert!(manifest.push(ManifestEntry {
            name: name.file_name(),
            day: name.day(),
            size: bytes.len() as u64,
            hash: hash.clone(),
            kind: Kind::Text,
            created_ms,
            mime: Some("text/plain; charset=utf-8".to_string()),
        }));
        let json = manifest.to_json().unwrap();
        let back = Manifest::from_json(&json).unwrap();
        assert!(back.contains(&name.file_name()));
        assert_eq!(back.items[0].hash, hash);
    }

    /// 对象名是加密 AAD：改名后必须解不开。
    #[test]
    fn 改名导致解密失败() {
        let key = cx_crypto_stub_key();
        let name = ItemName::build(1_759_299_998_712, "ab12cd34ef56", "9f3c1e77");
        let sealed = key.seal(&name, b"payload").unwrap();
        let renamed = ItemName::build(1_759_299_998_713, "ab12cd34ef56", "9f3c1e77");
        assert!(
            key.open(&renamed, &sealed.nonce, &sealed.ciphertext)
                .is_err()
        );
        assert!(key.open(&name, &sealed.nonce, &sealed.ciphertext).is_ok());
    }

    /// 这里只用固定密钥验证 AAD 语义，避免 cx-proto 依赖 cx-crypto。
    fn cx_crypto_stub_key() -> crate::test_support::FixedXChaChaKey {
        crate::test_support::FixedXChaChaKey::new([0x5au8; 32])
    }
}

/// 仅供测试使用的极简 XChaCha20-Poly1305 封装，避免 cx-proto 反向依赖 cx-crypto。
#[cfg(test)]
mod test_support {
    use chacha20poly1305::aead::{Aead, Payload};
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    /// 固定密钥的测试封装。
    pub struct FixedXChaChaKey([u8; 32]);

    impl FixedXChaChaKey {
        /// 用给定字节构造。
        pub fn new(bytes: [u8; 32]) -> Self {
            Self(bytes)
        }

        /// 加密（AAD 为对象名）。
        pub fn seal(&self, aad: &str, plain: &[u8]) -> Result<Sealed, String> {
            let cipher = XChaCha20Poly1305::new_from_slice(&self.0).map_err(|e| e.to_string())?;
            let nonce_bytes = [0x11u8; 24];
            let nonce = XNonce::from(nonce_bytes);
            let ct = cipher
                .encrypt(
                    &nonce,
                    Payload {
                        msg: plain,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|e| e.to_string())?;
            Ok(Sealed {
                nonce: nonce_bytes,
                ciphertext: ct,
            })
        }

        /// 解密。
        pub fn open(&self, aad: &str, nonce: &[u8; 24], ct: &[u8]) -> Result<Vec<u8>, String> {
            let cipher = XChaCha20Poly1305::new_from_slice(&self.0).map_err(|e| e.to_string())?;
            let nonce = XNonce::from(*nonce);
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: ct,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|e| e.to_string())
        }
    }

    /// 加密结果。
    pub struct Sealed {
        /// nonce。
        pub nonce: [u8; 24],
        /// 密文。
        pub ciphertext: Vec<u8>,
    }
}
