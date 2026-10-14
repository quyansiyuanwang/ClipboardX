//! 本地剪贴板历史：追加日志 + JSON Lines 索引。
//!
//! 为什么不是 SQLite：历史是**只追加、按时间倒序取前 N 条**这种最简单的访问模式，
//! 一个 append-only 日志加一份可重建的索引就够了，不必背上 SQLite 的体积与依赖
//! （体积与内存预算见 workspace 的 release profile 注释）。
//!
//! 落盘格式：
//!
//! ```text
//! items.log  = 帧序列：magic "CXL1" | len:u32le | item 字节（cx-proto 容器）
//! index.jsonl= 每行一个 HistoryEntry（含 offset/len，指向 items.log）
//! ```
//!
//! 关键性质：
//! - **索引可丢弃**：删掉或损坏都能从日志重建，重建时顺便丢弃尾部半截帧（掉电保护）。
//! - **日志可压缩**：只保留最新 N 条重写后原子替换。
//! - **读取不整文件载入**：按 offset 直接 seek 读单条。

use crate::error::CoreError;
use crate::fsutil;
use crate::paths::Paths;
use cx_proto::{Item, Kind};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// 帧魔数。
pub const FRAME_MAGIC: [u8; 4] = *b"CXL1";

/// 帧头长度（魔数 + 长度）。
pub const FRAME_HEADER_LEN: usize = 8;

/// 单帧上限（与 item 容器上限一致，防伪造长度导致巨额分配）。
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// 索引中的一条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 在日志中的字节偏移。
    pub offset: u64,
    /// 帧总长度（含帧头）。
    pub frame_len: u32,
    /// 对象名。
    pub name: String,
    /// 内容类型。
    pub kind: Kind,
    /// 产生时间（毫秒，UTC）。
    pub created_ms: u64,
    /// 内容哈希（十六进制）。
    pub plain_hash: String,
    /// 明文长度。
    pub plain_len: u32,
    /// 来源设备 id。
    pub origin_device: String,
    /// 来源设备名。
    pub origin_name: String,
}

impl HistoryEntry {
    /// 一句话摘要（`cx history` 展示用，截断到 `max_chars`）。
    pub fn summary(&self, max_chars: usize) -> String {
        let kind = self.kind.as_str();
        let hash = self.plain_hash.chars().take(8).collect::<String>();
        let text = format!(
            "[{kind}] {} 字节 来自 {} ({hash})",
            self.plain_len, self.origin_name
        );
        text.chars().take(max_chars).collect()
    }
}

/// 历史存储。
#[derive(Debug)]
pub struct History {
    log_path: PathBuf,
    index_path: PathBuf,
    entries: Vec<HistoryEntry>,
    log_size: u64,
    /// 重建索引时跳过的坏帧数（`cx doctor` 展示）。
    skipped_frames: u64,
}

impl History {
    /// 打开（必要时重建索引）。
    pub fn open(paths: &Paths) -> Result<Self, CoreError> {
        paths.ensure()?;
        let mut history = Self {
            log_path: paths.history_log(),
            index_path: paths.history_index(),
            entries: Vec::new(),
            log_size: 0,
            skipped_frames: 0,
        };
        history.log_size = match std::fs::metadata(&history.log_path) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(CoreError::io(&history.log_path, err)),
        };

        if !history.load_index()? {
            log::info!("历史索引缺失或与日志不一致，正在重建");
            history.rebuild_index()?;
        }
        Ok(history)
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 重建索引时跳过的坏帧数。
    pub fn skipped_frames(&self) -> u64 {
        self.skipped_frames
    }

    /// 日志字节数。
    pub fn log_size(&self) -> u64 {
        self.log_size
    }

    /// 最新在前地列出条目。
    pub fn newest_first(&self, limit: usize) -> Vec<&HistoryEntry> {
        self.entries.iter().rev().take(limit).collect()
    }

    /// 全部条目（插入序，最旧在前）。
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// 读取"从最新数第 `position` 条"（`position` 从 0 开始）对应的内容。
    pub fn read_newest(&self, position: usize) -> Result<Item, CoreError> {
        let len = self.entries.len();
        if position >= len {
            return Err(CoreError::HistoryCorrupt(format!(
                "历史只有 {len} 条，取不到第 {} 条（从最新数）",
                position + 1
            )));
        }
        let entry = self.entries[len - 1 - position].clone();
        self.read_entry(&entry)
    }

    /// 追加一条内容（`name` 为对象名，`item` 为完整容器）。
    pub fn append(&mut self, name: &str, item: &Item) -> Result<HistoryEntry, CoreError> {
        let bytes = item.encode()?;
        if bytes.len() as u64 > u64::from(MAX_FRAME_LEN) {
            return Err(CoreError::HistoryCorrupt(format!(
                "内容过大：{} 字节",
                bytes.len()
            )));
        }

        let offset = self.log_size;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| CoreError::io(&self.log_path, e))?;

        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + bytes.len());
        frame.extend_from_slice(&FRAME_MAGIC);
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&bytes);

        file.write_all(&frame)
            .map_err(|e| CoreError::io(&self.log_path, e))?;
        file.flush().map_err(|e| CoreError::io(&self.log_path, e))?;
        drop(file);

        self.log_size += frame.len() as u64;

        let entry = HistoryEntry {
            offset,
            frame_len: frame.len() as u32,
            name: name.to_string(),
            kind: item.kind,
            created_ms: item.meta.created_ms,
            plain_hash: item.hash_hex(),
            plain_len: item.plain_len(),
            origin_device: item.meta.origin_device.clone(),
            origin_name: item.meta.origin_name.clone(),
        };
        self.append_index_line(&entry)?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// 是否需要压缩（条目数超过保留上限的若干倍）。
    pub fn needs_compaction(&self, max_items: usize) -> bool {
        max_items > 0 && self.entries.len() > max_items.saturating_mul(4)
    }

    /// 压缩：只保留最新 `keep` 条，重写日志与索引（原子替换）。
    ///
    /// 返回保留下来的条目数。
    pub fn compact(&mut self, keep: usize) -> Result<usize, CoreError> {
        let keep = keep.max(1);
        let start = self.entries.len().saturating_sub(keep);
        let kept: Vec<HistoryEntry> = self.entries[start..].to_vec();

        let tmp_log = self.log_path.with_extension("log.compact");
        let mut out = std::fs::File::create(&tmp_log).map_err(|e| CoreError::io(&tmp_log, e))?;
        let mut new_entries = Vec::with_capacity(kept.len());
        let mut offset = 0u64;

        for entry in &kept {
            let item = self.read_entry(entry)?;
            let bytes = item.encode()?;
            let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + bytes.len());
            frame.extend_from_slice(&FRAME_MAGIC);
            frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            frame.extend_from_slice(&bytes);
            out.write_all(&frame)
                .map_err(|e| CoreError::io(&tmp_log, e))?;

            let mut updated = entry.clone();
            updated.offset = offset;
            updated.frame_len = frame.len() as u32;
            offset += frame.len() as u64;
            new_entries.push(updated);
        }
        out.flush().map_err(|e| CoreError::io(&tmp_log, e))?;
        out.sync_all().map_err(|e| CoreError::io(&tmp_log, e))?;
        drop(out);

        std::fs::rename(&tmp_log, &self.log_path).map_err(|e| CoreError::io(&self.log_path, e))?;
        self.log_size = offset;
        self.entries = new_entries;
        self.write_index()?;
        Ok(self.entries.len())
    }

    /// 读取一条完整 item。
    pub fn read_entry(&self, entry: &HistoryEntry) -> Result<Item, CoreError> {
        let mut file =
            std::fs::File::open(&self.log_path).map_err(|e| CoreError::io(&self.log_path, e))?;
        file.seek(SeekFrom::Start(entry.offset))
            .map_err(|e| CoreError::io(&self.log_path, e))?;

        let mut header = [0u8; FRAME_HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|e| CoreError::io(&self.log_path, e))?;
        if header[0..4] != FRAME_MAGIC {
            return Err(CoreError::HistoryCorrupt(format!(
                "偏移 {} 处缺少帧魔数",
                entry.offset
            )));
        }
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if len > MAX_FRAME_LEN {
            return Err(CoreError::HistoryCorrupt(format!("帧长度异常：{len}")));
        }
        let mut payload = vec![0u8; len as usize];
        file.read_exact(&mut payload)
            .map_err(|e| CoreError::io(&self.log_path, e))?;
        Ok(Item::decode(&payload)?)
    }

    /// 尝试从索引文件恢复；不一致时返回 `false` 交给调用方重建。
    fn load_index(&mut self) -> Result<bool, CoreError> {
        let Some(text) = fsutil::read_to_string_optional(&self.index_path)? else {
            return Ok(self.entries.is_empty() && self.log_size == 0);
        };

        let mut entries = Vec::new();
        for (line_no, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    log::warn!("索引第 {} 行损坏（{err}），将重建", line_no + 1);
                    return Ok(false);
                }
            }
        }

        // 一致性检查：每条帧必须落在日志内，且长度自洽
        for entry in &entries {
            let end = entry.offset as u128 + entry.frame_len as u128;
            if entry.frame_len < FRAME_HEADER_LEN as u32 || end > self.log_size as u128 {
                return Ok(false);
            }
        }
        // 最后一条必须正好到日志末尾（否则说明有未索引的追加或截断）
        if let Some(last) = entries.last() {
            if last.offset + last.frame_len as u64 != self.log_size {
                return Ok(false);
            }
        } else if self.log_size != 0 {
            return Ok(false);
        }

        self.entries = entries;
        Ok(true)
    }

    /// 扫描日志重建索引，并丢弃尾部损坏的半截帧。
    pub fn rebuild_index(&mut self) -> Result<usize, CoreError> {
        let mut entries = Vec::new();
        let mut skipped = 0u64;

        if self.log_size > 0 {
            let mut file = std::fs::File::open(&self.log_path)
                .map_err(|e| CoreError::io(&self.log_path, e))?;
            let mut offset = 0u64;

            loop {
                if offset >= self.log_size {
                    break;
                }
                let mut header = [0u8; FRAME_HEADER_LEN];
                match file.read_exact(&mut header) {
                    Ok(()) => {}
                    Err(_) => break, // 尾部半截帧
                }
                if header[0..4] != FRAME_MAGIC {
                    skipped += 1;
                    log::warn!("偏移 {offset} 处帧魔数不对，停止扫描并截断");
                    break;
                }
                let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
                if len > MAX_FRAME_LEN {
                    skipped += 1;
                    log::warn!("偏移 {offset} 处帧长度 {len} 异常，停止扫描并截断");
                    break;
                }
                let mut payload = vec![0u8; len as usize];
                if file.read_exact(&mut payload).is_err() {
                    skipped += 1;
                    log::warn!("偏移 {offset} 处帧不完整，停止扫描并截断");
                    break;
                }

                match Item::decode(&payload) {
                    Ok(item) => {
                        entries.push(HistoryEntry {
                            offset,
                            frame_len: (FRAME_HEADER_LEN + len as usize) as u32,
                            name: String::new(), // 日志里没有对象名，由调用方按需补齐
                            kind: item.kind,
                            created_ms: item.meta.created_ms,
                            plain_hash: item.hash_hex(),
                            plain_len: item.plain_len(),
                            origin_device: item.meta.origin_device.clone(),
                            origin_name: item.meta.origin_name.clone(),
                        });
                        offset += (FRAME_HEADER_LEN + len as usize) as u64;
                    }
                    Err(err) => {
                        skipped += 1;
                        log::warn!("偏移 {offset} 处帧无法解码（{err}），停止扫描并截断");
                        break;
                    }
                }
            }

            // 丢弃尾部坏帧：把日志截断到最后一条完整帧的末尾
            let valid_size = entries
                .last()
                .map(|e| e.offset + e.frame_len as u64)
                .unwrap_or(0);
            if valid_size != self.log_size {
                log::warn!(
                    "历史日志从 {} 截断到 {} 字节（丢弃尾部损坏数据）",
                    self.log_size,
                    valid_size
                );
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.log_path)
                    .map_err(|e| CoreError::io(&self.log_path, e))?;
                file.set_len(valid_size)
                    .map_err(|e| CoreError::io(&self.log_path, e))?;
                self.log_size = valid_size;
            }
        }

        self.entries = entries;
        self.skipped_frames = skipped;
        self.write_index()?;
        Ok(self.entries.len())
    }

    /// 追加一行索引。
    fn append_index_line(&self, entry: &HistoryEntry) -> Result<(), CoreError> {
        let line = serde_json::to_string(entry)
            .map_err(|e| CoreError::malformed("index.jsonl", e.to_string()))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.index_path)
            .map_err(|e| CoreError::io(&self.index_path, e))?;
        file.write_all(line.as_bytes())
            .map_err(|e| CoreError::io(&self.index_path, e))?;
        file.write_all(b"\n")
            .map_err(|e| CoreError::io(&self.index_path, e))?;
        file.flush()
            .map_err(|e| CoreError::io(&self.index_path, e))?;
        Ok(())
    }

    /// 原子重写整个索引。
    fn write_index(&self) -> Result<(), CoreError> {
        let mut text = String::new();
        for entry in &self.entries {
            let line = serde_json::to_string(entry)
                .map_err(|e| CoreError::malformed("index.jsonl", e.to_string()))?;
            text.push_str(&line);
            text.push('\n');
        }
        fsutil::write_atomic(&self.index_path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cx_proto::{Cipher, ItemMeta};

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-history-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    fn make_item(plain: &str, created_ms: u64) -> Item {
        let bytes = plain.as_bytes();
        let meta = ItemMeta {
            id: vec![1u8; cx_proto::ID_LEN],
            origin_device: "ab12cd34ef5678901234567890abcdef".to_string(),
            origin_name: "ws".to_string(),
            created_ms,
            seq: created_ms,
            kind: Kind::Text,
            mime: Some("text/plain; charset=utf-8".to_string()),
            plain_hash: cx_proto::content_hash(bytes).to_vec(),
            plain_len: bytes.len() as u32,
            nonce: Vec::new(),
        };
        Item::new(Kind::Text, Cipher::None, meta, bytes.to_vec()).unwrap()
    }

    fn make_name(created_ms: u64, hash_short: &str) -> String {
        format!("{created_ms:013}-ab12cd34-{hash_short}.cxi")
    }

    #[test]
    fn 追加与读取往返() {
        let paths = temp_paths("append");
        let mut history = History::open(&paths).unwrap();
        assert!(history.is_empty());

        let item = make_item("第一条", 1_000);
        let entry = history
            .append(&make_name(1_000, "11111111"), &item)
            .unwrap();
        assert_eq!(entry.plain_len, "第一条".len() as u32);
        assert_eq!(history.len(), 1);

        let read = history.read_newest(0).unwrap();
        assert_eq!(read, item);
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 最新在前排序() {
        let paths = temp_paths("order");
        let mut history = History::open(&paths).unwrap();
        for i in 1..=5u64 {
            history
                .append(
                    &make_name(i * 1000, &format!("{i:08x}")),
                    &make_item(&format!("第{i}条"), i * 1000),
                )
                .unwrap();
        }
        let listed = history.newest_first(3);
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].created_ms, 5_000);
        assert_eq!(listed[2].created_ms, 3_000);

        assert_eq!(
            history.read_newest(0).unwrap().plain_len(),
            "第5条".len() as u32
        );
        assert_eq!(
            history.read_newest(4).unwrap().plain_len(),
            "第1条".len() as u32
        );
        assert!(history.read_newest(5).is_err(), "越界应报错");
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 重开进程后可继续追加并读取() {
        let paths = temp_paths("reopen");
        {
            let mut history = History::open(&paths).unwrap();
            history
                .append(&make_name(1_000, "aaaa1111"), &make_item("旧", 1_000))
                .unwrap();
        }
        {
            let mut history = History::open(&paths).unwrap();
            assert_eq!(history.len(), 1, "索引应被复用");
            history
                .append(&make_name(2_000, "bbbb2222"), &make_item("新", 2_000))
                .unwrap();
            assert_eq!(history.len(), 2);
            assert_eq!(
                history.read_newest(0).unwrap().plain_len(),
                "新".len() as u32
            );
        }
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 索引缺失时自动重建() {
        let paths = temp_paths("rebuild");
        {
            let mut history = History::open(&paths).unwrap();
            for i in 1..=3u64 {
                history
                    .append(
                        &make_name(i * 1000, &format!("{i:08x}")),
                        &make_item(&format!("x{i}"), i * 1000),
                    )
                    .unwrap();
            }
        }
        std::fs::remove_file(paths.history_index()).unwrap();

        let history = History::open(&paths).unwrap();
        assert_eq!(history.len(), 3, "应从日志重建出 3 条");
        assert_eq!(
            history.read_newest(0).unwrap().plain_len(),
            "x3".len() as u32
        );
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 索引损坏时自动重建() {
        let paths = temp_paths("badindex");
        {
            let mut history = History::open(&paths).unwrap();
            history
                .append(&make_name(1_000, "aaaa1111"), &make_item("a", 1_000))
                .unwrap();
        }
        std::fs::write(paths.history_index(), "{这不是 json}\n".as_bytes()).unwrap();

        let history = History::open(&paths).unwrap();
        assert_eq!(history.len(), 1);
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 尾部半截帧被截断且旧数据保留() {
        let paths = temp_paths("torn");
        {
            let mut history = History::open(&paths).unwrap();
            history
                .append(&make_name(1_000, "aaaa1111"), &make_item("完整", 1_000))
                .unwrap();
        }
        // 模拟掉电：追加一段不完整的帧
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(paths.history_log())
                .unwrap();
            file.write_all(&FRAME_MAGIC).unwrap();
            file.write_all(&9999u32.to_le_bytes()).unwrap();
            file.write_all(b"only a few bytes").unwrap();
        }

        let history = History::open(&paths).unwrap();
        assert_eq!(history.len(), 1, "完整的那条应保留");
        assert_eq!(
            history.read_newest(0).unwrap().plain_len(),
            "完整".len() as u32
        );
        assert!(history.skipped_frames() >= 1);
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 压缩只保留最新若干条() {
        let paths = temp_paths("compact");
        let mut history = History::open(&paths).unwrap();
        for i in 1..=10u64 {
            history
                .append(
                    &make_name(i * 1000, &format!("{i:08x}")),
                    &make_item(&format!("条目{i}"), i * 1000),
                )
                .unwrap();
        }
        let before = history.log_size();
        let kept = history.compact(3).unwrap();
        assert_eq!(kept, 3);
        assert_eq!(history.len(), 3);
        assert!(history.log_size() < before, "压缩后日志应更小");
        assert_eq!(
            history.read_newest(0).unwrap().plain_len(),
            "条目10".len() as u32
        );
        assert_eq!(
            history.read_newest(2).unwrap().plain_len(),
            "条目8".len() as u32
        );

        // 重开一次，索引与日志仍一致
        let reopened = History::open(&paths).unwrap();
        assert_eq!(reopened.len(), 3);
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 压缩触发条件() {
        let paths = temp_paths("needcompact");
        let mut history = History::open(&paths).unwrap();
        assert!(!history.needs_compaction(10));
        for i in 1..=41u64 {
            history
                .append(
                    &make_name(i * 1000, &format!("{i:08x}")),
                    &make_item("x", i * 1000),
                )
                .unwrap();
        }
        assert!(history.needs_compaction(10), "超过 4 倍上限应触发压缩");
        assert!(!history.needs_compaction(0), "0 表示不限制");
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 摘要截断() {
        let paths = temp_paths("summary");
        let mut history = History::open(&paths).unwrap();
        let entry = history
            .append(&make_name(1_000, "aaaa1111"), &make_item("x", 1_000))
            .unwrap();
        let summary = entry.summary(12);
        assert!(summary.chars().count() <= 12);
        assert!(summary.contains("text"));
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn 日志被截断到零也能打开() {
        let paths = temp_paths("empty");
        std::fs::write(paths.history_log(), b"").unwrap();
        let history = History::open(&paths).unwrap();
        assert!(history.is_empty());
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }
}
