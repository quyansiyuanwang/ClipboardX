//! 剪贴板适配层。
//!
//! 对外只暴露 `ClipBackend` 契约：读、写、监听、能力查询。平台实现基于 `clipboard-rs`，
//! 测试使用内存替身，因此同步引擎不依赖真实窗口会话。
//!
//! 平台注意：
//! - Windows：剪贴板可能被其他进程占用（`CLIPBRD_E_CANT_OPEN`），需要有限次退避重试。
//! - macOS：需在用户登录会话中运行（LaunchAgent，而非 LaunchDaemon）。
//! - Linux：需要 X11（`DISPLAY`）或 Wayland 会话；两者都没有时只能以 `--no-clipboard` 运行。

/// 剪贴板内容类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    /// 纯文本。
    Text,
    /// 图片（内部统一转码为 PNG 传输）。
    Image,
}
