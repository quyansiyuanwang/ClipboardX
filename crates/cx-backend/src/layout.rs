//! 对象路径布局（所有后端必须一致）。
//!
//! 见 `docs/protocol.md` §2：
//!
//! ```text
//! <root>/cx-space.json
//! <root>/v1/manifests/<device_id>.json
//! <root>/v1/items/<yyyyMMdd>/<ts>-<dev8>-<hash8>.cxi
//! ```
//!
//! 后端本身只认"相对 root 的路径字符串"，路径怎么拼由这里统一决定——
//! 这样 WebDAV / S3 / 自建服务端三种实现的语义不会漂移。

use cx_proto::ItemName;

/// 空间元数据文件名（可公开读，不含密钥明文）。
pub const SPACE_FILE: &str = "cx-space.json";

/// manifest 前缀。
pub const MANIFESTS_PREFIX: &str = "v1/manifests/";

/// item 前缀。
pub const ITEMS_PREFIX: &str = "v1/items/";

/// 某设备的 manifest 路径。
pub fn manifest_path(device_id: &str) -> String {
    format!("{MANIFESTS_PREFIX}{device_id}.json")
}

/// 由 manifest 路径反推设备 id；不符合布局时返回 `None`。
pub fn device_id_from_manifest_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(MANIFESTS_PREFIX)?;
    let device_id = rest.strip_suffix(".json")?;
    // 设备 id 固定是 16 字节随机值的十六进制（32 字符），长度也必须校验
    if device_id.len() != 32 || !device_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(device_id.to_string())
}

/// item 对象路径（名字里已含日期，这里解析出来做目录分层）。
pub fn item_path(name: &str) -> Option<String> {
    let parsed = ItemName::parse(name).ok()?;
    Some(format!("{ITEMS_PREFIX}{}/{name}", parsed.day()))
}

/// 某一天的 items 目录前缀（用来按天列举，避免一次列出全部历史）。
pub fn items_prefix_for_day(day: &str) -> String {
    format!("{ITEMS_PREFIX}{day}/")
}

/// 从 item 路径中取出日期目录与对象名。
pub fn split_item_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(ITEMS_PREFIX)?;
    let (day, name) = rest.split_once('/')?;
    if day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    ItemName::parse(name).ok()?;
    Some((day.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cx_proto::content_hash_hex;

    const DEVICE: &str = "ab12cd34ef5678901234567890abcdef";

    fn sample_name() -> String {
        ItemName::build(1_759_299_998_712, DEVICE, &content_hash_hex(b"hello"))
    }

    #[test]
    fn manifest_路径与反解() {
        let path = manifest_path(DEVICE);
        assert_eq!(path, format!("v1/manifests/{DEVICE}.json"));
        assert_eq!(device_id_from_manifest_path(&path).as_deref(), Some(DEVICE));
    }

    #[test]
    fn 非_manifest_路径反解为_none() {
        assert!(device_id_from_manifest_path("v1/items/20251001/x.cxi").is_none());
        assert!(device_id_from_manifest_path("v1/manifests/.json").is_none());
        assert!(device_id_from_manifest_path("v1/manifests/not-hex.json").is_none());
        assert!(device_id_from_manifest_path("v1/manifests/ab12.json").is_none());
    }

    #[test]
    fn item_路径按天分层() {
        let name = sample_name();
        let path = item_path(&name).unwrap();
        assert!(path.starts_with("v1/items/20251001/"), "{path}");
        assert!(path.ends_with(&name));

        let (day, parsed) = split_item_path(&path).unwrap();
        assert_eq!(day, "20251001");
        assert_eq!(parsed, name);
    }

    #[test]
    fn 非法_item_名字返回_none_不panic() {
        assert!(item_path("bogus.cxi").is_none());
        assert!(split_item_path("v1/items/20251001/bogus.cxi").is_none());
        assert!(split_item_path("v1/items/2025/1759299998712-ab12cd34-9f3c1e77.cxi").is_none());
        assert!(split_item_path("v1/manifests/x.json").is_none());
    }

    #[test]
    fn 前缀常量与协议文档一致() {
        assert_eq!(SPACE_FILE, "cx-space.json");
        assert_eq!(MANIFESTS_PREFIX, "v1/manifests/");
        assert_eq!(ITEMS_PREFIX, "v1/items/");
        assert_eq!(items_prefix_for_day("20251001"), "v1/items/20251001/");
    }
}
