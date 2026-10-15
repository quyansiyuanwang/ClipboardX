//! ClipboardX 守护进程运行时。
//!
//! 职责：装配线程（watcher / engine / control）、单实例锁、后台挂载、自启注册、
//! 日志轮转，以及本机控制套接字。**不使用 async 运行时**：全部是阻塞线程 + channel。
//!
//! 与其他层的关系：
//! - [`cx_sync::Engine`] 是纯状态机，本 crate 负责把它的动作落到实处；
//! - [`cx_clip`] 提供剪贴板读写与平台事件；
//! - [`cx_core`] 管配置、身份、空间、历史与状态。

pub mod control;
mod error;
mod lock;
pub mod logging;
mod outbox;
pub mod runtime;
pub mod service;

pub use control::{
    Broadcaster, ControlClient, ControlServer, DAEMON_INFO_VERSION, DaemonInfo, Dispatcher,
    SubscribeFactory, read_control_token, send_one_way,
};
pub use error::DaemonError;
pub use lock::InstanceLock;
pub use runtime::{
    Daemon, DaemonOptions, autostart_status, install_autostart, running_pid, spawn_background,
    stop_running, uninstall_autostart,
};

/// 守护进程运行信息文件名（`daemon.json`）。
pub const DAEMON_INFO_FILE: &str = "daemon.json";

/// 控制令牌文件名（0600）。
pub const CONTROL_TOKEN_FILE: &str = "control.token";

/// 单实例锁文件名。
pub const LOCK_FILE: &str = "daemon.lock";
