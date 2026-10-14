//! ClipboardX 同步引擎。
//!
//! 单线程状态机：唯一持有可变状态（抑制表、已处理集合、后端游标、历史索引、
//! 设备序号），外部世界的一切以事件传入、以动作返回。因此没有锁，也不需要
//! 线程或定时器就能完整测试。
//!
//! 三层防回环/防重复（详见 `docs/protocol.md` §7）：
//! 1. 剪贴板指纹去重（本机内容没变就不处理）；
//! 2. 抑制表 TTL（把远端内容写进剪贴板后，watcher 立刻触发的那次变化被丢弃）；
//! 3. 已处理集合 + 内容哈希（同名对象不重复入库、同内容不重复上传）。
//!
//! 与远端后端的粘合在 [`remote::RemoteSync`]：它负责把 [`Action::Publish`]
//! 落到对象存储上、并按 manifest 增量把别人的对象取回来。

mod engine;
pub mod remote;

pub use engine::{Action, Command, Engine, Event, ReplyPayload};
pub use remote::{DeviceCursor, PollOutcome, RemoteSync, RetentionLimits, publish_job};

/// 抑制表默认容量（条）。
pub use engine::DEFAULT_SUPPRESS_CAPACITY;

/// 抑制表条目存活时间（毫秒）。
pub use engine::DEFAULT_SUPPRESS_TTL_MS;
