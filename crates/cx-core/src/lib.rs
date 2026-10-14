//! ClipboardX 核心领域层。
//!
//! 本 crate 承载与平台、传输、界面无关的部分：配置模型、设备身份、同步空间与密钥、

//!
//! 约定：本 crate 不依赖任何平台剪贴板 API、不发起网络请求、不访问终端。

/// 对象存储布局版本（`v1/` 目录下的落盘格式版本，见 `docs/protocol.md`）。
pub const STORE_LAYOUT_VERSION: u32 = 1;

/// 本地控制套接字协议版本。
pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

/// 本程序写入 `cx-space.json` 的格式版本。
pub const SPACE_FORMAT_VERSION: u32 = 1;
