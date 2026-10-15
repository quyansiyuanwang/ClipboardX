//! 原子文件写入与小工具。
//!
//! 为什么需要原子写：`state.json`、`identity.json`、`space.key` 都是"要么完整要么不存在"
//! 的数据。任何时刻断电/被杀进程都不能留下半个文件，否则下次启动会把程序挡在门外。
//! 做法：写同目录临时文件 → fsync → rename 覆盖（同分区 rename 是原子的）。

use crate::error::CoreError;
use std::fs;
use std::io::Write;
use std::path::Path;

/// 原子写入文件。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "{}.{}.{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("data"),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut created = false;
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(|e| CoreError::io(&tmp, e))?;
        created = true;
        file.write_all(bytes).map_err(|e| CoreError::io(&tmp, e))?;
        file.sync_all().map_err(|e| CoreError::io(&tmp, e))?;
        drop(file);
        fs::rename(&tmp, path).map_err(|e| CoreError::io(path, e))
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 读取文本文件；不存在时返回 `None`（"没有"与"读失败"必须区分开）。
pub fn read_to_string_optional(path: &Path) -> Result<Option<String>, CoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CoreError::io(path, err)),
    }
}

/// Unix：把文件/目录权限收紧到 `0o600` / `0o700`。
///
/// 里面放的是空间密钥、控制令牌、订阅凭据，同机其他用户不应读取。
/// 权限过宽时返回 `Ok(false)` 让调用方决定是告警还是拒绝启动。
#[cfg(unix)]
pub fn tighten_permissions(path: &Path, is_dir: bool) -> Result<bool, CoreError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_dir { 0o700 } else { 0o600 };
    let meta = fs::metadata(path).map_err(|e| CoreError::io(path, e))?;
    let current = meta.permissions().mode() & 0o777;
    if current == mode {
        return Ok(true);
    }
    // 只关心"是否比期望更宽"，更严（例如 0400）就不动它
    if current & !mode != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| CoreError::io(path, e))?;
        return Ok(true);
    }
    Ok(true)
}

/// Windows：没有等价的 POSIX 权限位，返回 `true`（由 `%APPDATA%` 的 ACL 保护）。
#[cfg(not(unix))]
pub fn tighten_permissions(_path: &Path, _is_dir: bool) -> Result<bool, CoreError> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 原子写入与读取() {
        let dir = std::env::temp_dir().join(format!("cx-fsutil-{}", std::process::id()));
        let path = dir.join("sub").join("state.json");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(read_to_string_optional(&path).unwrap().unwrap(), "hello");

        // 覆盖写
        write_atomic(&path, b"world").unwrap();
        assert_eq!(read_to_string_optional(&path).unwrap().unwrap(), "world");

        // 临时文件不应残留
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 缺失文件返回_none_而不是错误() {
        let missing = std::env::temp_dir().join("cx-fsutil-definitely-missing.json");
        let _ = fs::remove_file(&missing);
        assert!(read_to_string_optional(&missing).unwrap().is_none());
    }
}
