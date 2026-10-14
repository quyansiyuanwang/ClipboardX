//! 内存剪贴板替身。
//!
//! 存在的理由：同步引擎的逻辑（去重、抑制、忽略规则、应用远端内容）**不应该依赖
//! 真实图形会话**，否则在 CI 与无头环境里完全无法测试。所有引擎级测试都用它。

use crate::content::ClipContent;
use crate::error::ClipError;
use crate::{ClipBackend, ClipCapabilities};
use std::sync::{Arc, Mutex};

/// 共享的内存剪贴板。
#[derive(Debug, Clone, Default)]
pub struct FakeClipboard {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Debug, Default)]
struct FakeInner {
    content: Option<ClipContent>,
    /// 写入次数（断言"应用远端内容"用）。
    writes: u64,
    /// 读取次数（断言轮询行为用）。
    reads: u64,
    /// 是否模拟"被其他进程占用"的错误。
    busy: bool,
    /// 下一次读取是否返回错误。
    fail_next_read: bool,
}

impl FakeClipboard {
    /// 新建空剪贴板。
    pub fn new() -> Self {
        Self::default()
    }

    /// 带初始内容。
    pub fn with_content(content: ClipContent) -> Self {
        let clipboard = Self::new();
        clipboard.set_external(content);
        clipboard
    }

    /// 模拟"用户在别的程序里复制了内容"（不计入写入次数）。
    pub fn set_external(&self, content: ClipContent) {
        let mut inner = self.inner.lock().unwrap();
        inner.content = Some(content);
    }

    /// 清空。
    pub fn clear_external(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.content = None;
    }

    /// 当前内容。
    pub fn current(&self) -> Option<ClipContent> {
        self.inner.lock().unwrap().content.clone()
    }

    /// 本进程写入次数。
    pub fn write_count(&self) -> u64 {
        self.inner.lock().unwrap().writes
    }

    /// 本进程读取次数。
    pub fn read_count(&self) -> u64 {
        self.inner.lock().unwrap().reads
    }

    /// 设置"占用"状态（模拟 Windows 上别的进程持有剪贴板）。
    pub fn set_busy(&self, busy: bool) {
        self.inner.lock().unwrap().busy = busy;
    }

    /// 让下一次读取失败一次（测试重试逻辑）。
    pub fn fail_next_read(&self) {
        self.inner.lock().unwrap().fail_next_read = true;
    }
}

impl ClipBackend for FakeClipboard {
    fn read(&mut self) -> Result<Option<ClipContent>, ClipError> {
        let mut inner = self.inner.lock().unwrap();
        inner.reads += 1;
        if inner.fail_next_read {
            inner.fail_next_read = false;
            return Err(ClipError::Platform("注入的读取失败".to_string()));
        }
        if inner.busy {
            return Err(ClipError::Busy);
        }
        Ok(inner.content.clone())
    }

    fn write(&mut self, content: &ClipContent) -> Result<(), ClipError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.busy {
            return Err(ClipError::Busy);
        }
        inner.content = Some(content.clone());
        inner.writes += 1;
        Ok(())
    }

    fn capabilities(&self) -> ClipCapabilities {
        ClipCapabilities {
            text: true,
            image: true,
            watch: false,
            note: "内存替身（仅测试用）".to_string(),
        }
    }

    fn describe(&self) -> String {
        "内存剪贴板替身".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::RawImage;

    #[test]
    fn 读写与计数() {
        let mut clip = FakeClipboard::new();
        assert!(clip.read().unwrap().is_none());

        clip.write(&ClipContent::Text("hi".to_string())).unwrap();
        assert_eq!(clip.write_count(), 1);
        assert_eq!(clip.current().unwrap().text(), Some("hi"));
    }

    #[test]
    fn 外部设置不计入写入() {
        let clip = FakeClipboard::with_content(ClipContent::Text("外部".to_string()));
        assert_eq!(clip.write_count(), 0);
        assert_eq!(clip.current().unwrap().text(), Some("外部"));
    }

    #[test]
    fn 占用状态返回可重试错误() {
        let clip = FakeClipboard::new();
        clip.set_busy(true);
        let mut clip2 = clip.clone();
        let err = clip2.read().unwrap_err();
        assert!(err.is_retryable());
        assert!(matches!(err, ClipError::Busy));
    }

    #[test]
    fn 注入一次性读取失败() {
        let mut clip = FakeClipboard::new();
        clip.fail_next_read();
        assert!(clip.read().is_err());
        assert!(clip.read().is_ok(), "只应失败一次");
    }

    #[test]
    fn 图片内容也能存取() {
        let mut clip = FakeClipboard::new();
        let image = RawImage::new(
            2,
            2,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        )
        .unwrap();
        clip.write(&ClipContent::Image(image.clone())).unwrap();
        assert_eq!(clip.current().unwrap(), ClipContent::Image(image));
    }
}
