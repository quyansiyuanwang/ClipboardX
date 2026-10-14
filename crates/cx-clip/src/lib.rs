//! ClipboardX 剪贴板适配层。
//!
//! 对外只暴露 [`ClipBackend`] 契约：读、写、能力查询，加上可选的平台变更通知
//! （[`ChangeNotifier`]）。平台实现基于 `clipboard-rs`，测试使用内存替身
//! （[`FakeClipboard`]），因此同步引擎的逻辑不依赖真实窗口会话。
//!
//! 平台注意：
//! - **Windows**：剪贴板是全局独占资源，别的进程持有它时读写会失败
//!   （`CLIPBRD_E_CANT_OPEN`），适配层内部会退避重试。
//! - **macOS**：必须在用户登录会话中运行（LaunchAgent，而非 LaunchDaemon），
//!   否则读不到 `NSPasteboard`。
//! - **Linux**：需要 X11（`DISPLAY`）或 Wayland（`WAYLAND_DISPLAY`）会话；
//!   两者都没有时只能以 `cx daemon --no-clipboard` 运行（纯中继）。

mod content;
mod error;
mod fake;
mod system;

pub use content::{ClipContent, ClipKind, RawImage};
pub use error::ClipError;
pub use fake::FakeClipboard;
pub use system::{ChangeNotifier, SystemClipboard};

/// 剪贴板能力（`cx doctor` 展示，也用于降级提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipCapabilities {
    /// 支持读取/写入文本。
    pub text: bool,
    /// 支持读取/写入图片。
    pub image: bool,
    /// 支持平台变更通知（否则只能轮询）。
    pub watch: bool,
    /// 说明文字。
    pub note: String,
}

/// 剪贴板后端契约。
///
/// 所有方法都可能因为"剪贴板被占用"而返回 [`ClipError::Busy`]；
/// 调用方应当把它当成"这次没拿到"，而不是致命错误。
pub trait ClipBackend: Send {
    /// 读取当前内容；返回 `Ok(None)` 表示剪贴板为空或格式不受支持。
    fn read(&mut self) -> Result<Option<ClipContent>, ClipError>;

    /// 写入内容。
    fn write(&mut self, content: &ClipContent) -> Result<(), ClipError>;

    /// 能力查询。
    fn capabilities(&self) -> ClipCapabilities;

    /// 人类可读的描述（日志与 `cx status` 用）。
    fn describe(&self) -> String;
}
