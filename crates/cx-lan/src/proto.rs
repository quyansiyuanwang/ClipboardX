//! 帧格式与消息。
//!
//! 帧：`[u32 大端长度][postcard 负载]`。用 postcard 而不是 JSON：item 载荷可以是
//! 几 MB 的密文，JSON 的 base64 膨胀与解析开销都不划算；而控制消息本来也不大。
//!
//! 长度上限是**必须**的：不设上限的话，一个恶意对端只要发 4 字节长度就能让我们
//! 分配 4GB 内存。上限与单对象上限同量级（16 MiB + 信封余量）。

use crate::error::LanError;
use serde::{Deserialize, Serialize};

/// 单帧上限（16 MiB 对象 + 元数据余量）。
pub const MAX_FRAME_BYTES: usize = 17 * 1024 * 1024;

/// 清单条目（用于对端之间"我有什么"）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferEntry {
    /// 对象名。
    pub name: String,
    /// 创建时间（毫秒）。
    pub created_ms: u64,
    /// 字节数。
    pub size: u64,
    /// 类型（`text` / `image`）。
    pub kind: String,
}

/// 局域网消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanMessage {
    /// 握手后的身份交换。
    Hello {
        /// 设备 id。
        device_id: String,
        /// 设备名。
        device_name: String,
        /// 空间 id（用于校验在同一空间）。
        space_id: Option<String>,
    },
    /// 我有哪些条目（建立连接时交换，用于双向补齐）。
    Manifest {
        /// 设备 id。
        device_id: String,
        /// 条目列表。
        entries: Vec<OfferEntry>,
    },
    /// 我想要这些条目。
    Want {
        /// 对象名列表。
        names: Vec<String>,
    },
    /// 一条内容。
    Item {
        /// 对象名。
        name: String,
        /// 容器字节（本身就是密文）。
        bytes: Vec<u8>,
    },
    /// 保活。
    Ping,
    /// 保活应答。
    Pong,
    /// 主动断开。
    Bye,
}

impl LanMessage {
    /// 消息名（日志与错误信息用）。
    pub fn name(&self) -> &'static str {
        match self {
            LanMessage::Hello { .. } => "Hello",
            LanMessage::Manifest { .. } => "Manifest",
            LanMessage::Want { .. } => "Want",
            LanMessage::Item { .. } => "Item",
            LanMessage::Ping => "Ping",
            LanMessage::Pong => "Pong",
            LanMessage::Bye => "Bye",
        }
    }

    /// 编码成字节（不含帧头）。
    pub fn encode(&self) -> Result<Vec<u8>, LanError> {
        let bytes = postcard::to_stdvec(self)
            .map_err(|err| LanError::Protocol(format!("编码 {} 失败：{err}", self.name())))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(LanError::Protocol(format!(
                "{} 编码后 {} 字节，超过上限 {MAX_FRAME_BYTES}",
                self.name(),
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// 从字节解码（不含帧头）。
    pub fn decode(bytes: &[u8]) -> Result<Self, LanError> {
        postcard::from_bytes(bytes).map_err(|err| {
            LanError::Protocol(format!("解码消息失败（{} 字节）：{err}", bytes.len()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 消息往返一致() {
        let messages = vec![
            LanMessage::Hello {
                device_id: "ab12".to_string(),
                device_name: "笔记本".to_string(),
                space_id: Some("space-1".to_string()),
            },
            LanMessage::Manifest {
                device_id: "ab12".to_string(),
                entries: vec![OfferEntry {
                    name: "1759323998000-ab12cd34-9f3c1e77.cxi".to_string(),
                    created_ms: 1_759_323_998_000,
                    size: 123,
                    kind: "text".to_string(),
                }],
            },
            LanMessage::Want {
                names: vec!["a.cxi".to_string(), "b.cxi".to_string()],
            },
            LanMessage::Item {
                name: "a.cxi".to_string(),
                bytes: vec![0u8, 1, 2, 255],
            },
            LanMessage::Ping,
            LanMessage::Pong,
            LanMessage::Bye,
        ];
        for message in messages {
            let encoded = message.encode().unwrap();
            let decoded = LanMessage::decode(&encoded).unwrap();
            assert_eq!(message, decoded, "{} 往返应一致", message.name());
        }
    }

    #[test]
    fn 名字与解码失败的错误信息() {
        assert_eq!(LanMessage::Ping.name(), "Ping");
        let err = LanMessage::decode(&[0xff, 0xff, 0xff, 0xff]).unwrap_err();
        assert!(matches!(err, LanError::Protocol(_)), "{err}");
        assert!(err.to_string().contains("解码"), "{err}");

        // 乱码不应 panic（模糊测试的最小版本）
        for seed in 0u8..64 {
            let junk: Vec<u8> = (0..32u8)
                .map(|i| i.wrapping_mul(seed).wrapping_add(7))
                .collect();
            let _ = LanMessage::decode(&junk);
        }
    }

    #[test]
    fn 超长消息被拒() {
        let message = LanMessage::Item {
            name: "big".to_string(),
            bytes: vec![0u8; MAX_FRAME_BYTES + 1],
        };
        let err = message.encode().unwrap_err();
        assert!(err.to_string().contains("超过上限"), "{err}");
    }
}
