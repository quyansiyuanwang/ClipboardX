//! 设备身份：设备 id、设备名与单调递增序号。
//!
//! - **设备 id**：16 字节随机值的十六进制（32 字符），首次运行生成后终身不变。
//!   刻意不用机器名/主机指纹：那些会变，一旦变了同步历史就会被当成新设备。
//! - **序号 seq**：同一设备内单调递增，用于在系统时钟回拨/超前时仍保持顺序。
//!   每 32 次才落盘一次（剪贴板场景下崩溃丢失几个序号是无害的：排序还有 created_ms）。

use crate::error::CoreError;
use crate::fsutil;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};

/// 身份文件格式版本。
pub const IDENTITY_VERSION: u32 = 1;

/// 序号落盘间隔。
const SEQ_PERSIST_EVERY: u64 = 32;

/// 设备身份（持久化于 `identity.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// 格式版本。
    pub v: u32,
    /// 设备 id（十六进制，32 字符）。
    pub device_id: String,
    /// 设备名（展示用，可改）。
    pub name: String,
    /// 首次创建时间（毫秒，UTC）。
    pub created_ms: u64,
    /// 最后一次已落盘的序号。
    #[serde(default)]
    pub seq: u64,
}

impl Identity {
    /// 读取身份；不存在则生成一份新的。
    pub fn load_or_create(paths: &Paths, name_hint: Option<&str>) -> Result<Self, CoreError> {
        let path = paths.identity_file();
        if let Some(text) = fsutil::read_to_string_optional(&path)? {
            let identity: Self = serde_json::from_str(&text)
                .map_err(|e| CoreError::malformed("identity.json", e.to_string()))?;
            if identity.v != IDENTITY_VERSION {
                return Err(CoreError::malformed(
                    "identity.json",
                    format!("不支持的版本 {}", identity.v),
                ));
            }
            if identity.device_id.len() != 32
                || !identity.device_id.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(CoreError::malformed(
                    "identity.json",
                    format!("设备 id 非法：{}", identity.device_id),
                ));
            }
            fsutil::tighten_permissions(&path, false)?;
            return Ok(identity);
        }

        let identity = Self::create(name_hint);
        identity.save(paths)?;
        Ok(identity)
    }

    /// 生成一份新身份（不落盘）。
    pub fn create(name_hint: Option<&str>) -> Self {
        let device_id = cx_crypto::random_hex(16).unwrap_or_else(|_| "0".repeat(32));
        Self {
            v: IDENTITY_VERSION,
            device_id,
            name: name_hint.map(str::to_string).unwrap_or_else(hostname),
            created_ms: now_ms(),
            seq: 0,
        }
    }

    /// 落盘（原子写 + 收紧权限）。
    pub fn save(&self, paths: &Paths) -> Result<(), CoreError> {
        let path = paths.identity_file();
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| CoreError::malformed("identity.json", e.to_string()))?;
        fsutil::write_atomic(&path, text.as_bytes())?;
        fsutil::tighten_permissions(&path, false)?;
        Ok(())
    }

    /// 分配下一个序号（内存自增），返回 `(seq, 是否需要落盘)`。
    pub fn bump_seq(&mut self) -> (u64, bool) {
        self.seq = self.seq.saturating_add(1);
        (self.seq, self.seq.is_multiple_of(SEQ_PERSIST_EVERY))
    }

    /// 把序号抬高到至少 `floor`（例如从 state.json 里发现更大的值）。
    pub fn raise_seq_floor(&mut self, floor: u64) {
        if floor > self.seq {
            self.seq = floor;
        }
    }
}

/// 当前 Unix 毫秒。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 主机名（Windows `COMPUTERNAME`、Unix `HOSTNAME`，都没有时退化为 `unknown`）。
pub fn hostname() -> String {
    #[cfg(windows)]
    let keys = ["COMPUTERNAME", "HOSTNAME"];
    #[cfg(not(windows))]
    let keys = ["HOSTNAME", "COMPUTERNAME"];

    for key in keys {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 首次运行生成并可复读() {
        let paths = temp_paths("create");
        let first = Identity::load_or_create(&paths, Some("my-box")).unwrap();
        assert_eq!(first.name, "my-box");
        assert_eq!(first.device_id.len(), 32);
        assert!(first.device_id.bytes().all(|b| b.is_ascii_hexdigit()));

        let second = Identity::load_or_create(&paths, Some("ignored")).unwrap();
        assert_eq!(second.device_id, first.device_id, "设备 id 必须终身不变");
        assert_eq!(second.name, "my-box", "已有身份不应被 name_hint 覆盖");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 设备名默认取主机名() {
        let paths = temp_paths("hostname");
        let identity = Identity::load_or_create(&paths, None).unwrap();
        assert!(!identity.name.is_empty());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 序号单调递增且定期落盘() {
        let mut identity = Identity::create(Some("t"));
        let mut persists = 0;
        let mut last = 0;
        for _ in 0..SEQ_PERSIST_EVERY * 2 {
            let (seq, due) = identity.bump_seq();
            assert!(seq > last, "序号必须严格递增");
            last = seq;
            if due {
                persists += 1;
            }
        }
        assert_eq!(persists, 2, "每 {SEQ_PERSIST_EVERY} 次应落盘一次");
    }

    #[test]
    fn 序号下限抬升() {
        let mut identity = Identity::create(Some("t"));
        identity.raise_seq_floor(100);
        let (seq, _) = identity.bump_seq();
        assert_eq!(seq, 101);
        identity.raise_seq_floor(5);
        let (seq, _) = identity.bump_seq();
        assert_eq!(seq, 102, "更低的下限不应回退序号");
    }

    #[test]
    fn 损坏的身份文件报错() {
        let paths = temp_paths("corrupt");
        std::fs::write(paths.identity_file(), b"{ not json").unwrap();
        assert!(Identity::load_or_create(&paths, None).is_err());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 设备_id_非法时报错() {
        let paths = temp_paths("baddid");
        let bad = Identity {
            v: IDENTITY_VERSION,
            device_id: "zz".to_string(),
            name: "x".to_string(),
            created_ms: 1,
            seq: 0,
        };
        bad.save(&paths).unwrap();
        assert!(Identity::load_or_create(&paths, None).is_err());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
