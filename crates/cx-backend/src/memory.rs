//! 内存后端：测试与本地演示用。
//!
//! 价值在于它让"两台设备经同一个后端同步"可以在**单进程、零网络**的条件下测：
//! `shared()` 克隆出的实例指向同一份存储，于是 `docs/protocol.md` 里的发布/拉取/
//! 去重/回环抑制全流程都能断言，而不需要起 WebDAV 或 S3。

use crate::error::BackendError;
use crate::{PutOutcome, RemoteBackend, RemoteObject};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 一个内存对象。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryObject {
    bytes: Vec<u8>,
    etag: String,
    updated_ms: u64,
}

#[derive(Debug, Default)]
struct MemoryStore {
    objects: BTreeMap<String, MemoryObject>,
    /// 单调递增的"修改序号"，用来生成可比较的 updated_ms
    clock: u64,
}

/// 内存后端。
#[derive(Debug, Clone)]
pub struct MemoryBackend {
    name: String,
    store: Arc<Mutex<MemoryStore>>,
    /// 模拟"服务端不支持 If-None-Match"（降级路径也要能被测到）
    ignore_conditional_put: Arc<AtomicBool>,
    /// 模拟请求计数（断言"ETag 未变就不重复下载"）
    reads: Arc<AtomicU64>,
}

impl MemoryBackend {
    /// 新建。
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            store: Arc::new(Mutex::new(MemoryStore::default())),
            ignore_conditional_put: Arc::new(AtomicBool::new(false)),
            reads: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 共享同一份存储的另一个实例（模拟"另一台设备连同一个后端"）。
    pub fn shared(&self, name: &str) -> Self {
        Self {
            name: name.to_string(),
            store: self.store.clone(),
            ignore_conditional_put: self.ignore_conditional_put.clone(),
            reads: self.reads.clone(),
        }
    }

    /// 直接写入对象（测试预置数据用）。
    pub fn seed(&self, path: &str, bytes: &[u8]) {
        let mut store = self.store.lock().unwrap();
        store.clock += 1;
        let clock = store.clock;
        store.objects.insert(
            path.to_string(),
            MemoryObject {
                etag: etag_for(bytes),
                bytes: bytes.to_vec(),
                updated_ms: clock,
            },
        );
    }

    /// 对象总数。
    pub fn object_count(&self) -> usize {
        self.store.lock().unwrap().objects.len()
    }

    /// 按前缀统计对象数。
    pub fn count_with_prefix(&self, prefix: &str) -> usize {
        self.store
            .lock()
            .unwrap()
            .objects
            .keys()
            .filter(|key| key.starts_with(prefix))
            .count()
    }

    /// 全部路径（断言用）。
    pub fn paths(&self) -> Vec<String> {
        self.store.lock().unwrap().objects.keys().cloned().collect()
    }

    /// 已发生的读取次数（含 GET/PROPFIND）。
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// 模拟服务端忽略 `If-None-Match`。
    pub fn set_ignore_conditional_put(&self, value: bool) {
        self.ignore_conditional_put.store(value, Ordering::Relaxed);
    }

    fn object(&self, path: &str) -> Option<RemoteObject> {
        let store = self.store.lock().unwrap();
        store.objects.get(path).map(|object| RemoteObject {
            path: path.to_string(),
            etag: Some(object.etag.clone()),
            last_modified_ms: Some(object.updated_ms),
            size: Some(object.bytes.len() as u64),
            is_collection: false,
        })
    }
}

impl RemoteBackend for MemoryBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "memory"
    }

    fn describe(&self) -> String {
        format!("内存后端（{} 个对象）", self.object_count())
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<RemoteObject>, BackendError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let store = self.store.lock().unwrap();
        Ok(store
            .objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, object)| RemoteObject {
                path: key.clone(),
                etag: Some(object.etag.clone()),
                last_modified_ms: Some(object.updated_ms),
                size: Some(object.bytes.len() as u64),
                is_collection: false,
            })
            .collect())
    }

    fn head(&mut self, path: &str) -> Result<Option<RemoteObject>, BackendError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.object(path))
    }

    fn get(&mut self, path: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let store = self.store.lock().unwrap();
        Ok(store.objects.get(path).map(|object| object.bytes.clone()))
    }

    fn get_with_meta(
        &mut self,
        path: &str,
    ) -> Result<Option<(Vec<u8>, RemoteObject)>, BackendError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let store = self.store.lock().unwrap();
        Ok(store.objects.get(path).map(|object| {
            (
                object.bytes.clone(),
                RemoteObject {
                    path: path.to_string(),
                    etag: Some(object.etag.clone()),
                    last_modified_ms: Some(object.updated_ms),
                    size: Some(object.bytes.len() as u64),
                    is_collection: false,
                },
            )
        }))
    }

    fn put(
        &mut self,
        path: &str,
        bytes: &[u8],
        only_if_absent: bool,
    ) -> Result<PutOutcome, BackendError> {
        let mut store = self.store.lock().unwrap();
        let exists = store.objects.contains_key(path);
        if exists && only_if_absent && !self.ignore_conditional_put.load(Ordering::Relaxed) {
            return Ok(PutOutcome::AlreadyExists);
        }
        store.clock += 1;
        let clock = store.clock;
        let etag = etag_for(bytes);
        store.objects.insert(
            path.to_string(),
            MemoryObject {
                bytes: bytes.to_vec(),
                etag: etag.clone(),
                updated_ms: clock,
            },
        );
        Ok(if exists {
            PutOutcome::Replaced { etag: Some(etag) }
        } else {
            PutOutcome::Created { etag: Some(etag) }
        })
    }

    fn delete(&mut self, path: &str) -> Result<(), BackendError> {
        let mut store = self.store.lock().unwrap();
        store.objects.remove(path);
        Ok(())
    }
}

/// 内容哈希当作 ETag（与 item 的 plain_hash 同算法，便于人工比对）。
fn etag_for(bytes: &[u8]) -> String {
    format!("\"{}\"", cx_proto::content_hash_hex(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 共享实例看到同一份数据() {
        let a = MemoryBackend::new("a");
        let mut b = a.shared("b");
        a.seed("v1/manifests/dev1.json", b"one");
        assert_eq!(b.object_count_or_list("v1/manifests/").len(), 1);
        assert_eq!(b.get("v1/manifests/dev1.json").unwrap().unwrap(), b"one");
    }

    #[test]
    fn 条件写在已存在时被拒绝() {
        let mut backend = MemoryBackend::new("m");
        assert!(matches!(
            backend.put("v1/items/x", b"a", true).unwrap(),
            PutOutcome::Created { .. }
        ));
        assert_eq!(
            backend.put("v1/items/x", b"b", true).unwrap(),
            PutOutcome::AlreadyExists
        );
        // 内容没变
        assert_eq!(backend.get("v1/items/x").unwrap().unwrap(), b"a");
    }

    #[test]
    fn 模拟不支持条件写时会覆盖() {
        let mut backend = MemoryBackend::new("m");
        backend.put("v1/items/x", b"a", true).unwrap();
        backend.set_ignore_conditional_put(true);
        assert!(matches!(
            backend.put("v1/items/x", b"b", true).unwrap(),
            PutOutcome::Replaced { .. }
        ));
        assert_eq!(backend.get("v1/items/x").unwrap().unwrap(), b"b");
    }

    #[test]
    fn etag_随内容变化() {
        let mut backend = MemoryBackend::new("m");
        backend.put("x", b"a", false).unwrap();
        let first = backend.head("x").unwrap().unwrap().etag;
        backend.put("x", b"b", false).unwrap();
        let second = backend.head("x").unwrap().unwrap().etag;
        assert_ne!(first, second);
        assert!(first.unwrap().starts_with('"'));
    }

    #[test]
    fn 删除与不存在读取() {
        let mut backend = MemoryBackend::new("m");
        backend.put("x", b"a", false).unwrap();
        backend.delete("x").unwrap();
        assert!(backend.get("x").unwrap().is_none());
        assert!(backend.head("x").unwrap().is_none());
        // 删除不存在的对象不算错误（幂等）
        backend.delete("x").unwrap();
    }

    #[test]
    fn 前缀列举与计数() {
        let backend = MemoryBackend::new("m");
        backend.seed("v1/manifests/a.json", b"1");
        backend.seed("v1/manifests/b.json", b"2");
        backend.seed("v1/items/20251001/x.cxi", b"3");
        assert_eq!(backend.count_with_prefix("v1/manifests/"), 2);
        assert_eq!(backend.count_with_prefix("v1/items/"), 1);
        assert_eq!(backend.object_count(), 3);
        assert_eq!(backend.paths().len(), 3);
    }

    /// 测试助手：把 list 的结果抽出来（避免在断言里写 mut 绑定）。
    trait ListExt {
        fn object_count_or_list(&mut self, prefix: &str) -> Vec<RemoteObject>;
    }
    impl ListExt for MemoryBackend {
        fn object_count_or_list(&mut self, prefix: &str) -> Vec<RemoteObject> {
            self.list(prefix).unwrap()
        }
    }
}
