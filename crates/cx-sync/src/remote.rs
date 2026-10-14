//! 远端后端与引擎之间的粘合层。
//!
//! 职责边界：
//! - [`crate::Engine`] 只管"本机该不该发布什么、收到远端该怎么办"，不认识 HTTP；
//! - [`cx_backend::RemoteBackend`] 只管"列前缀、按名字读写不可变对象"；
//! - [`RemoteSync`] 在两者之间：把引擎产出的对象写上去（并维护自己的 manifest），
//!   再按 manifest 增量把别人的对象取回来。
//!
//! 增量靠两层游标（都按设备分开存）：
//! 1. `etag`：manifest 的 ETag 没变就整份跳过（一次 LIST + 少量 HEAD 的成本）；
//! 2. `items_ms`：只看 `created_ms` 比游标新的条目，避免每次重启把所有历史重下一遍。
//!
//! 去重与回环抑制**不在这里做**：那是引擎的 [`crate::Engine::handle`] 的职责
//! （已处理集合 + 抑制表）。这里只保证"不重复下载"，不保证"不重复应用"——
//! 后者需要引擎的状态，两边各管各的才不会出现"两处都以为对方查过"的漏洞。

use crate::engine::Action;
use cx_backend::{BackendError, RemoteBackend, layout, list_manifests};
use cx_proto::{Item, Manifest, ManifestEntry};
use std::collections::BTreeMap;

/// 某台设备 manifest 的游标。
///
/// 直接复用 `cx-core` 里持久化的游标类型（`state.json` 的 `BackendState.cursors`），
/// 避免"内存里一套、落盘另一套"的转换与漂移——那正是最容易长出 bug 的地方。
pub use cx_core::state::Cursor as DeviceCursor;

/// 一轮拉取最多下载多少个对象（防止一次涌入过多内容）。
pub const DEFAULT_MAX_FETCH: usize = 32;

/// 远端同步器：一个实例对应一个后端。
pub struct RemoteSync {
    backend: Box<dyn RemoteBackend>,
    device_id: String,
    device_name: String,
    /// 自己的 manifest（懒加载）。
    own_manifest: Option<Manifest>,
    cursors: BTreeMap<String, DeviceCursor>,
    published: u64,
    applied: u64,
}

/// 一次拉取的结果。
#[derive(Debug)]
pub struct PollOutcome {
    /// 待交给引擎处理的远端对象（引擎会自行去重）。
    pub incoming: Vec<(String, Item)>,
    /// 本轮是否有 manifest 发生变化。
    pub manifest_changed: bool,
}

impl RemoteSync {
    /// 构造。
    pub fn new(backend: Box<dyn RemoteBackend>, device_id: &str, device_name: &str) -> Self {
        Self {
            backend,
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
            own_manifest: None,
            cursors: BTreeMap::new(),
            published: 0,
            applied: 0,
        }
    }

    /// 后端名。
    pub fn name(&self) -> &str {
        self.backend.name()
    }

    /// 后端种类。
    pub fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    /// 人类可读描述。
    pub fn describe(&self) -> String {
        self.backend.describe()
    }

    /// 当前游标（由守护进程持久化）。
    pub fn cursors(&self) -> &BTreeMap<String, DeviceCursor> {
        &self.cursors
    }

    /// 恢复游标（守护进程启动时从 `state.json` 注入）。
    pub fn set_cursors(&mut self, cursors: BTreeMap<String, DeviceCursor>) {
        self.cursors = cursors;
    }

    /// 已发布条数。
    pub fn published(&self) -> u64 {
        self.published
    }

    /// 已应用条数（由调用方在 apply 成功后自增）。
    pub fn note_applied(&mut self, count: u64) {
        self.applied = self.applied.saturating_add(count);
    }

    /// 已应用条数。
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// 探活（`cx backend test` / doctor 用）。
    pub fn probe(&mut self) -> Result<String, BackendError> {
        // 先确保"根"可用：S3 上桶可能还不存在（自建 MinIO 首次使用很常见），
        // 这一步会顺手把桶建出来；WebDAV 的根集合由用户事先配好，这里什么都不做。
        let created = match self.backend.ensure_root() {
            Ok(created) => created,
            Err(err) => {
                return Err(err);
            }
        };
        let root_note = if created {
            "；已自动创建根（S3 的桶）"
        } else {
            ""
        };

        let space = if cx_backend::load_space_meta(&mut *self.backend)?.is_some() {
            "空间元数据：已存在"
        } else {
            "空间元数据：尚未写入（需要 cx space init/join）"
        };
        Ok(format!("{}；{space}{root_note}", self.backend.describe()))
    }

    /// 发布一个对象，并把它记进自己的 manifest。
    ///
    /// `entry` 由引擎给出（含哈希、大小、类型、来源设备）。对象名字含内容哈希，
    /// 因此用 `only_if_absent` 写：已存在说明完全相同的内容已经在远端了。
    pub fn publish(
        &mut self,
        name: &str,
        bytes: &[u8],
        entry: ManifestEntry,
        limits: RetentionLimits,
    ) -> Result<(), BackendError> {
        let path = layout::item_path(name)
            .ok_or_else(|| BackendError::Malformed(format!("非法的对象名：{name}")))?;
        self.backend.put(&path, bytes, true)?;

        let mut manifest = self.load_own_manifest()?;
        manifest.push(entry);
        manifest.updated_ms = cx_core::now_ms();
        let removed = manifest.trim(limits.max_items, limits.max_age_ms, manifest.updated_ms);

        // 只删自己 manifest 裁掉的对象：别人的对象由他们自己裁
        for name in &removed {
            if let Some(path) = layout::item_path(name)
                && let Err(err) = self.backend.delete(&path)
            {
                log::debug!("删除过期对象 {path} 失败：{err}");
            }
        }

        let body = manifest
            .to_json()
            .map_err(|err| BackendError::Malformed(err.to_string()))?;
        self.backend.put(
            &layout::manifest_path(&self.device_id),
            body.as_bytes(),
            false,
        )?;
        self.own_manifest = Some(manifest);
        self.published = self.published.saturating_add(1);
        Ok(())
    }

    /// 拉取一轮：按 manifest 增量取回别人的对象。
    pub fn poll(&mut self, max_fetch: usize) -> Result<PollOutcome, BackendError> {
        let objects = list_manifests(&mut *self.backend)?;
        let mut incoming = Vec::new();
        let mut manifest_changed = false;

        for object in objects {
            let Some(device_id) = layout::device_id_from_manifest_path(&object.path) else {
                continue;
            };
            if device_id == self.device_id {
                continue; // 自己的 manifest 不用读
            }

            let cursor = self.cursors.entry(device_id.clone()).or_default();
            if object.matches_etag(cursor.etag.as_deref()) {
                continue; // ETag 没变 ⇒ 整份跳过
            }

            let Some(raw) = self.backend.get(&object.path)? else {
                continue;
            };
            let text = match String::from_utf8(raw) {
                Ok(text) => text,
                Err(err) => {
                    log::warn!("{device_id} 的 manifest 不是 UTF-8：{err}");
                    continue;
                }
            };
            let manifest = match Manifest::from_json(&text) {
                Ok(manifest) => manifest,
                Err(err) => {
                    // 别人的 manifest 坏了不该拖垮我们：记一笔，下次再说
                    log::warn!("{device_id} 的 manifest 无法解析：{err}");
                    continue;
                }
            };

            manifest_changed = true;
            cursor.etag = object.etag.clone();
            let since = cursor.items_ms;
            let mut newest = since;

            for entry in manifest
                .items
                .iter()
                .filter(|entry| entry.created_ms > since)
            {
                newest = newest.max(entry.created_ms);
                if incoming.len() >= max_fetch {
                    continue;
                }
                let Some(path) = layout::item_path(&entry.name) else {
                    continue;
                };
                let Some(bytes) = self.backend.get(&path)? else {
                    log::debug!("manifest 里提到了 {} 但对象不存在", entry.name);
                    continue;
                };
                match Item::decode(&bytes) {
                    Ok(item) => incoming.push((entry.name.clone(), item)),
                    Err(err) => log::warn!("对象 {} 解码失败：{err}", entry.name),
                }
            }

            cursor.items_ms = newest;
        }

        Ok(PollOutcome {
            incoming,
            manifest_changed,
        })
    }

    /// 懒加载自己的 manifest（首次发布时读一次；远端不存在就从空开始）。
    fn load_own_manifest(&mut self) -> Result<Manifest, BackendError> {
        if let Some(manifest) = &self.own_manifest {
            return Ok(manifest.clone());
        }
        let path = layout::manifest_path(&self.device_id);
        let manifest = match self.backend.get(&path)? {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Manifest::from_json(&text).unwrap_or_else(|err| {
                    log::warn!("本机 manifest 解析失败（{err}），从空开始");
                    Manifest::new(&self.device_id, &self.device_name, cx_core::now_ms())
                }),
                Err(_) => Manifest::new(&self.device_id, &self.device_name, cx_core::now_ms()),
            },
            None => Manifest::new(&self.device_id, &self.device_name, cx_core::now_ms()),
        };
        self.own_manifest = Some(manifest.clone());
        Ok(manifest)
    }
}

/// 保留策略（与 `[sync]` 配置对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    /// 最多保留多少条（0 = 不限）。
    pub max_items: usize,
    /// 最多保留多久（毫秒，0 = 不限）。
    pub max_age_ms: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_items: 200,
            max_age_ms: 30 * 24 * 60 * 60 * 1000,
        }
    }
}

/// 由引擎动作取出发布用的对象（供守护进程的 worker 线程使用）。
pub fn publish_job(action: &Action) -> Option<(&str, &[u8], &ManifestEntry)> {
    match action {
        Action::Publish { name, entry, bytes } => Some((name.as_str(), bytes.as_slice(), entry)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cx_backend::MemoryBackend;

    /// 合法的设备 id（32 位十六进制；对象名解析会校验格式）
    const DEV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DEV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DEV_X: &str = "cccccccccccccccccccccccccccccccc";

    fn limits(items: usize) -> RetentionLimits {
        RetentionLimits {
            max_items: items,
            max_age_ms: 0,
        }
    }

    fn entry(name: &str, created_ms: u64) -> ManifestEntry {
        ManifestEntry {
            name: name.to_string(),
            day: cx_proto::civil::day_from_ms(created_ms),
            size: 10,
            hash: "a".repeat(64),
            kind: cx_proto::Kind::Text,
            created_ms,
            mime: None,
        }
    }

    fn item_for(name: &str, plain: &[u8], created_ms: u64) -> Item {
        cx_proto::Item::new(
            cx_proto::Kind::Text,
            cx_proto::Cipher::None,
            cx_proto::ItemMeta {
                id: cx_proto::content_hash(name.as_bytes())[..cx_proto::ID_LEN].to_vec(),
                origin_device: "aaaa".to_string(),
                origin_name: "dev".to_string(),
                created_ms,
                seq: 1,
                kind: cx_proto::Kind::Text,
                mime: None,
                plain_hash: cx_proto::content_hash(plain).to_vec(),
                plain_len: plain.len() as u32,
                nonce: Vec::new(),
            },
            plain.to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn 发布写入对象与自身_manifest() {
        let mut store = MemoryBackend::new("mem");
        let mut sync = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "box1");

        let name = cx_proto::ItemName::build(1_759_299_998_712, DEV_A, &"b".repeat(64));
        sync.publish(
            &name,
            b"payload",
            entry(&name, 1_759_299_998_712),
            limits(10),
        )
        .unwrap();

        assert_eq!(sync.published(), 1);
        assert!(store.object_count() >= 2, "应有 item 与 manifest");
        let manifest = store
            .get(&layout::manifest_path(DEV_A))
            .expect("manifest 应存在");
        let manifest =
            Manifest::from_json(&String::from_utf8(manifest.expect("manifest 应存在")).unwrap())
                .unwrap();
        assert_eq!(manifest.items.len(), 1);
        assert_eq!(manifest.items[0].name, name);
    }

    #[test]
    fn 重复发布同名对象不报错() {
        let mut store = MemoryBackend::new("mem");
        let mut sync = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "box1");
        let name = cx_proto::ItemName::build(1_759_299_998_712, DEV_A, &"b".repeat(64));

        sync.publish(
            &name,
            b"payload",
            entry(&name, 1_759_299_998_712),
            limits(10),
        )
        .unwrap();
        sync.publish(
            &name,
            b"payload",
            entry(&name, 1_759_299_998_712),
            limits(10),
        )
        .unwrap();

        let manifest = store.get(&layout::manifest_path(DEV_A)).unwrap();
        let manifest =
            Manifest::from_json(&String::from_utf8(manifest.expect("manifest 应存在")).unwrap())
                .unwrap();
        assert_eq!(manifest.items.len(), 1, "同名条目不应重复");
    }

    #[test]
    fn 拉取别的设备的新对象并推进游标() {
        let store = MemoryBackend::new("mem");
        // 设备 A 发布一条
        let mut a = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "boxA");
        let item = item_for("x", b"hello", 1_759_299_998_712);
        let bytes = item.encode().unwrap();
        let name = cx_proto::ItemName::build(1_759_299_998_712, DEV_A, &"c".repeat(64));
        a.publish(&name, &bytes, entry(&name, 1_759_299_998_712), limits(10))
            .unwrap();

        // 设备 B 拉取
        let mut b = RemoteSync::new(Box::new(store.shared("b")), DEV_B, "boxB");
        let outcome = b.poll(DEFAULT_MAX_FETCH).unwrap();
        assert!(outcome.manifest_changed);
        assert_eq!(outcome.incoming.len(), 1);
        assert_eq!(outcome.incoming[0].0, name);
        assert_eq!(outcome.incoming[0].1.plain_len(), 5);

        // 再拉一次：游标已推进，不应重复下载
        let again = b.poll(DEFAULT_MAX_FETCH).unwrap();
        assert!(again.incoming.is_empty(), "游标应阻止重复下载");
        assert!(!again.manifest_changed, "ETag 未变，应整份跳过");
    }

    #[test]
    fn 自己的_manifest_不会被自己拉取() {
        let store = MemoryBackend::new("mem");
        let mut sync = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "boxA");
        let name = cx_proto::ItemName::build(1_759_299_998_712, DEV_A, &"d".repeat(64));
        sync.publish(
            &name,
            b"payload",
            entry(&name, 1_759_299_998_712),
            limits(10),
        )
        .unwrap();

        let outcome = sync.poll(DEFAULT_MAX_FETCH).unwrap();
        assert!(outcome.incoming.is_empty());
        assert!(!outcome.manifest_changed);
    }

    #[test]
    fn 游标注入后跳过历史条目() {
        let store = MemoryBackend::new("mem");
        let mut a = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "boxA");
        let old = 1_759_299_998_712u64;
        let name = cx_proto::ItemName::build(old, DEV_A, &"e".repeat(64));
        a.publish(
            &name,
            &item_for("x", b"old", old).encode().unwrap(),
            entry(&name, old),
            limits(10),
        )
        .unwrap();

        let mut b = RemoteSync::new(Box::new(store.shared("b")), DEV_B, "boxB");
        b.set_cursors(BTreeMap::from([(
            DEV_A.to_string(),
            DeviceCursor {
                etag: None,
                updated_ms: 0,
                items_ms: old + 1, // 比条目更新
            },
        )]));
        let outcome = b.poll(DEFAULT_MAX_FETCH).unwrap();
        assert!(outcome.incoming.is_empty(), "游标之前的内容不该再下载");
    }

    #[test]
    fn 保留策略会裁掉过期对象() {
        let mut store = MemoryBackend::new("mem");
        let mut sync = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "box1");
        for index in 0..5u64 {
            let created = 1_759_299_998_712 + index * 1000;
            let name = cx_proto::ItemName::build(created, DEV_A, &format!("{:0>64}", index));
            sync.publish(&name, b"payload", entry(&name, created), limits(2))
                .unwrap();
        }
        let manifest = store.get(&layout::manifest_path(DEV_A)).unwrap();
        let manifest =
            Manifest::from_json(&String::from_utf8(manifest.expect("manifest 应存在")).unwrap())
                .unwrap();
        assert_eq!(manifest.items.len(), 2, "只保留最新的 2 条");
        // 被裁掉的对象也应从远端删掉
        let items = store.count_with_prefix(layout::ITEMS_PREFIX);
        assert_eq!(items, 2, "过期 item 应被删除，实际 {items}");
    }

    #[test]
    fn 坏_manifest_不影响其他设备() {
        let store = MemoryBackend::new("mem");
        let mut sync = RemoteSync::new(Box::new(store.shared("b")), DEV_B, "boxB");
        // 放一个坏掉的 manifest 与一个正常的
        store.seed(&layout::manifest_path(DEV_X), b"{ not json");
        let mut a = RemoteSync::new(Box::new(store.shared("a")), DEV_A, "boxA");
        let name = cx_proto::ItemName::build(1_759_299_998_712, DEV_A, &"f".repeat(64));
        a.publish(
            &name,
            &item_for("y", b"ok", 1_759_299_998_712).encode().unwrap(),
            entry(&name, 1_759_299_998_712),
            limits(10),
        )
        .unwrap();

        let outcome = sync.poll(DEFAULT_MAX_FETCH).unwrap();
        assert_eq!(
            outcome.incoming.len(),
            1,
            "坏 manifest 应被跳过而不是中断整轮"
        );
    }
}
