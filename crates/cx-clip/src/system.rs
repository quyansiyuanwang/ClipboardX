//! 平台剪贴板实现（`clipboard-rs`）。
//!
//! 两个平台差异要点：
//! 1. **占用与重试**：Windows 上剪贴板是全局独占资源，其他程序正持有它时
//!    读写会失败（`CLIPBRD_E_CANT_OPEN`）。这类错误必须有限次退避重试，
//!    否则用户偶尔会看到"复制没同步"。
//! 2. **变更通知 vs 轮询**：平台通知只在事件发生时才叫醒我们，比每 500 ms
//!    读一次剪贴板便宜得多（尤其大图）。通知不可用时（无图形会话、
//!    初始化失败）调用方应退回纯轮询。

use crate::content::{ClipContent, RawImage};
use crate::error::ClipError;
use crate::{ClipBackend, ClipCapabilities};
use clipboard_rs::common::RustImage;
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
    ContentFormat, RustImageData, WatcherShutdown,
};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::Mutex;
use std::time::Duration;

/// 遇到"剪贴板被占用"时的退避序列（毫秒）。三次之后放弃这一次读写，
/// 下一次轮询/事件还会再来。
const RETRY_DELAYS_MS: [u64; 3] = [30, 60, 120];

/// 系统剪贴板。
pub struct SystemClipboard {
    ctx: ClipboardContext,
}

impl SystemClipboard {
    /// 绑定当前会话的剪贴板。无图形会话（SSH、无 DISPLAY/WAYLAND_DISPLAY、
    /// 未登录的 macOS）会在这里失败，由调用方决定是否以 `--no-clipboard` 运行。
    pub fn new() -> Result<Self, ClipError> {
        let ctx = ClipboardContext::new().map_err(|e| {
            ClipError::Unavailable(format!(
                "{}（无图形会话时属正常：可用 cx daemon --no-clipboard 做纯中继）",
                e
            ))
        })?;
        Ok(Self { ctx })
    }

    /// 带退避重试地执行一次剪贴板操作。
    fn with_retry<T>(&self, op: impl Fn() -> Result<T, ClipError>) -> Result<T, ClipError> {
        let mut attempt = 0usize;
        loop {
            match op() {
                Ok(value) => return Ok(value),
                Err(err) => {
                    if !err.is_retryable() || attempt + 1 >= RETRY_DELAYS_MS.len() {
                        return Err(err);
                    }
                    let delay = RETRY_DELAYS_MS[attempt];
                    log::debug!("剪贴板被占用，{delay} ms 后重试");
                    std::thread::sleep(Duration::from_millis(delay));
                    attempt += 1;
                }
            }
        }
    }
}

impl ClipBackend for SystemClipboard {
    fn read(&mut self) -> Result<Option<ClipContent>, ClipError> {
        // 文本优先：绝大多数内容是文本，而且读取最便宜
        match self.with_retry(|| self.ctx.get_text().map_err(classify)) {
            Ok(text) if !text.is_empty() => return Ok(Some(ClipContent::Text(text))),
            Ok(_) => {}
            // 剪贴板被占用：这是真错误，交给上层退避
            Err(err) if err.is_retryable() => return Err(err),
            // 其余错误表示"当前没有文本格式"，继续看图片
            Err(_) => {}
        }

        if !self.ctx.has(ContentFormat::Image) {
            return Ok(None);
        }
        let image = self.with_retry(|| self.ctx.get_image().map_err(classify))?;
        Ok(Some(ClipContent::Image(raw_from_platform(image)?)))
    }

    fn write(&mut self, content: &ClipContent) -> Result<(), ClipError> {
        match content {
            ClipContent::Text(text) => {
                self.with_retry(|| self.ctx.set_text(text.clone()).map_err(classify))
            }
            ClipContent::Image(image) => {
                // 统一以 PNG 交给平台层，由它转成平台格式（Windows DIB / macOS TIFF）
                let png = image.to_png()?;
                self.with_retry(|| {
                    let data = RustImageData::from_bytes(&png)
                        .map_err(|e| ClipError::ImageCodec(e.to_string()))?;
                    self.ctx.set_image(data).map_err(classify)
                })
            }
        }
    }

    fn capabilities(&self) -> ClipCapabilities {
        ClipCapabilities {
            text: true,
            image: true,
            watch: true,
            note: format!("系统剪贴板（{}）", std::env::consts::OS),
        }
    }

    fn describe(&self) -> String {
        format!("系统剪贴板（{}）", std::env::consts::OS)
    }
}

/// 把平台错误翻译成本层错误；识别"被占用"以便重试。
fn classify(err: impl std::fmt::Display) -> ClipError {
    let message = err.to_string();
    // Windows 的 CLIPBRD_E_CANT_OPEN 是 0x800401D0；不同封装层给出的文案不一致，
    // 因此同时匹配符号名与错误码。
    if message.contains("CLIPBRD_E_CANT_OPEN") || message.contains("800401D0") {
        return ClipError::Busy;
    }
    ClipError::Platform(message)
}

/// 平台图片 → 原始 RGBA。
fn raw_from_platform(image: RustImageData) -> Result<RawImage, ClipError> {
    let (width, height) = image.get_size();
    let rgba = image
        .to_rgba8()
        .map_err(|e| ClipError::ImageCodec(e.to_string()))?;
    RawImage::new(width, height, rgba.into_raw())
}

/// 平台变更通知句柄。
pub struct ChangeNotifier {
    rx: Receiver<()>,
    shutdown: Mutex<Option<WatcherShutdown>>,
}

impl ChangeNotifier {
    /// 启动平台监听线程。失败表示该平台后端不可用，调用方应改为纯轮询。
    pub fn spawn() -> Result<Self, ClipError> {
        // 容量 1：复制过程中会连续触发多次事件，一次待处理通知就够（我们会重读剪贴板）
        let (tx, rx) = bounded::<()>(1);

        let mut watcher: ClipboardWatcherContext<NotifyHandler> =
            ClipboardWatcherContext::new().map_err(|e| ClipError::Unavailable(e.to_string()))?;
        let shutdown = watcher.get_shutdown_channel();
        watcher.add_handler(NotifyHandler { tx });

        std::thread::Builder::new()
            .name("cx-clip-watch".to_string())
            .spawn(move || watcher.start_watch())
            .map_err(|e| ClipError::Unavailable(format!("无法启动剪贴板监听线程：{e}")))?;

        Ok(Self {
            rx,
            shutdown: Mutex::new(Some(shutdown)),
        })
    }

    /// 等待一次变化。返回 `true` 表示有事件，`false` 表示超时——
    /// 超时不是错误，调用方应做兜底轮询（有些平台不保证通知可靠）。
    pub fn wait(&self, timeout: Duration) -> bool {
        self.rx.recv_timeout(timeout).is_ok()
    }

    /// 停止监听。
    pub fn stop(&self) {
        if let Some(shutdown) = self.shutdown.lock().unwrap().take() {
            shutdown.stop();
        }
    }
}

impl Drop for ChangeNotifier {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 通知处理器：只做一件事——叫醒等待方。
struct NotifyHandler {
    tx: Sender<()>,
}

impl ClipboardHandler for NotifyHandler {
    fn on_clipboard_change(&mut self) {
        // 满了就丢弃：待处理通知只需一个
        let _ = self.tx.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 占用错误被识别为可重试() {
        let err = classify("failed to open clipboard: CLIPBRD_E_CANT_OPEN");
        assert!(matches!(err, ClipError::Busy));
        assert!(err.is_retryable());

        let err = classify("OpenClipboard failed with hr = 0x800401D0");
        assert!(matches!(err, ClipError::Busy));
    }

    #[test]
    fn 其他错误不被当成可重试() {
        let err = classify("some other platform failure");
        assert!(matches!(err, ClipError::Platform(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn 无图形会话时报错信息包含提示() {
        // 直接构造 Unavailable，验证文案里有可操作建议
        let err = ClipError::Unavailable(
            "no display（无图形会话时属正常：可用 cx daemon --no-clipboard 做纯中继）".to_string(),
        );
        assert!(err.to_string().contains("--no-clipboard"));
    }
}
