//! 剪贴板适配层的错误类型。

/// 剪贴板操作的失败。
///
/// 注意 [`ClipError::Busy`]：Windows 上别的进程可能正持有剪贴板，
/// 这类错误是可重试的，调用方（适配层内部）会自动退避重试。
#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    /// 剪贴板被其他进程占用（可重试）。
    #[error("剪贴板被占用（可重试）")]
    Busy,

    /// 平台剪贴板不可用：没有图形会话、权限不足、或平台后端初始化失败。
    #[error("剪贴板不可用：{0}")]
    Unavailable(String),

    /// 剪贴板当前没有可识别的内容。
    #[error("剪贴板为空或格式不受支持")]
    Empty,

    /// 图片尺寸与像素数据长度不一致。
    #[error("图片数据非法：{width}x{height} 需要 {expected} 字节，实际 {actual}")]
    BadImage {
        /// 宽。
        width: u32,
        /// 高。
        height: u32,
        /// 期望字节数。
        expected: usize,
        /// 实际字节数。
        actual: usize,
    },

    /// 图片编解码失败。
    #[error("图片编解码失败：{0}")]
    ImageCodec(String),

    /// 平台错误透传。
    #[error("平台剪贴板错误：{0}")]
    Platform(String),
}

impl ClipError {
    /// 是否值得重试（占用类错误）。
    pub fn is_retryable(&self) -> bool {
        matches!(self, ClipError::Busy)
    }
}
