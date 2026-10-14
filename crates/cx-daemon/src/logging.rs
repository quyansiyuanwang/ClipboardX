//! 最小日志实现：`log` facade → stderr + 按大小轮转的文件。
//!
//! 为什么不用 `tracing-subscriber`：它连同 `env-filter` 会带来可观体积，而本项目
//! 只需要"分级 + 时间戳 + 轮转文件"三件事。这里 80 行就够，且轮转逻辑完全可控。
//!
//! 轮转策略：`daemon.log` 超过 `max_bytes` 时改名为 `daemon.log.1`（覆盖旧文件）后重开。
//! 只保留一代，因为守护进程的日志只用于排障，历史价值低。

use crate::error::DaemonError;
use cx_core::Paths;
use log::{Level, LevelFilter, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// 单个日志文件的大小上限。
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// 日志文件与已写入字节数（放在同一个锁里，保证"写 + 计数 + 轮转"是原子的）。
struct FileState {
    file: File,
    written: u64,
}

/// 日志器。
struct FileLogger {
    level: LevelFilter,
    state: Mutex<FileState>,
    path: PathBuf,
    max_bytes: u64,
    also_stderr: bool,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} {}\n",
            timestamp(),
            record.level().as_str(),
            record.args()
        );

        if self.also_stderr {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }

        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.file.write_all(line.as_bytes()).is_err() {
            return;
        }
        state.written += line.len() as u64;
        let should_rotate = state.written >= self.max_bytes;
        drop(state);

        if should_rotate {
            // 轮转失败不应影响主流程，下一次写入会再试
            let rotated = self.path.with_extension("log.1");
            let _ = std::fs::rename(&self.path, &rotated);
            if let Ok(new_file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                && let Ok(mut state) = self.state.lock()
            {
                state.file = new_file;
                state.written = 0;
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = state.file.flush();
        }
    }
}

/// `2026-10-14T06:46:45.123Z`（UTC，毫秒精度）。
fn timestamp() -> String {
    let ms = cx_core::now_ms();
    let days = ms / 86_400_000;
    let rest = ms % 86_400_000;
    let (year, month, day) = civil_from_days(days as i64);
    let hours = rest / 3_600_000;
    let minutes = (rest % 3_600_000) / 60_000;
    let seconds = (rest % 60_000) / 1000;
    let millis = rest % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// 天数 → (年, 月, 日)，与 `cx-proto::civil` 同一算法（此处避免跨 crate 依赖）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 初始化全局日志器。
///
/// 级别优先级：显式参数 > `CX_LOG` 环境变量 > 默认 `info`。
pub fn init(
    paths: &Paths,
    level: Option<LevelFilter>,
    also_stderr: bool,
) -> Result<(), DaemonError> {
    let level = level
        .or_else(|| {
            std::env::var("CX_LOG")
                .ok()
                .and_then(|value| value.parse::<LevelFilter>().ok())
        })
        .unwrap_or(LevelFilter::Info);

    std::fs::create_dir_all(paths.log_dir())
        .map_err(|e| DaemonError::Core(cx_core::CoreError::io(paths.log_dir(), e)))?;
    let path = paths.daemon_log();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| DaemonError::Core(cx_core::CoreError::io(&path, e)))?;
    let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);

    let logger = FileLogger {
        level,
        state: Mutex::new(FileState { file, written }),
        path,
        max_bytes: MAX_LOG_BYTES,
        also_stderr,
    };

    // 用 Box::leak 得到 &'static：`set_boxed_logger` 需要 `log` 的 std 特性，
    // 而这里只用一个实例，泄漏一份日志器是安全的固定开销。
    let leaked: &'static FileLogger = Box::leak(Box::new(logger));
    // 重复初始化（例如测试里）时忽略错误
    let _ = log::set_logger(leaked);
    log::set_max_level(level);
    Ok(())
}

/// 记录一条指定级别的日志（`log` 宏的便捷封装，避免各模块引入宏）。
pub fn log_line(level: log::Level, message: &str) {
    match level {
        Level::Error => log::error!("{message}"),
        Level::Warn => log::warn!("{message}"),
        Level::Info => log::info!("{message}"),
        Level::Debug => log::debug!("{message}"),
        Level::Trace => log::trace!("{message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn 时间戳格式正确() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 24, "形如 2026-10-14T06:46:45.123Z：{stamp}");
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
    }

    #[test]
    fn 日期换算与已知值一致() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_362), (2025, 10, 1));
        // 2024-01-01 是第 19723 天，再加 160 天 ⇒ 2024-06-09
        assert_eq!(civil_from_days(19_883), (2024, 6, 9));
    }

    #[test]
    fn 日志写入并轮转() {
        let root = std::env::temp_dir().join(format!("cx-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let path = paths.daemon_log();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let logger = FileLogger {
            level: LevelFilter::Debug,
            state: Mutex::new(FileState { file, written: 0 }),
            path: path.clone(),
            max_bytes: 200, // 故意设小以便触发轮转
            also_stderr: false,
        };

        for i in 0..50 {
            logger.log(
                &Record::builder()
                    .level(Level::Info)
                    .args(format_args!("第 {i} 行日志，填充一些内容"))
                    .build(),
            );
        }
        logger.flush();

        assert!(path.exists());
        assert!(
            path.with_extension("log.1").exists(),
            "超过上限后应轮转出 daemon.log.1"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 级别过滤生效() {
        let root = std::env::temp_dir().join(format!("cx-log-level-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();

        let path = paths.daemon_log();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let logger = FileLogger {
            level: LevelFilter::Warn,
            state: Mutex::new(FileState { file, written: 0 }),
            path: path.clone(),
            max_bytes: 1024,
            also_stderr: false,
        };

        logger.log(
            &Record::builder()
                .level(Level::Debug)
                .args(format_args!("不应出现"))
                .build(),
        );
        logger.log(
            &Record::builder()
                .level(Level::Error)
                .args(format_args!("应出现"))
                .build(),
        );
        logger.flush();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("不应出现"));
        assert!(text.contains("应出现"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
