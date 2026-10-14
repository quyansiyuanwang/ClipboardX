//! 线协议与落盘格式。
//!
//! - item 容器：`"CXI1" | ver | kind | cipher | rsv | meta_len:u32le | meta(postcard) | payload`
//! - manifest：每个设备只写自己的 `v1/manifests/<device_id>.json`，因此无写竞争
//! - 控制协议：本地 `127.0.0.1` 上的 JSON Lines

/// item 容器魔数（4 字节）。
pub const ITEM_MAGIC: [u8; 4] = *b"CXI1";

/// item 容器版本。
pub const ITEM_VERSION: u8 = 1;

/// manifest 文件名格式版本。
pub const MANIFEST_VERSION: u32 = 1;
