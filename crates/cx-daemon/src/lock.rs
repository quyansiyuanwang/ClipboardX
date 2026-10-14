//! 单实例锁。
//!
//! 两个守护进程同时读写同一份历史与状态会互相破坏（历史日志是追加写，
//! 状态文件是整体覆盖）。因此启动第一步就是抢锁，抢不到直接退出并告诉用户
//! "已经在运行"——而不是让两个进程互相打架。
//!
//! 用 `fs2` 的**建议性文件锁**：进程崩溃时由操作系统自动释放，不会留下需要
//! 手工清理的死锁文件（这正是 pid 文件方案的常见毛病）。

use crate::error::DaemonError;
use cx_core::Paths;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;

/// 持有锁的守卫；`Drop` 时自动释放。
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// 尝试独占加锁。
    pub fn acquire(paths: &Paths) -> Result<Self, DaemonError> {
        let path = paths.lock_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DaemonError::Core(cx_core::CoreError::io(parent, e)))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| DaemonError::Core(cx_core::CoreError::io(&path, e)))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                // 写入 pid，便于排障（锁本身才是互斥依据）
                let _ = file.set_len(0);
                let _ = writeln!(file, "{}", std::process::id());
                let _ = file.flush();
                Ok(Self { file })
            }
            Err(_) => {
                let holder = std::fs::read_to_string(&path)
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "未知 pid".to_string());
                Err(DaemonError::AlreadyRunning(format!("pid {holder}")))
            }
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 第二次加锁失败并提示持有者() {
        let paths = temp_paths("dup");
        let first = InstanceLock::acquire(&paths).unwrap();
        let err = InstanceLock::acquire(&paths).unwrap_err();
        assert!(matches!(err, DaemonError::AlreadyRunning(_)));
        assert!(
            err.to_string().contains("运行"),
            "错误信息应说明已有实例：{err}"
        );

        drop(first);
        // 释放后可以再次获取
        let _second = InstanceLock::acquire(&paths).unwrap();
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }

    #[test]
    fn pid_被写入锁文件() {
        let paths = temp_paths("pid");
        let lock = InstanceLock::acquire(&paths).unwrap();
        // Windows 上独占锁会阻止其他句柄读取该文件，因此先释放再读
        drop(lock);
        let text = std::fs::read_to_string(paths.lock_file()).unwrap();
        assert_eq!(text.trim(), std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }
}
