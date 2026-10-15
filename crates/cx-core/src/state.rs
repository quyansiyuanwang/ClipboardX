//! 运行时状态：暂停开关、每个后端的游标与退避、已处理集合。
//!
//! 持久化在 `state.json`，因为**重启后必须记得"这条已经同步过"**：
//! 否则每次重启都会把历史里的最后一条重新推到所有后端。

use crate::error::CoreError;
use crate::fsutil;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 状态文件版本。
pub const STATE_VERSION: u32 = 1;

/// 已处理对象名的持久化上限（与 `cx_sync` 运行时的容量保持一致）。
pub const SEEN_CAPACITY: usize = 1024;

/// 退避上限（毫秒）。
pub const MAX_BACKOFF_MS: u64 = 300_000;

/// 首次失败后的退避基数（毫秒）。
pub const BASE_BACKOFF_MS: u64 = 1_000;

/// 某个设备在某后端上的拉取游标。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cursor {
    /// 该设备 manifest 的 ETag（未变则跳过下载）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// manifest 的 Last-Modified（毫秒，UTC）。
    #[serde(default)]
    pub updated_ms: u64,
    /// 已见到的最大 `created_ms`。
    #[serde(default)]
    pub items_ms: u64,
    /// 游标算法版本；旧时间戳游标升级时重新扫描，靠对象名去重补齐漏项。
    #[serde(default)]
    pub progress_v: u32,
    /// 当前 manifest 已成功下载并交付的对象名（不能用最大时间越过未处理条目）。
    #[serde(default)]
    pub completed_names: Vec<String>,
}

/// 单个后端的运行状态。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackendState {
    /// 是否启用。
    #[serde(default = "crate::state::default_true")]
    pub enabled: bool,
    /// 各设备的游标。
    #[serde(default)]
    pub cursors: BTreeMap<String, Cursor>,
    /// 上次轮询时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_ms: Option<u64>,
    /// 上次成功时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    /// 最近一次错误信息（`cx status` 展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 连续失败次数（用于退避）。
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 下次允许尝试的时间。
    #[serde(default)]
    pub next_attempt_ms: u64,
    /// 累计发布条数。
    #[serde(default)]
    pub published: u64,
    /// 累计应用远端条数。
    #[serde(default)]
    pub applied: u64,
}

pub(crate) fn default_true() -> bool {
    true
}

impl BackendState {
    /// 是否到了可以重试的时间。
    pub fn is_due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_attempt_ms
    }

    /// 记录一次成功：清空错误与退避。
    pub fn record_success(&mut self, now_ms: u64) {
        self.last_poll_ms = Some(now_ms);
        self.last_ok_ms = Some(now_ms);
        self.last_error = None;
        self.consecutive_failures = 0;
        self.next_attempt_ms = now_ms;
    }

    /// 记录一次失败：递增退避（带上抖动，避免多设备同时重试）。
    pub fn record_failure(&mut self, now_ms: u64, error: impl Into<String>, jitter: u64) {
        self.last_poll_ms = Some(now_ms);
        self.last_error = Some(error.into());
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.next_attempt_ms = now_ms.saturating_add(backoff_ms(self.consecutive_failures, jitter));
    }

    /// 记录一次发布。
    pub fn count_published(&mut self, count: u64) {
        self.published = self.published.saturating_add(count);
    }

    /// 记录一次远端应用。
    pub fn count_applied(&mut self, count: u64) {
        self.applied = self.applied.saturating_add(count);
    }
}

/// 指数退避：`base * 2^(failures-1)`，上限 [`MAX_BACKOFF_MS`]，再叠加抖动。
///
/// `jitter` 取 `0..=20` 表示百分比；抖动由调用方提供随机数，便于测试。
pub fn backoff_ms(failures: u32, jitter_percent: u64) -> u64 {
    // 指数上限给到 16：确保退避能真正触到 MAX_BACKOFF_MS（8 时最大只有 256 秒）
    let exponent = failures.saturating_sub(1).min(16);
    let base = BASE_BACKOFF_MS.saturating_mul(1u64 << exponent);
    let capped = base.min(MAX_BACKOFF_MS);
    if jitter_percent == 0 {
        return capped;
    }
    let delta = capped * jitter_percent.min(20) / 100;
    capped.saturating_sub(delta / 2).saturating_add(delta)
}

/// 守护进程整体状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonState {
    /// 格式版本。
    #[serde(default = "crate::state::default_version")]
    pub v: u32,
    /// 是否暂停同步。
    #[serde(default)]
    pub paused: bool,
    /// 最后更新时间。
    #[serde(default)]
    pub updated_ms: u64,
    /// 已处理的对象名（按插入序，超出 [`SEEN_CAPACITY`] 时丢弃最旧）。
    #[serde(default)]
    pub seen_items: Vec<String>,
    /// 本机最后一次发布的内容哈希（重启后避免重复发布）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_published_hash: Option<String>,
    /// 因超出体积上限被跳过的条数。
    #[serde(default)]
    pub skipped_oversize: u64,
    /// 因忽略规则被跳过的条数。
    #[serde(default)]
    pub skipped_ignored: u64,
    /// 各后端状态。
    #[serde(default)]
    pub backends: BTreeMap<String, BackendState>,
}

pub(crate) fn default_version() -> u32 {
    STATE_VERSION
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            paused: false,
            updated_ms: 0,
            seen_items: Vec::new(),
            last_published_hash: None,
            skipped_oversize: 0,
            skipped_ignored: 0,
            backends: BTreeMap::new(),
        }
    }
}

impl DaemonState {
    /// 读取状态；文件不存在或版本不符时返回默认值（状态是可重建的派生数据）。
    pub fn load(paths: &Paths) -> Result<Self, CoreError> {
        match fsutil::read_to_string_optional(&paths.state_file())? {
            Some(text) => match serde_json::from_str::<Self>(&text) {
                Ok(state) if state.v == STATE_VERSION => Ok(state.with_capacity(SEEN_CAPACITY)),
                Ok(state) => {
                    log::warn!("state.json 版本 {} 不受支持，改用默认状态", state.v);
                    Ok(Self::default())
                }
                Err(err) => {
                    // 状态文件损坏不值得阻断启动：丢弃它只是多做一次去重
                    log::warn!("state.json 解析失败（{err}），改用默认状态");
                    Ok(Self::default())
                }
            },
            None => Ok(Self::default()),
        }
    }

    /// 落盘。
    pub fn save(&self, paths: &Paths) -> Result<(), CoreError> {
        let mut snapshot = self.clone();
        snapshot.updated_ms = crate::device::now_ms();
        let text = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| CoreError::malformed("state.json", e.to_string()))?;
        let path = paths.state_file();
        fsutil::write_atomic(&path, text.as_bytes())?;
        fsutil::tighten_permissions(&path, false)?;
        Ok(())
    }

    /// 收尾 `seen_items` 到指定容量（只保留最新）。
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        if self.seen_items.len() > capacity {
            let drop_count = self.seen_items.len() - capacity;
            self.seen_items.drain(..drop_count);
        }
        self
    }

    /// 取得（必要时创建）某后端的状态。
    pub fn backend_mut(&mut self, name: &str, enabled: bool) -> &mut BackendState {
        let entry = self
            .backends
            .entry(name.to_string())
            .or_insert_with(|| BackendState {
                enabled,
                ..BackendState::default()
            });
        entry.enabled = enabled;
        entry
    }

    /// 只读取得某后端状态。
    pub fn backend(&self, name: &str) -> Option<&BackendState> {
        self.backends.get(name)
    }

    /// 记录一条已处理的对象名。
    pub fn mark_seen(&mut self, name: &str) {
        if self.seen_items.iter().any(|n| n == name) {
            return;
        }
        self.seen_items.push(name.to_string());
        if self.seen_items.len() > SEEN_CAPACITY {
            let drop_count = self.seen_items.len() - SEEN_CAPACITY;
            self.seen_items.drain(..drop_count);
        }
    }

    /// 是否已处理过。
    pub fn is_seen(&self, name: &str) -> bool {
        self.seen_items.iter().any(|n| n == name)
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 状态往返() {
        let paths = temp_paths("roundtrip");
        let mut state = DaemonState::default();
        state.paused = true;
        state.mark_seen("a.cxi");
        state.last_published_hash = Some("beef".to_string());
        let backend = state.backend_mut("home", true);
        backend.record_success(1_000);
        backend.cursors.insert(
            "dev1".to_string(),
            Cursor {
                etag: Some("\"abc\"".to_string()),
                updated_ms: 900,
                items_ms: 800,
                ..Default::default()
            },
        );
        state.save(&paths).unwrap();

        let loaded = DaemonState::load(&paths).unwrap();
        assert!(loaded.paused);
        assert!(loaded.is_seen("a.cxi"));
        assert_eq!(loaded.last_published_hash.as_deref(), Some("beef"));
        assert_eq!(
            loaded.backend("home").unwrap().cursors["dev1"].items_ms,
            800
        );
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 缺失或损坏的状态回退为默认值() {
        let paths = temp_paths("corrupt");
        std::fs::write(paths.state_file(), b"{ not json").unwrap();
        let state = DaemonState::load(&paths).unwrap();
        assert!(!state.paused);
        assert!(state.seen_items.is_empty());
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 退避随失败次数增长并封顶() {
        let b1 = backoff_ms(1, 0);
        let b2 = backoff_ms(2, 0);
        let b3 = backoff_ms(3, 0);
        assert_eq!(b1, BASE_BACKOFF_MS);
        assert_eq!(b2, BASE_BACKOFF_MS * 2);
        assert_eq!(b3, BASE_BACKOFF_MS * 4);
        assert!(backoff_ms(1, 0) < backoff_ms(2, 0));
        assert_eq!(backoff_ms(30, 0), MAX_BACKOFF_MS, "必须封顶");
        assert_eq!(backoff_ms(0, 0), BASE_BACKOFF_MS, "0 次失败也应有基数");
    }

    #[test]
    fn 抖动在合理范围内() {
        let base = backoff_ms(3, 0);
        for jitter in [0u64, 5, 10, 20] {
            let with_jitter = backoff_ms(3, jitter);
            assert!(with_jitter >= base * 9 / 10 - 1, "抖动不应把退避压得过低");
            assert!(with_jitter <= base * 12 / 10 + 1, "抖动不应把退避抬得过高");
        }
        assert_eq!(backoff_ms(3, 99), backoff_ms(3, 20), "抖动上限 20%");
    }

    #[test]
    fn 失败与成功转换() {
        let mut backend = BackendState::default();
        assert!(backend.is_due(0));

        backend.record_failure(1_000, "网络不可达", 0);
        assert!(!backend.is_due(1_500), "失败后应进入退避");
        assert_eq!(backend.last_error.as_deref(), Some("网络不可达"));
        assert!(backend.is_due(2_000));

        backend.record_failure(2_000, "还是不通", 0);
        assert!(!backend.is_due(3_000), "退避应加倍");
        assert!(backend.is_due(6_000));

        backend.record_success(7_000);
        assert!(backend.is_due(7_000));
        assert!(backend.last_error.is_none());
        assert_eq!(backend.consecutive_failures, 0);
    }

    #[test]
    fn 已处理集合容量受限() {
        let mut state = DaemonState::default();
        for i in 0..SEEN_CAPACITY + 10 {
            state.mark_seen(&format!("item-{i}.cxi"));
        }
        assert_eq!(state.seen_items.len(), SEEN_CAPACITY);
        assert!(!state.is_seen("item-0.cxi"), "最旧的应被丢弃");
        assert!(state.is_seen(&format!("item-{}.cxi", SEEN_CAPACITY + 9)));
    }

    #[test]
    fn 重复标记不增长() {
        let mut state = DaemonState::default();
        state.mark_seen("x.cxi");
        state.mark_seen("x.cxi");
        assert_eq!(state.seen_items.len(), 1);
    }
}
