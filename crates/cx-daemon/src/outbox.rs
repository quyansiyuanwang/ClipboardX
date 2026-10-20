//! 每后端持久发件箱：先落盘后唤醒 worker，网络/队列/进程失败都不丢发布。
use crate::error::DaemonError;
use cx_core::{Paths, fsutil};
use cx_proto::ManifestEntry;
use std::path::{Path, PathBuf};

pub(crate) fn bound_directory(
    paths: &Paths,
    backend: &cx_core::BackendSection,
    space: Option<&cx_core::LocalSpace>,
) -> Result<PathBuf, DaemonError> {
    let key_fingerprint = space
        .and_then(|space| space.cipher.space_key())
        .map(|key| hex::encode(cx_proto::content_hash(key.to_hex().as_bytes())));
    let binding = serde_json::to_vec(&serde_json::json!({
        "name":backend.name,"kind":backend.kind.as_str(),"url":backend.url,
        "endpoint":backend.endpoint,"bucket":backend.bucket,"prefix":backend.prefix,
        "region":backend.region,"path_style":backend.path_style,"remote_space":backend.space,
        "space_id":space.map(|space| &space.meta.space_id),"key_fingerprint":key_fingerprint,
    }))
    .map_err(|err| DaemonError::Protocol(err.to_string()))?;
    Ok(directory(
        paths,
        &hex::encode(cx_proto::content_hash(&binding)),
    ))
}

pub(crate) fn directory(paths: &Paths, backend: &str) -> PathBuf {
    let id = hex::encode(cx_proto::content_hash(backend.as_bytes()));
    paths.data_dir.join("outbox").join(id)
}

pub(crate) fn enqueue(
    dir: &Path,
    name: &str,
    entry: &ManifestEntry,
    bytes: &[u8],
) -> Result<(), DaemonError> {
    cx_proto::ItemName::parse(name).map_err(|err| DaemonError::Protocol(err.to_string()))?;
    std::fs::create_dir_all(dir).map_err(DaemonError::io)?;
    fsutil::tighten_permissions(dir, true)?;
    fsutil::write_atomic(&dir.join(name), bytes)?;
    // 元数据最后提交；崩溃时最多留下可覆盖的未提交载荷，不会产生残缺任务。
    let json = serde_json::to_vec(entry).map_err(|err| DaemonError::Protocol(err.to_string()))?;
    fsutil::write_atomic(&dir.join(format!("{name}.json")), &json)?;
    Ok(())
}

pub(crate) fn pending(
    dir: &Path,
    limit: usize,
) -> Result<Vec<(ManifestEntry, Vec<u8>)>, DaemonError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DaemonError::io(err)),
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    let mut jobs = Vec::new();
    for path in paths.into_iter().take(limit) {
        // 并发的 acknowledge 可能刚好在这一瞬间删掉了这个条目：那不是错误，而是
        // "已经发出去了"，跳过即可。早期版本在这里直接返回 Err，一次巧合就会让
        // 整次读取失败——worker 的积压看起来像坏了（并行跑测试时能稳定复现）。
        let Some(raw) = read_if_present(&path)? else {
            continue;
        };
        let entry: ManifestEntry = serde_json::from_slice(&raw)
            .map_err(|_| DaemonError::Protocol("发件箱元数据损坏，已保留文件供恢复".into()))?;
        cx_proto::ItemName::parse(&entry.name)
            .map_err(|_| DaemonError::Protocol("发件箱对象名非法".into()))?;
        if path.file_name().and_then(|n| n.to_str())
            != Some(format!("{}.json", entry.name).as_str())
        {
            return Err(DaemonError::Protocol("发件箱对象身份不匹配".into()));
        }
        // 载荷也可能已被删除（元数据先删、载荷后删），同样视为已完成
        let Some(bytes) = read_if_present(&dir.join(&entry.name))? else {
            continue;
        };
        jobs.push((entry, bytes));
    }
    Ok(jobs)
}

/// 读文件；文件已经不存在时返回 `Ok(None)`。
///
/// Windows 上"删除挂起"的文件会返回「拒绝访问（os error 5）」而不是 NotFound：
/// 文件已被 `remove_file` 标记删除、还有别的句柄没关。对读者来说这与"不存在"等价，
/// 因此按"已经没了"处理——**不能**当成 IO 错误把整次读取带崩。
fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, DaemonError> {
    for attempt in 0..4 {
        match std::fs::read(path) {
            Ok(bytes) => return Ok(Some(bytes)),
            // 已经不在磁盘上：并发的 acknowledge 完成了
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // Windows：remove_file 标记删除后、最后一个句柄关闭前，再打开会得到
            // 「拒绝访问」(os error 5)。这不是"我们没有权限"，而是"它正在被删掉"，
            // 等一两毫秒通常就消失了。续几轮都还不行，就当作已经没有了——
            // 跳过只是"这一轮不发它"，文件还在，下一轮仍会重试，不会丢数据。
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                return Ok(None);
            }
            Err(_) if !path.exists() => return Ok(None),
            Err(err) => return Err(DaemonError::io(err)),
        }
    }
    // 循环里每一条分支都 return/continue，走到这里说明重试全部命中删除竞态
    Ok(None)
}

pub(crate) fn acknowledge(dir: &Path, name: &str) -> Result<(), DaemonError> {
    // 先移除提交标记；删除载荷失败不会导致重复任务，之后可清理孤儿载荷。
    std::fs::remove_file(dir.join(format!("{name}.json"))).map_err(DaemonError::io)?;
    std::fs::remove_file(dir.join(name)).map_err(DaemonError::io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 更换目的地或密钥空间不会复用发件箱() {
        let paths = Paths::from_root(std::env::temp_dir().join("cx-binding-only"));
        let mut config: cx_core::Config = serde_json::from_value(serde_json::json!({"backend":[{"name":"same","kind":"s3","bucket":"a","endpoint":"https://oss-cn-beijing.aliyuncs.com","access_key_id":"fake","secret_access_key_file":"fake.key"}]})).unwrap();
        let backend = &mut config.backends[0];
        let original = bound_directory(&paths, backend, None).unwrap();
        backend.bucket = Some("another".into());
        assert_ne!(original, bound_directory(&paths, backend, None).unwrap());
        backend.bucket = Some("a".into());
        let (meta, cipher) = cx_core::space::SpaceMeta::create(None).unwrap();
        let space = cx_core::LocalSpace { meta, cipher };
        assert_ne!(
            original,
            bound_directory(&paths, backend, Some(&space)).unwrap()
        );
    }

    #[test]
    fn 发件箱重开仍保留未确认任务() {
        let root = std::env::temp_dir().join(format!(
            "cx-outbox-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let paths = Paths::from_root(&root);
        let dir = directory(&paths, "a.b");
        let name = cx_proto::ItemName::build(1_759_299_998_712, &"a".repeat(32), &"b".repeat(64));
        let entry = ManifestEntry {
            name: name.clone(),
            day: "20251001".into(),
            size: 4,
            hash: "b".repeat(64),
            kind: cx_proto::Kind::Text,
            created_ms: 1_759_299_998_712,
            mime: None,
        };
        enqueue(&dir, &name, &entry, b"fake").unwrap();
        assert_eq!(
            pending(&directory(&paths, "a.b"), 64).unwrap()[0].1,
            b"fake"
        );
        assert!(pending(&directory(&paths, "a"), 64).unwrap().is_empty());
        acknowledge(&dir, &name).unwrap();
        assert!(pending(&dir, 64).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn 边读边确认不会让整次读取失败() {
        // 读发件箱与确认（删除）是两条并行路径：列表拿到之后文件被删掉是**正常时序**，
        // 不能因此让整次读取报错——早期版本就是这样，并行跑测试时能稳定复现
        // （cx-daemon 的 worker 回归测试因此偶发失败）。
        let root = std::env::temp_dir().join(format!(
            "cx-outbox-race-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let paths = Paths::from_root(&root);
        let dir = directory(&paths, "race");
        let device = "c".repeat(32);
        let entry_for = |index: u64| {
            let name = cx_proto::ItemName::build(
                1_759_299_998_712 + index,
                &device,
                &format!("{index:064x}"),
            );
            let entry = ManifestEntry {
                name: name.clone(),
                day: "20251001".into(),
                size: 4,
                hash: "d".repeat(64),
                kind: cx_proto::Kind::Text,
                created_ms: 1_759_299_998_712 + index,
                mime: None,
            };
            (name, entry)
        };

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_dir = dir.clone();
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut seen = 0usize;
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let jobs = pending(&reader_dir, 64)
                    .unwrap_or_else(|err: DaemonError| panic!("并发确认不该让读取失败：{err}"));
                seen += jobs.len();
            }
            seen
        });
        for index in 0..200u64 {
            let (name, entry) = entry_for(index);
            enqueue(&dir, &name, &entry, b"fake").unwrap();
            let _ = pending(&dir, 64).unwrap();
            acknowledge(&dir, &name).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
        assert!(pending(&dir, 64).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
