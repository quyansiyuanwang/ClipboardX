//! 每个设备自己的变更清单（manifest）。
//!
//! 为什么需要它：直接列 `v1/items/**` 的成本随历史线性增长，而列 `v1/manifests/` 只有
//! 设备数那么多对象。每个设备**只写自己的 manifest**，因此不存在读-改-写竞争，
//! 拉取方也只需要比对 ETag 就能判断"这个设备有没有新东西"。

use crate::error::ProtoError;
use crate::item::Kind;
use serde::{Deserialize, Serialize};

/// manifest 格式版本。
pub const MANIFEST_VERSION: u32 = 1;

/// manifest 中的一条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// item 对象名（`<ts>-<dev8>-<hash8>.cxi`）。
    pub name: String,
    /// 所属日期目录（`yyyyMMdd`）。
    pub day: String,
    /// 对象字节数（含头部与元数据）。
    pub size: u64,
    /// 内容哈希（十六进制）。
    pub hash: String,
    /// 内容类型。
    pub kind: Kind,
    /// 产生时间（毫秒，UTC）。
    pub created_ms: u64,
    /// MIME 类型（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// 某设备的清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// 格式版本。
    pub v: u32,
    /// 该 manifest 的拥有者（设备 id，十六进制）。
    pub device_id: String,
    /// 设备名（展示用）。
    pub device_name: String,
    /// 最后更新时间（毫秒，UTC）。
    pub updated_ms: u64,
    /// 条目，按 `(created_ms, name)` 升序。
    pub items: Vec<ManifestEntry>,
}

impl Manifest {
    /// 新建空 manifest。
    pub fn new(device_id: &str, device_name: &str, updated_ms: u64) -> Self {
        Self {
            v: MANIFEST_VERSION,
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
            updated_ms,
            items: Vec::new(),
        }
    }

    /// 写入一条记录：按名字去重（同名只保留一份），并保持有序。
    ///
    /// 返回 `true` 表示确实是新增（调用方据此决定要不要真的上传对象）。
    pub fn push(&mut self, entry: ManifestEntry) -> bool {
        if let Some(existing) = self.items.iter_mut().find(|e| e.name == entry.name) {
            // 同名视为同一对象：更新哈希/大小等元信息，覆盖为最新
            if *existing == entry {
                return false;
            }
            *existing = entry;
            return false;
        }
        self.items.push(entry);
        self.items.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        true
    }

    /// 是否已包含某对象名。
    pub fn contains(&self, name: &str) -> bool {
        self.items.iter().any(|e| e.name == name)
    }

    /// 按数量与时间裁剪，返回被移除的对象名（调用方据此删除对应对象）。
    ///
    /// - `max_items`：最多保留多少条（0 表示不限制）
    /// - `max_age_ms`：超过该时长的条目被移除（0 表示不限制）
    pub fn trim(&mut self, max_items: usize, max_age_ms: u64, now_ms: u64) -> Vec<String> {
        let before = self.items.len();
        let mut removed = Vec::new();

        if max_age_ms > 0 {
            let cutoff = now_ms.saturating_sub(max_age_ms);
            self.items.retain(|e| {
                if e.created_ms < cutoff {
                    removed.push(e.name.clone());
                    false
                } else {
                    true
                }
            });
        }

        if max_items > 0 && self.items.len() > max_items {
            let drop_count = self.items.len() - max_items;
            let dropped: Vec<String> = self.items[..drop_count]
                .iter()
                .map(|e| e.name.clone())
                .collect();
            removed.extend(dropped);
            self.items.drain(..drop_count);
        }

        debug_assert!(self.items.len() <= before);
        removed
    }

    /// 自检。
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.v != MANIFEST_VERSION {
            return Err(ProtoError::MetaInvalid(format!(
                "不支持的 manifest 版本 {}",
                self.v
            )));
        }
        if self.device_id.is_empty() || !self.device_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtoError::MetaInvalid(
                "manifest 的 device_id 非法".to_string(),
            ));
        }
        for entry in &self.items {
            crate::name::ItemName::parse(&entry.name)?;
        }
        Ok(())
    }

    /// 序列化为 JSON（人类可读，便于用 rclone/浏览器直接排查）。
    pub fn to_json(&self) -> Result<String, ProtoError> {
        serde_json::to_string_pretty(self).map_err(|e| ProtoError::Json(e.to_string()))
    }

    /// 由 JSON 解析（容忍未知字段，便于向后兼容地扩展）。
    pub fn from_json(text: &str) -> Result<Self, ProtoError> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| ProtoError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64, hash: &str) -> ManifestEntry {
        ManifestEntry {
            name: format!("{ts:013}-ab12cd34-{hash}.cxi"),
            day: crate::civil::day_from_ms(ts),
            size: 123,
            hash: format!("{hash}00000000000000000000000000000000000000000000000000000000"),
            kind: Kind::Text,
            created_ms: ts,
            mime: Some("text/plain; charset=utf-8".to_string()),
        }
    }

    #[test]
    fn 写入去重且有序() {
        let mut m = Manifest::new("ab12cd34ef5678901234567890abcdef", "ws", 0);
        assert!(m.push(entry(300, "aa000000")));
        assert!(m.push(entry(100, "bb000000")));
        assert!(m.push(entry(200, "cc000000")));
        assert!(!m.push(entry(200, "cc000000")), "同一条不应重复计入");

        let times: Vec<u64> = m.items.iter().map(|e| e.created_ms).collect();
        assert_eq!(times, vec![100, 200, 300]);
        assert!(m.contains(&entry(200, "cc000000").name));
    }

    #[test]
    fn 按数量裁剪() {
        let mut m = Manifest::new("ab12cd34ef5678901234567890abcdef", "ws", 0);
        for i in 1..=10u64 {
            m.push(entry(i * 1000, &format!("{:08x}", i)));
        }
        let removed = m.trim(3, 0, 100_000);
        assert_eq!(m.items.len(), 3);
        assert_eq!(removed.len(), 7);
        assert_eq!(m.items[0].created_ms, 8000, "应保留最新的 3 条");
    }

    #[test]
    fn 按时间裁剪() {
        let mut m = Manifest::new("ab12cd34ef5678901234567890abcdef", "ws", 0);
        m.push(entry(1_000, "aa000000"));
        m.push(entry(50_000, "bb000000"));
        m.push(entry(99_000, "cc000000"));

        // 保留 30 秒内的：now=100_000，cutoff=70_000
        let removed = m.trim(0, 30_000, 100_000);
        assert_eq!(removed.len(), 2);
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].created_ms, 99_000);
    }

    #[test]
    fn 同时按数量与时间裁剪() {
        let mut m = Manifest::new("ab12cd34ef5678901234567890abcdef", "ws", 0);
        for i in 1..=20u64 {
            m.push(entry(i * 1000, &format!("{:08x}", i)));
        }
        let removed = m.trim(5, 10_000, 20_000);
        assert_eq!(m.items.len(), 5);
        assert_eq!(removed.len(), 15);
        assert!(
            removed
                .iter()
                .all(|n| crate::name::ItemName::parse(n).is_ok())
        );
    }

    #[test]
    fn json_往返() {
        let mut m = Manifest::new(
            "ab12cd34ef5678901234567890abcdef",
            "工作站",
            1_759_299_998_712,
        );
        m.push(entry(1_759_299_998_712, "9f3c1e77"));
        let json = m.to_json().unwrap();
        assert!(json.contains("工作站"));
        let back = Manifest::from_json(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn 未知字段被容忍() {
        let json = r#"{
            "v": 1,
            "device_id": "ab12cd34ef5678901234567890abcdef",
            "device_name": "ws",
            "updated_ms": 1,
            "future_field": {"a": 1},
            "items": []
        }"#;
        let m = Manifest::from_json(json).unwrap();
        assert_eq!(m.items.len(), 0);
    }

    #[test]
    fn 版本不符被拒绝() {
        let json = r#"{"v": 99, "device_id": "ab12cd34ef5678901234567890abcdef", "device_name": "ws", "updated_ms": 1, "items": []}"#;
        assert!(Manifest::from_json(json).is_err());
    }

    #[test]
    fn 非法条目名被拒绝() {
        let json = r#"{"v": 1, "device_id": "ab12cd34ef5678901234567890abcdef", "device_name": "ws", "updated_ms": 1, "items": [{"name": "bogus.cxi", "day": "20251001", "size": 1, "hash": "aa", "kind": "text", "created_ms": 1}]}"#;
        assert!(Manifest::from_json(json).is_err());
    }

    #[test]
    fn 非法json不panic() {
        for bad in [
            "",
            "{",
            "[]",
            "null",
            "{\"v\": \"x\"}",
            "{\"v\":1,\"items\":\"no\"}",
        ] {
            assert!(Manifest::from_json(bad).is_err(), "应当拒绝 {bad}");
        }
    }
}
