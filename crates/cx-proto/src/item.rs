//! item 容器：`"CXI1" | ver | kind | cipher | rsv | meta_len:u32le | meta(postcard) | payload`
//!
//! 设计取舍：
//! - **元数据明文、载荷加密**。元数据只有类型/大小/哈希/来源设备，不含内容，
//!   这样"列出对象并决定要不要下载"不需要密钥，对象存储侧的保留策略也能独立工作。
//! - **头部冗余 kind**，让轮询时无需解析 postcard 就能过滤类型。
//! - **解码必须是全防线**：对象存储里的文件可能被截断、被污染、甚至是别人放的垃圾，
//!   因此每个字段都校验，任何异常都返回 [`ProtoError`] 而不是 panic。

use crate::error::ProtoError;
use serde::{Deserialize, Serialize};

/// 容器魔数。
pub const MAGIC: [u8; 4] = *b"CXI1";

/// 容器版本。
pub const CONTAINER_VERSION: u8 = 1;

/// 固定头部长度。
pub const HEADER_LEN: usize = 12;

/// 元数据长度上限（防御性上限，正常元数据远小于此）。
pub const MAX_META_LEN: usize = 64 * 1024;

/// 载荷长度上限（与 `max_item_bytes` 的默认量级一致，防止伪造字段导致巨额分配）。
pub const MAX_PAYLOAD_LEN: u64 = 64 * 1024 * 1024;

/// item id 长度。
pub const ID_LEN: usize = 16;

/// 内容哈希长度（blake3）。
pub const HASH_LEN: usize = 32;

/// XChaCha20-Poly1305 nonce 长度。
pub const NONCE_LEN: usize = 24;

/// Poly1305 认证标签长度。
pub const TAG_LEN: usize = 16;

/// 剪贴板内容类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// 纯文本。
    Text,
    /// 图片（载荷统一为 PNG 字节）。
    Image,
}

impl Kind {
    /// 头部字节编码。
    pub fn as_u8(self) -> u8 {
        match self {
            Kind::Text => 0,
            Kind::Image => 1,
        }
    }

    /// 由头部字节解码。
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Kind::Text),
            1 => Some(Kind::Image),
            _ => None,
        }
    }

    /// 展示/JSON 用的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Image => "image",
        }
    }
}

/// 载荷加密方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    /// 明文（`space.encrypt = false`，或调试场景）。
    None,
    /// XChaCha20-Poly1305。
    XChaCha20Poly1305,
}

impl Cipher {
    /// 头部字节编码。
    pub fn as_u8(self) -> u8 {
        match self {
            Cipher::None => 0,
            Cipher::XChaCha20Poly1305 => 1,
        }
    }

    /// 由头部字节解码。
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Cipher::None),
            1 => Some(Cipher::XChaCha20Poly1305),
            _ => None,
        }
    }

    /// 是否加密。
    pub fn is_encrypted(self) -> bool {
        matches!(self, Cipher::XChaCha20Poly1305)
    }
}

/// item 元数据（明文，可公开读取）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMeta {
    /// 随机 item id（16 字节）。
    pub id: Vec<u8>,
    /// 产生该内容的设备 id（十六进制）。
    pub origin_device: String,
    /// 产生该内容的设备名（仅用于展示）。
    pub origin_name: String,
    /// 产生时间（毫秒，UTC）。
    pub created_ms: u64,
    /// 该设备内单调递增序号（时钟回拨时仍能保持顺序）。
    pub seq: u64,
    /// 内容类型。
    pub kind: Kind,
    /// MIME 类型（可选）。
    pub mime: Option<String>,
    /// 载荷明文的 blake3（32 字节）。
    pub plain_hash: Vec<u8>,
    /// 载荷明文长度。
    pub plain_len: u32,
    /// 加密 nonce（24 字节）；明文载荷时为空。
    pub nonce: Vec<u8>,
}

impl ItemMeta {
    /// 字段自检：长度、取值、以及与头部的一致性都由 [`Item::decode`] 调用。
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.id.len() != ID_LEN {
            return Err(ProtoError::MetaInvalid(format!(
                "item id 长度应为 {ID_LEN}，实际 {}",
                self.id.len()
            )));
        }
        if self.plain_hash.len() != HASH_LEN {
            return Err(ProtoError::MetaInvalid(format!(
                "内容哈希长度应为 {HASH_LEN}，实际 {}",
                self.plain_hash.len()
            )));
        }
        if self.nonce.len() != NONCE_LEN && !self.nonce.is_empty() {
            return Err(ProtoError::MetaInvalid(format!(
                "nonce 长度应为 0 或 {NONCE_LEN}，实际 {}",
                self.nonce.len()
            )));
        }
        if self.origin_device.is_empty() {
            return Err(ProtoError::MetaInvalid("来源设备 id 为空".to_string()));
        }
        if !self.origin_device.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtoError::MetaInvalid(
                "来源设备 id 含非十六进制字符".to_string(),
            ));
        }
        if u64::from(self.plain_len) > MAX_PAYLOAD_LEN {
            return Err(ProtoError::MetaInvalid(format!(
                "明文长度超出上限：{}",
                self.plain_len
            )));
        }
        Ok(())
    }
}

/// 一个完整的 item（元数据 + 载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// 内容类型（与 `meta.kind` 必须一致）。
    pub kind: Kind,
    /// 加密方式（与 `meta.nonce` 是否为空必须一致）。
    pub cipher: Cipher,
    /// 元数据。
    pub meta: ItemMeta,
    /// 载荷（已加密时是密文，长度 = 明文 + [`TAG_LEN`]）。
    pub payload: Vec<u8>,
}

impl Item {
    /// 构造并校验一个 item。
    pub fn new(
        kind: Kind,
        cipher: Cipher,
        meta: ItemMeta,
        payload: Vec<u8>,
    ) -> Result<Self, ProtoError> {
        let item = Self {
            kind,
            cipher,
            meta,
            payload,
        };
        item.validate()?;
        Ok(item)
    }

    /// 结构自检（不影响内容的正确性，只保证与本格式的约定一致）。
    pub fn validate(&self) -> Result<(), ProtoError> {
        self.meta.validate()?;
        if self.meta.kind != self.kind {
            return Err(ProtoError::MetaInvalid(format!(
                "头部 kind={} 与元数据 kind={} 不一致",
                self.kind.as_str(),
                self.meta.kind.as_str()
            )));
        }
        let plain_len = u64::from(self.meta.plain_len);
        let expected = if self.cipher.is_encrypted() {
            if self.meta.nonce.len() != NONCE_LEN {
                return Err(ProtoError::MetaInvalid(
                    "加密载荷必须带 24 字节 nonce".to_string(),
                ));
            }
            plain_len + TAG_LEN as u64
        } else {
            if !self.meta.nonce.is_empty() {
                return Err(ProtoError::MetaInvalid("明文载荷不应带 nonce".to_string()));
            }
            plain_len
        };
        if self.payload.len() as u64 != expected {
            return Err(ProtoError::MetaInvalid(format!(
                "载荷长度应为 {expected}，实际 {}",
                self.payload.len()
            )));
        }
        Ok(())
    }

    /// 编码为落盘/上传字节。
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        self.validate()?;
        let meta_bytes =
            postcard::to_allocvec(&self.meta).map_err(|e| ProtoError::MetaDecode(e.to_string()))?;
        if meta_bytes.len() > MAX_META_LEN {
            return Err(ProtoError::BadMetaLen(meta_bytes.len()));
        }

        let mut out = Vec::with_capacity(HEADER_LEN + meta_bytes.len() + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(CONTAINER_VERSION);
        out.push(self.kind.as_u8());
        out.push(self.cipher.as_u8());
        out.push(0); // 保留
        out.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&meta_bytes);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    /// 解码。对任意输入都必须返回 `Err` 而不是 panic。
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < HEADER_LEN {
            return Err(ProtoError::Truncated {
                need: HEADER_LEN,
                got: bytes.len(),
            });
        }
        if bytes[0..4] != MAGIC {
            return Err(ProtoError::BadMagic);
        }
        let version = bytes[4];
        if version != CONTAINER_VERSION {
            return Err(ProtoError::UnsupportedVersion(version));
        }
        let kind = Kind::from_u8(bytes[5]).ok_or(ProtoError::BadFlags {
            kind: bytes[5],
            cipher: bytes[6],
        })?;
        let cipher = Cipher::from_u8(bytes[6]).ok_or(ProtoError::BadFlags {
            kind: bytes[5],
            cipher: bytes[6],
        })?;
        if bytes[7] != 0 {
            return Err(ProtoError::BadReserved);
        }

        let meta_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        if meta_len > MAX_META_LEN {
            return Err(ProtoError::BadMetaLen(meta_len));
        }
        let meta_end = HEADER_LEN
            .checked_add(meta_len)
            .ok_or(ProtoError::BadMetaLen(meta_len))?;
        if meta_end > bytes.len() {
            return Err(ProtoError::Truncated {
                need: meta_end,
                got: bytes.len(),
            });
        }

        let (meta, remaining) = postcard::take_from_bytes::<ItemMeta>(&bytes[HEADER_LEN..meta_end])
            .map_err(|e| ProtoError::MetaDecode(e.to_string()))?;
        if !remaining.is_empty() {
            return Err(ProtoError::MetaDecode(format!(
                "元数据尾部有 {} 字节多余数据",
                remaining.len()
            )));
        }

        let item = Self {
            kind,
            cipher,
            meta,
            payload: bytes[meta_end..].to_vec(),
        };
        item.validate()?;
        Ok(item)
    }

    /// 明文长度（解密后应有的字节数）。
    pub fn plain_len(&self) -> u32 {
        self.meta.plain_len
    }

    /// 内容哈希（十六进制）。
    pub fn hash_hex(&self) -> String {
        hex::encode(&self.meta.plain_hash)
    }

    /// 校验明文是否与元数据中的哈希一致。
    pub fn verify_plain(&self, plaintext: &[u8]) -> bool {
        plaintext.len() as u64 == u64::from(self.meta.plain_len)
            && content_hash(plaintext).as_slice() == self.meta.plain_hash.as_slice()
    }
}

/// 内容哈希（blake3）。文本取 UTF-8 原文、图片取 PNG 字节。
pub fn content_hash(bytes: &[u8]) -> [u8; HASH_LEN] {
    *blake3::hash(bytes).as_bytes()
}

/// 内容哈希（十六进制小写）。
pub fn content_hash_hex(bytes: &[u8]) -> String {
    hex::encode(content_hash(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(kind: Kind, plain: &[u8], cipher: Cipher) -> ItemMeta {
        ItemMeta {
            id: vec![7u8; ID_LEN],
            origin_device: "ab12cd34ef5678901234567890abcdef".to_string(),
            origin_name: "workstation".to_string(),
            created_ms: 1_759_299_998_712,
            seq: 42,
            kind,
            mime: Some("text/plain; charset=utf-8".to_string()),
            plain_hash: content_hash(plain).to_vec(),
            plain_len: plain.len() as u32,
            nonce: if cipher.is_encrypted() {
                vec![9u8; NONCE_LEN]
            } else {
                Vec::new()
            },
        }
    }

    fn plaintext_item() -> Item {
        let plain = "剪贴板内容 🎉".as_bytes();
        Item::new(
            Kind::Text,
            Cipher::None,
            sample_meta(Kind::Text, plain, Cipher::None),
            plain.to_vec(),
        )
        .unwrap()
    }

    fn encrypted_item() -> Item {
        let plain = b"secret payload".to_vec();
        let mut payload = plain.clone();
        payload.extend_from_slice(&[0u8; TAG_LEN]);
        Item::new(
            Kind::Text,
            Cipher::XChaCha20Poly1305,
            sample_meta(Kind::Text, &plain, Cipher::XChaCha20Poly1305),
            payload,
        )
        .unwrap()
    }

    #[test]
    fn 明文容器往返() {
        let item = plaintext_item();
        let bytes = item.encode().unwrap();
        assert_eq!(&bytes[0..4], b"CXI1");
        let back = Item::decode(&bytes).unwrap();
        assert_eq!(back, item);
    }

    #[test]
    fn 加密容器往返() {
        let item = encrypted_item();
        let back = Item::decode(&item.encode().unwrap()).unwrap();
        assert_eq!(back, item);
        assert!(back.cipher.is_encrypted());
    }

    #[test]
    fn 任意截断长度都必须报错而不是panic() {
        let bytes = encrypted_item().encode().unwrap();
        for cut in 0..bytes.len() {
            let result = Item::decode(&bytes[..cut]);
            assert!(result.is_err(), "截断到 {cut} 字节应报错");
        }
        // 完整数据必须成功
        assert!(Item::decode(&bytes).is_ok());
    }

    #[test]
    fn 头部字段被破坏时报错() {
        let good = plaintext_item().encode().unwrap();

        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(Item::decode(&bad), Err(ProtoError::BadMagic)));

        let mut bad = good.clone();
        bad[4] = 9;
        assert!(matches!(
            Item::decode(&bad),
            Err(ProtoError::UnsupportedVersion(9))
        ));

        let mut bad = good.clone();
        bad[5] = 7;
        assert!(matches!(
            Item::decode(&bad),
            Err(ProtoError::BadFlags { .. })
        ));

        let mut bad = good.clone();
        bad[6] = 7;
        assert!(matches!(
            Item::decode(&bad),
            Err(ProtoError::BadFlags { .. })
        ));

        let mut bad = good.clone();
        bad[7] = 1;
        assert!(matches!(Item::decode(&bad), Err(ProtoError::BadReserved)));

        // 超大的 meta_len
        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(matches!(Item::decode(&bad), Err(ProtoError::BadMetaLen(_))));
    }

    #[test]
    fn 元数据与头部不一致时报错() {
        // 头部说 image，元数据说 text
        let item = plaintext_item();
        let mut bytes = item.encode().unwrap();
        bytes[5] = Kind::Image.as_u8();
        assert!(Item::decode(&bytes).is_err());
    }

    #[test]
    fn 载荷长度不符时报错() {
        let item = plaintext_item();
        let mut bytes = item.encode().unwrap();
        bytes.push(0); // 多一个字节
        assert!(Item::decode(&bytes).is_err());
    }

    #[test]
    fn 非加密载荷带_nonce_时报错() {
        let mut item = plaintext_item();
        item.meta.nonce = vec![1u8; NONCE_LEN];
        assert!(item.encode().is_err());
    }

    #[test]
    fn 随机字节输入不panic() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..3000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = ((state >> 33) as usize) % 64;
            let mut buf = vec![0u8; len];
            for byte in buf.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *byte = (state >> 33) as u8;
            }
            let _ = Item::decode(&buf);
        }
    }

    #[test]
    fn 内容哈希稳定且对改动敏感() {
        let a = content_hash(b"hello");
        let b = content_hash(b"hello");
        let c = content_hash(b"hellp");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(content_hash_hex(b"hello").len(), HASH_LEN * 2);
    }

    #[test]
    fn 校验明文哈希() {
        let item = plaintext_item();
        assert!(item.verify_plain("剪贴板内容 🎉".as_bytes()));
        assert!(!item.verify_plain(b"other"));
        assert!(!item.verify_plain(b""));
    }

    #[test]
    fn 明文长度上限自检() {
        let mut item = plaintext_item();
        item.meta.plain_len = u32::MAX;
        assert!(item.validate().is_err());
    }
}
