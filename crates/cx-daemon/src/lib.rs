//! 守护进程运行时。
//!
//! 负责装配各线程（watcher / engine / 每后端 worker / control）、单实例 pid 锁、
//! 后台挂载（分离进程）、自启注册（Windows `HKCU\...\Run`、macOS LaunchAgent、
//! Linux systemd user unit）以及日志轮转。
//!
//! 不使用 async 运行时：全部为阻塞线程 + channel。

/// 守护进程实例状态文件名（记录端口、pid、启动时间）。
pub const DAEMON_STATE_FILE: &str = "daemon.json";

/// 控制令牌文件名（0600）。
pub const CONTROL_TOKEN_FILE: &str = "control.token";

/// 单实例锁文件名。
pub const LOCK_FILE: &str = "daemon.lock";
