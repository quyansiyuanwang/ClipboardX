//! 同步引擎。
//!
//! 单线程状态机：唯一持有可变状态（seen 集合、抑制表、各后端游标、历史索引），
//! 串行处理来自剪贴板 watcher、各后端 worker 与控制套接字的输入事件。

/// 抑制表默认容量（条）。
pub const DEFAULT_SUPPRESS_CAPACITY: usize = 8;

/// 抑制表条目存活时间（毫秒）——远端内容写入本机剪贴板后，这段时间内的
/// watcher 事件被视为回声而丢弃。
pub const DEFAULT_SUPPRESS_TTL_MS: u64 = 30_000;
