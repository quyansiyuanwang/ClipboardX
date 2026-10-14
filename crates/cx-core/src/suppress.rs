//! 回声抑制表与已处理集合。
//!
//! 这两张表是"防回环/防重复"的核心：
//!
//! - [`TtlSet`]：把远端内容写进本机剪贴板后，watcher 立刻就会触发一次变化。
//!   把那次的哈希放进抑制表（默认 8 条 / 30 秒），命中就直接丢弃，不产生上传。
//! - [`SeenSet`]：已经处理过的 item 对象名（LRU，默认 1024 条）。拉取时用它跳过
//!   已应用过的对象，避免重复写剪贴板、重复入库。
//!
//! 两张表都需要在重启后仍然有效（否则重启会把旧内容再同步一遍），因此
//! `SeenSet` 提供序列化，由 `state.json` 持久化。

use std::collections::{HashSet, VecDeque};

/// 带 TTL 与容量上限的字符串集合。
#[derive(Debug, Clone)]
pub struct TtlSet {
    capacity: usize,
    ttl_ms: u64,
    /// (值, 过期时间)，先进先出用于容量淘汰
    queue: VecDeque<(String, u64)>,
    /// 值 → 当前有效条目的过期时间
    live: std::collections::HashMap<String, u64>,
}

impl TtlSet {
    /// 新建。
    pub fn new(capacity: usize, ttl_ms: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl_ms,
            queue: VecDeque::new(),
            live: std::collections::HashMap::new(),
        }
    }

    /// 插入（重复插入会刷新过期时间）。返回是否为首次插入。
    pub fn insert(&mut self, value: impl Into<String>, now_ms: u64) -> bool {
        let value = value.into();
        let expires_at = now_ms.saturating_add(self.ttl_ms);
        let is_new = !self.contains(&value, now_ms);

        self.live.insert(value.clone(), expires_at);
        self.queue.push_back((value, expires_at));
        self.evict(now_ms);
        is_new
    }

    /// 是否处于抑制状态。
    pub fn contains(&self, value: &str, now_ms: u64) -> bool {
        match self.live.get(value) {
            Some(expires_at) => *expires_at > now_ms,
            None => false,
        }
    }

    /// 当前有效条目数。
    pub fn len(&self, now_ms: u64) -> usize {
        self.live
            .iter()
            .filter(|(_, expires_at)| **expires_at > now_ms)
            .count()
    }

    /// 是否为空。
    pub fn is_empty(&self, now_ms: u64) -> bool {
        self.len(now_ms) == 0
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.queue.clear();
        self.live.clear();
    }

    /// 淘汰过期项与超出容量的最旧项。
    fn evict(&mut self, now_ms: u64) {
        loop {
            let Some((value, expires_at)) = self.queue.front() else {
                break;
            };
            let expired = *expires_at <= now_ms;
            let stale = self.live.get(value) != Some(expires_at);
            if !expired && !stale {
                break;
            }
            // 只有当队列里的这条就是当前有效条目时才从 map 里删除，
            // 否则删掉的是已经被刷新过的较新记录
            if let Some((value, expires_at)) = self.queue.pop_front()
                && self.live.get(&value) == Some(&expires_at)
            {
                self.live.remove(&value);
            }
        }

        while self.live.len() > self.capacity {
            let Some((value, expires_at)) = self.queue.pop_front() else {
                break;
            };
            if self.live.get(&value) == Some(&expires_at) {
                self.live.remove(&value);
            }
        }
    }
}

/// 已处理对象名的 LRU 集合（插入序淘汰最旧）。
#[derive(Debug, Clone)]
pub struct SeenSet {
    capacity: usize,
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl SeenSet {
    /// 新建。
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    /// 由已有序列表重建（持久化后恢复用），超出容量时只保留最后 `capacity` 条。
    pub fn from_names<'a, I: IntoIterator<Item = &'a str>>(capacity: usize, names: I) -> Self {
        let mut set = Self::new(capacity);
        for name in names {
            set.insert(name);
        }
        set
    }

    /// 是否已处理。
    pub fn contains(&self, name: &str) -> bool {
        self.set.contains(name)
    }

    /// 标记为已处理。返回是否为新增。
    pub fn insert(&mut self, name: &str) -> bool {
        if self.set.contains(name) {
            return false;
        }
        self.set.insert(name.to_string());
        self.order.push_back(name.to_string());
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// 按插入序导出（持久化用）。
    pub fn to_names(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 抑制表按_ttl_过期() {
        let mut set = TtlSet::new(8, 1000);
        assert!(set.insert("hash-a", 0));
        assert!(set.contains("hash-a", 0));
        assert!(set.contains("hash-a", 999));
        assert!(!set.contains("hash-a", 1000), "到期后不再抑制");
        assert!(set.insert("hash-a", 1000), "过期后再插入应算新增");
    }

    #[test]
    fn 抑制表按容量淘汰最旧() {
        let mut set = TtlSet::new(3, 60_000);
        for i in 0..3 {
            set.insert(format!("h{i}"), 0);
        }
        set.insert("h3", 0);
        assert!(!set.contains("h0", 0), "最旧的应被淘汰");
        assert!(set.contains("h3", 0));
        assert_eq!(set.len(0), 3);
    }

    #[test]
    fn 抑制表重复插入刷新时间() {
        let mut set = TtlSet::new(4, 1000);
        set.insert("h", 0);
        assert!(!set.insert("h", 500), "重复插入不算新增");
        assert!(set.contains("h", 1400), "过期时间应被刷新到 1500");
        assert!(!set.contains("h", 1600));
    }

    #[test]
    fn 抑制表清空() {
        let mut set = TtlSet::new(4, 1000);
        set.insert("a", 0);
        set.clear();
        assert!(set.is_empty(0));
        assert!(!set.contains("a", 0));
    }

    #[test]
    fn 已处理集合去重与淘汰() {
        let mut seen = SeenSet::new(3);
        assert!(seen.insert("a"));
        assert!(!seen.insert("a"), "重复插入不算新增");
        seen.insert("b");
        seen.insert("c");
        seen.insert("d");
        assert_eq!(seen.len(), 3);
        assert!(!seen.contains("a"), "最旧的应被淘汰");
        assert!(seen.contains("d"));
    }

    #[test]
    fn 已处理集合可持久化重建() {
        let mut seen = SeenSet::new(4);
        seen.insert("a.cxi");
        seen.insert("b.cxi");
        let names = seen.to_names();
        let restored = SeenSet::from_names(4, names.iter().map(String::as_str));
        assert!(restored.contains("a.cxi"));
        assert!(restored.contains("b.cxi"));
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn 容量为零也至少保留一条() {
        let mut seen = SeenSet::new(0);
        seen.insert("a");
        assert_eq!(seen.len(), 1);

        let mut ttl = TtlSet::new(0, 100);
        ttl.insert("x", 0);
        assert_eq!(ttl.len(0), 1);
    }
}
