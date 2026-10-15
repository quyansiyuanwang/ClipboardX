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
        let raw = std::fs::read(&path).map_err(DaemonError::io)?;
        let entry: ManifestEntry = serde_json::from_slice(&raw)
            .map_err(|_| DaemonError::Protocol("发件箱元数据损坏，已保留文件供恢复".into()))?;
        cx_proto::ItemName::parse(&entry.name)
            .map_err(|_| DaemonError::Protocol("发件箱对象名非法".into()))?;
        if path.file_name().and_then(|n| n.to_str())
            != Some(format!("{}.json", entry.name).as_str())
        {
            return Err(DaemonError::Protocol("发件箱对象身份不匹配".into()));
        }
        let bytes = std::fs::read(dir.join(&entry.name)).map_err(DaemonError::io)?;
        jobs.push((entry, bytes));
    }
    Ok(jobs)
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
}
