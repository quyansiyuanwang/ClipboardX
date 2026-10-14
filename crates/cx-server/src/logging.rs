//! 极简日志：追加写入 `<log_dir>/cx-server.log`，超过大小上限就轮转一次。
//!
//! 为什么不引 `env_logger`/`tracing`：服务端要长期无人值守运行，真正需要的是
//! "写文件 + 别把磁盘写满"，这两件事各十行。轮转由外部工具（logrotate）做更好的话
//! 也没问题，这里只是保证默认情况下不会无限增长。

use crate::error::ServerError;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单个日志文件的轮转阈值（16 MiB）。
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

struct FileLogger {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    stderr: bool,
}

impl Log for FileLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let line = format!("{} {level} {}\n", crate::store::now_ms(), record.args());
        if self.stderr {
            eprint!("{line}");
        }
        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if guard.is_none() {
            *guard = open_log(&self.path).ok();
        }
        if let Some(file) = guard.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
            // 轮转：超过阈值就换一个文件（旧的改成 .1）
            if let Ok(meta) = file.metadata()
                && meta.len() > MAX_LOG_BYTES
            {
                *guard = None;
                let rotated = self.path.with_extension("log.1");
                let _ = std::fs::rename(&self.path, &rotated);
                *guard = open_log(&self.path).ok();
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.flush();
        }
    }
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// 安装全局日志器（重复调用返回 `Ok(())`，不报错）。
pub fn init(log_dir: &Path, stderr: bool) -> Result<(), ServerError> {
    std::fs::create_dir_all(log_dir).map_err(|err| ServerError::io(log_dir, err))?;
    let logger = FileLogger {
        path: log_dir.join("cx-server.log"),
        file: Mutex::new(None),
        stderr,
    };
    // `set_logger` 要 &'static：用 Box::leak 换一个（进程生命周期内本来也不会释放）。
    // 只在第一次成功；重复调用忽略错误即可（测试里会重复调用）。
    let leaked: &'static FileLogger = Box::leak(Box::new(logger));
    if log::set_logger(leaked).is_ok() {
        log::set_max_level(if stderr {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 能建目录并写入() {
        let root = std::env::temp_dir().join(format!(
            "cx-server-log-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        assert!(init(&root, false).is_ok());

        // 直接构造一个 logger 写一行（避免测试之间争抢全局 logger）
        let logger = FileLogger {
            path: root.join("direct.log"),
            file: Mutex::new(None),
            stderr: false,
        };
        let record = Record::builder()
            .args(format_args!("测试日志"))
            .level(Level::Info)
            .build();
        logger.log(&record);
        logger.flush();

        let text = std::fs::read_to_string(root.join("direct.log")).unwrap();
        assert!(text.contains("测试日志"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }
}
