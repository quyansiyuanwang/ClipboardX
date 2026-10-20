//! ClipboardX 核心领域层。
//!
//! 与平台、传输、界面无关的一切都放在这里：配置、设备身份、空间与密钥、
//! 剪贴板历史、去重与回环抑制状态、运行时状态。约定：
//!
//! - 不依赖任何平台剪贴板 API；
//! - 不发起网络请求（后端 crate 负责）；
//! - 不访问终端（TUI 负责）；
//! - 所有磁盘写入都是原子的（见 [`fsutil::write_atomic`]）。

pub mod config;
pub mod device;
pub mod error;
pub mod fsutil;
pub mod history;
pub mod paths;
pub mod pattern;
pub mod space;
pub mod state;
pub mod suppress;

pub use config::{ApplyRemoteMode, BackendKind, BackendSection, Config, SyncSection};
pub use device::{Identity, hostname, now_ms};
pub use error::CoreError;
pub use history::{History, HistoryEntry};
pub use paths::Paths;
pub use space::{LocalSpace, SealedPayload, SpaceCipher, SpaceKey, SpaceMeta};
pub use state::{BackendState, Cursor, DaemonState};
pub use suppress::{SeenSet, TtlSet};

/// 生成一个用户能抄下来的随机口令（hex，36 字符）。
///
/// 用随机口令而不是让用户在终端里现想一个：现想的往往太弱，留空又会退化成明文上传。
pub fn random_passphrase() -> Result<String, CoreError> {
    cx_crypto::random_hex(18).map_err(|err| CoreError::SpaceKey(err.to_string()))
}

/// 对象存储布局版本（`v1/` 目录下的落盘格式版本，见 `docs/protocol.md`）。
pub const STORE_LAYOUT_VERSION: u32 = 1;

/// 本地控制套接字协议版本。
pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

/// 空间元数据格式版本（与 [`space::SPACE_FORMAT_VERSION`] 同义，便于顶层引用）。
pub const SPACE_FORMAT_VERSION: u32 = space::SPACE_FORMAT_VERSION;

/// 抑制表默认容量（条）。
pub const DEFAULT_SUPPRESS_CAPACITY: usize = 8;

/// 抑制表条目存活时间（毫秒）。
pub const DEFAULT_SUPPRESS_TTL_MS: u64 = 30_000;

#[cfg(test)]
mod smoke_tests {
    use super::*;
    use cx_proto::{Cipher, Item, ItemMeta, ItemName, Kind};

    /// 端到端串联：配置 → 身份 → 空间（加密）→ 内容 → 加密 → 历史 → 状态 → 抑制表。
    #[test]
    fn 核心层串起来能跑通一次完整往返() {
        let root = std::env::temp_dir().join(format!("cx-core-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        // 配置
        let config = Config::load_or_create(&paths).unwrap();
        assert!(config.problems().is_empty());

        // 身份
        let mut identity = Identity::load_or_create(&paths, Some("smoke-box")).unwrap();
        let (seq, _) = identity.bump_seq();
        assert_eq!(seq, 1);

        // 空间（不加密分支便于断言载荷；加密分支已由 space 模块单测覆盖）
        let space = LocalSpace::create(&paths, None).unwrap();
        assert_eq!(space.cipher.as_item_cipher(), Cipher::None);

        // 内容 → 对象名 → 加密 → item
        let plaintext = "跨设备同步 🎉".as_bytes();
        let hash_hex = cx_proto::content_hash_hex(plaintext);
        let name = ItemName::new(now_ms(), &identity.device_id, &hash_hex).unwrap();
        let sealed = space.cipher.seal(&name.file_name(), plaintext).unwrap();
        let item = Item::new(
            Kind::Text,
            sealed.cipher,
            ItemMeta {
                id: vec![5u8; cx_proto::ID_LEN],
                origin_device: identity.device_id.clone(),
                origin_name: identity.name.clone(),
                created_ms: name.created_ms,
                seq,
                kind: Kind::Text,
                mime: Some("text/plain; charset=utf-8".to_string()),
                plain_hash: cx_proto::content_hash(plaintext).to_vec(),
                plain_len: plaintext.len() as u32,
                nonce: sealed.nonce,
            },
            sealed.payload,
        )
        .unwrap();

        // 加密器能解回原文（并校验哈希）
        assert_eq!(
            space.cipher.open(&name.file_name(), &item).unwrap(),
            plaintext
        );

        // 历史
        let mut history = History::open(&paths).unwrap();
        history.append(&name.file_name(), &item).unwrap();
        assert_eq!(history.read_newest(0).unwrap(), item);
        let entry = history.newest_first(1)[0];
        assert_eq!(entry.plain_hash, hash_hex);

        // 状态
        let mut state = DaemonState::load(&paths).unwrap();
        state.mark_seen(&name.file_name());
        assert!(state.is_seen(&name.file_name()));
        let backend = state.backend_mut("home", true);
        backend.record_success(now_ms());
        backend.record_failure(now_ms(), "测试失败", 0);
        assert!(backend.last_error.is_some());
        state.save(&paths).unwrap();
        assert_eq!(DaemonState::load(&paths).unwrap().seen_items.len(), 1);

        // 抑制表：模拟"远端写入本机剪贴板触发 watcher"
        let mut suppress = TtlSet::new(DEFAULT_SUPPRESS_CAPACITY, DEFAULT_SUPPRESS_TTL_MS);
        suppress.insert(&hash_hex, 10_000);
        assert!(suppress.contains(&hash_hex, 10_001), "应抑制回声");
        assert!(!suppress.contains(&hash_hex, 10_000 + DEFAULT_SUPPRESS_TTL_MS + 1));

        // 忽略规则
        assert!(pattern::is_blank("   "));
        assert!(pattern::glob_match("*密码*", "这里是我的密码"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
