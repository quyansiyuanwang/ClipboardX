//! 平台目录解析。
//!
//! **刻意不用 `directories` crate**：安装包（MSI 的 `%APPDATA%\ClipboardX`、
//! macOS 的 `~/Library/Application Support/ClipboardX`、deb 里的 systemd 单元
//! `~/.config/clipboardx`、`~/.local/share/clipboardx`）已经写死了这些位置，
//! 目录必须与安装包一致，而不是与某个 crate 的约定一致。手工解析还能让测试
//! 用 `from_root` 完全隔离文件系统。

use crate::error::CoreError;
use std::path::{Path, PathBuf};

/// 各文件名的唯一出处。
pub const CONFIG_FILE: &str = "config.toml";
/// 设备身份文件。
pub const IDENTITY_FILE: &str = "identity.json";
/// 本机空间密钥文件（0600）。
pub const SPACE_KEY_FILE: &str = "space.key";
/// 空间元数据本地缓存（与对象存储里的 `cx-space.json` 同构）。
pub const SPACE_FILE: &str = "space.json";
/// 守护进程状态（游标、暂停、计数）。
pub const STATE_FILE: &str = "state.json";
/// 控制套接字令牌（0600）。
pub const CONTROL_TOKEN_FILE: &str = "control.token";
/// 守护进程运行信息（端口、pid）。
pub const DAEMON_INFO_FILE: &str = "daemon.json";
/// 单实例锁。
pub const LOCK_FILE: &str = "daemon.lock";
/// 历史目录。
pub const HISTORY_DIR: &str = "history";
/// 历史追加日志。
pub const HISTORY_LOG_FILE: &str = "items.log";
/// 历史索引。
pub const HISTORY_INDEX_FILE: &str = "index.jsonl";
/// 日志目录。
pub const LOG_DIR: &str = "logs";
/// 守护进程日志。
pub const DAEMON_LOG_FILE: &str = "daemon.log";
/// 后端凭据目录。
pub const SECRETS_DIR: &str = "secrets";

/// 解析后的目录集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// 配置目录（config.toml、identity.json、space.key）。
    pub config_dir: PathBuf,
    /// 数据目录（历史、状态、锁）。
    pub data_dir: PathBuf,
    /// 缓存目录（日志等可丢弃数据）。
    pub cache_dir: PathBuf,
}

impl Paths {
    /// 按平台解析默认目录，允许用环境变量覆盖（`CX_CONFIG_DIR` / `CX_DATA_DIR` / `CX_CACHE_DIR`）。
    pub fn resolve() -> Result<Self, CoreError> {
        if let (Ok(config), Ok(data), Ok(cache)) = (
            std::env::var("CX_CONFIG_DIR"),
            std::env::var("CX_DATA_DIR"),
            std::env::var("CX_CACHE_DIR"),
        ) {
            return Ok(Self {
                config_dir: PathBuf::from(config),
                data_dir: PathBuf::from(data),
                cache_dir: PathBuf::from(cache),
            });
        }

        let (config_dir, data_dir, cache_dir) = platform_dirs()?;
        Ok(Self {
            config_dir: env_override("CX_CONFIG_DIR").unwrap_or(config_dir),
            data_dir: env_override("CX_DATA_DIR").unwrap_or(data_dir),
            cache_dir: env_override("CX_CACHE_DIR").unwrap_or(cache_dir),
        })
    }

    /// 所有目录都放在 `root` 下（测试与便携模式用）。
    pub fn from_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
        }
    }

    /// 创建目录并收紧权限（Unix 下 0700）。
    pub fn ensure(&self) -> Result<(), CoreError> {
        for dir in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::io(dir, e))?;
            crate::fsutil::tighten_permissions(dir, true)?;
        }
        for dir in [self.history_dir(), self.log_dir(), self.secrets_dir()] {
            std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
            crate::fsutil::tighten_permissions(&dir, true)?;
        }
        Ok(())
    }

    /// `config.toml`。
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    /// `identity.json`。
    pub fn identity_file(&self) -> PathBuf {
        self.config_dir.join(IDENTITY_FILE)
    }

    /// `space.key`。
    pub fn space_key_file(&self) -> PathBuf {
        self.config_dir.join(SPACE_KEY_FILE)
    }

    /// `space.json`。
    pub fn space_file(&self) -> PathBuf {
        self.config_dir.join(SPACE_FILE)
    }

    /// `state.json`。
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join(STATE_FILE)
    }

    /// `control.token`。
    pub fn control_token_file(&self) -> PathBuf {
        self.data_dir.join(CONTROL_TOKEN_FILE)
    }

    /// `daemon.json`。
    pub fn daemon_info_file(&self) -> PathBuf {
        self.data_dir.join(DAEMON_INFO_FILE)
    }

    /// `daemon.lock`。
    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join(LOCK_FILE)
    }

    /// 历史目录。
    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join(HISTORY_DIR)
    }

    /// 历史追加日志。
    pub fn history_log(&self) -> PathBuf {
        self.history_dir().join(HISTORY_LOG_FILE)
    }

    /// 历史索引。
    pub fn history_index(&self) -> PathBuf {
        self.history_dir().join(HISTORY_INDEX_FILE)
    }

    /// 日志目录。
    pub fn log_dir(&self) -> PathBuf {
        self.cache_dir.join(LOG_DIR)
    }

    /// 守护进程日志。
    pub fn daemon_log(&self) -> PathBuf {
        self.log_dir().join(DAEMON_LOG_FILE)
    }

    /// 后端凭据目录。
    pub fn secrets_dir(&self) -> PathBuf {
        self.config_dir.join(SECRETS_DIR)
    }

    /// 某个后端的凭据/令牌文件路径。
    pub fn secret_file(&self, name: &str) -> PathBuf {
        self.secrets_dir().join(name)
    }
}

fn env_override(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn platform_dirs() -> Result<(PathBuf, PathBuf, PathBuf), CoreError> {
    let appdata = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or(CoreError::NoDirectory("APPDATA"))?;
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or(CoreError::NoDirectory("LOCALAPPDATA"))?;
    Ok((
        appdata.join("ClipboardX"),
        appdata.join("ClipboardX"),
        local.join("ClipboardX").join("cache"),
    ))
}

#[cfg(target_os = "macos")]
fn platform_dirs() -> Result<(PathBuf, PathBuf, PathBuf), CoreError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(CoreError::NoDirectory("HOME"))?;
    let support = home
        .join("Library")
        .join("Application Support")
        .join("ClipboardX");
    Ok((
        support.clone(),
        support,
        home.join("Library").join("Caches").join("ClipboardX"),
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_dirs() -> Result<(PathBuf, PathBuf, PathBuf), CoreError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(CoreError::NoDirectory("HOME"))?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local").join("share"));
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    Ok((
        config.join("clipboardx"),
        data.join("clipboardx"),
        cache.join("clipboardx"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_root_布局固定() {
        let paths = Paths::from_root("/tmp/cx-test");
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/tmp/cx-test/config/config.toml")
        );
        assert_eq!(
            paths.identity_file(),
            PathBuf::from("/tmp/cx-test/config/identity.json")
        );
        assert_eq!(
            paths.space_key_file(),
            PathBuf::from("/tmp/cx-test/config/space.key")
        );
        assert_eq!(
            paths.state_file(),
            PathBuf::from("/tmp/cx-test/data/state.json")
        );
        assert_eq!(
            paths.history_log(),
            PathBuf::from("/tmp/cx-test/data/history/items.log")
        );
        assert_eq!(
            paths.history_index(),
            PathBuf::from("/tmp/cx-test/data/history/index.jsonl")
        );
        assert_eq!(
            paths.daemon_log(),
            PathBuf::from("/tmp/cx-test/cache/logs/daemon.log")
        );
    }

    #[test]
    fn ensure_创建全部目录() {
        let root = std::env::temp_dir().join(format!("cx-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        for dir in [
            paths.config_dir.clone(),
            paths.data_dir.clone(),
            paths.cache_dir.clone(),
            paths.history_dir(),
            paths.log_dir(),
            paths.secrets_dir(),
        ] {
            assert!(dir.is_dir(), "应创建目录 {dir:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 平台默认目录可控() {
        // 设置环境变量后必须生效（systemd 单元与测试都依赖这个能力）
        unsafe {
            std::env::set_var("CX_CONFIG_DIR", "/tmp/cx-cfg");
            std::env::set_var("CX_DATA_DIR", "/tmp/cx-data");
            std::env::set_var("CX_CACHE_DIR", "/tmp/cx-cache");
        }
        let paths = Paths::resolve().unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/tmp/cx-cfg"));
        assert_eq!(paths.data_dir, PathBuf::from("/tmp/cx-data"));
        assert_eq!(paths.cache_dir, PathBuf::from("/tmp/cx-cache"));
        unsafe {
            std::env::remove_var("CX_CONFIG_DIR");
            std::env::remove_var("CX_DATA_DIR");
            std::env::remove_var("CX_CACHE_DIR");
        }
    }
}
