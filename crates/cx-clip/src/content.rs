//! 剪贴板内容模型。
//!
//! 关键设计：**指纹用原始数据、载荷用归一化数据**。
//!
//! - 轮询/去重要算指纹，必须便宜：文本直接用 UTF-8 字节，图片用原始 RGBA 字节，
//!   不涉及任何编码/解码。
//! - 真正要同步时才产出载荷：文本原样，图片统一转成 PNG —— 这样接收端不必关心
//!   来源平台是 DIB（Windows）、TIFF（macOS）还是别的格式。

use crate::error::ClipError;
use cx_proto::{HASH_LEN, Kind};

/// 剪贴板内容类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipKind {
    /// 纯文本。
    Text,
    /// 图片。
    Image,
}

impl ClipKind {
    /// 转为协议层的类型。
    pub fn to_proto(self) -> Kind {
        match self {
            ClipKind::Text => Kind::Text,
            ClipKind::Image => Kind::Image,
        }
    }

    /// 展示名。
    pub fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Image => "image",
        }
    }
}

/// 原始 RGBA8 位图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImage {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8 像素数据，长度必须是 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl RawImage {
    /// 构造并校验长度。
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, ClipError> {
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            return Err(ClipError::BadImage {
                width,
                height,
                expected,
                actual: rgba.len(),
            });
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// 编码为 PNG（仅用于同步载荷，不进指纹）。
    pub fn to_png(&self) -> Result<Vec<u8>, ClipError> {
        use image::{ImageFormat, RgbaImage};
        let img = RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| ClipError::ImageCodec("像素数据与尺寸不匹配".to_string()))?;
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| ClipError::ImageCodec(e.to_string()))?;
        Ok(out)
    }

    /// 从 PNG 解码。
    pub fn from_png(bytes: &[u8]) -> Result<Self, ClipError> {
        let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map_err(|e| ClipError::ImageCodec(e.to_string()))?
            .to_rgba8();
        let (width, height) = img.dimensions();
        Self::new(width, height, img.into_raw())
    }

    /// 像素数。
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// 剪贴板内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipContent {
    /// 文本（保持原始字节，不做换行归一化）。
    Text(String),
    /// 图片。
    Image(RawImage),
}

impl ClipContent {
    /// 内容类型。
    pub fn kind(&self) -> ClipKind {
        match self {
            ClipContent::Text(_) => ClipKind::Text,
            ClipContent::Image(_) => ClipKind::Image,
        }
    }

    /// 指纹：文本取 UTF-8 原文、图片取宽高 + RGBA 原文。用于轮询去重与回声抑制。
    pub fn fingerprint(&self) -> [u8; HASH_LEN] {
        match self {
            ClipContent::Text(text) => *blake3::hash(text.as_bytes()).as_bytes(),
            ClipContent::Image(image) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&image.width.to_le_bytes());
                hasher.update(&image.height.to_le_bytes());
                hasher.update(&image.rgba);
                *hasher.finalize().as_bytes()
            }
        }
    }

    /// 指纹的十六进制表示。
    pub fn fingerprint_hex(&self) -> String {
        hex::encode(self.fingerprint())
    }

    /// 归一化载荷（文本 UTF-8 / 图片 PNG）。哈希与传输都基于它。
    pub fn plain_bytes(&self) -> Result<Vec<u8>, ClipError> {
        match self {
            ClipContent::Text(text) => Ok(text.as_bytes().to_vec()),
            ClipContent::Image(image) => image.to_png(),
        }
    }

    /// 由载荷还原内容（接收端用）。
    pub fn from_plain(kind: ClipKind, bytes: &[u8]) -> Result<Self, ClipError> {
        match kind {
            ClipKind::Text => {
                let text = String::from_utf8(bytes.to_vec())
                    .map_err(|e| ClipError::Platform(format!("远端文本不是合法 UTF-8：{e}")))?;
                Ok(ClipContent::Text(text))
            }
            ClipKind::Image => Ok(ClipContent::Image(RawImage::from_png(bytes)?)),
        }
    }

    /// 文本视图（供忽略规则匹配）。
    pub fn text(&self) -> Option<&str> {
        match self {
            ClipContent::Text(text) => Some(text),
            ClipContent::Image(_) => None,
        }
    }

    /// 载荷大小的粗略估计（用于体积上限判断，避免提前编码图片）。
    pub fn approx_len(&self) -> usize {
        match self {
            ClipContent::Text(text) => text.len(),
            // PNG 通常小于原始 RGBA；这里保守地按原始大小估计，宁严不宽
            ClipContent::Image(image) => image.rgba.len(),
        }
    }

    /// 人类可读摘要（日志与 TUI 展示用；**不包含内容本身**）。
    pub fn describe(&self) -> String {
        match self {
            ClipContent::Text(text) => {
                let chars = text.chars().count();
                format!("文本 {chars} 字符 / {} 字节", text.len())
            }
            ClipContent::Image(image) => format!(
                "图片 {}x{}（约 {:.1} MiB 原始像素）",
                image.width,
                image.height,
                image.rgba.len() as f64 / 1_048_576.0
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(w: u32, h: u32) -> RawImage {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.push((x % 256) as u8);
                rgba.push((y % 256) as u8);
                rgba.push(128);
                rgba.push(255);
            }
        }
        RawImage::new(w, h, rgba).unwrap()
    }

    #[test]
    fn 文本指纹稳定且区分大小写() {
        let a = ClipContent::Text("Hello".to_string());
        let b = ClipContent::Text("Hello".to_string());
        let c = ClipContent::Text("hello".to_string());
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(a.fingerprint_hex().len(), 64);
    }

    #[test]
    fn 图片指纹包含尺寸与像素() {
        let base = ClipContent::Image(solid_image(4, 4));
        let same = ClipContent::Image(solid_image(4, 4));
        let other_pixels = ClipContent::Image(solid_image(4, 5));
        assert_eq!(base.fingerprint(), same.fingerprint());
        assert_ne!(base.fingerprint(), other_pixels.fingerprint());
    }

    #[test]
    fn 图片尺寸与长度不一致时报错() {
        let err = RawImage::new(4, 4, vec![0; 10]).unwrap_err();
        assert!(matches!(err, ClipError::BadImage { .. }));
    }

    #[test]
    fn png_往返保持像素() {
        let image = solid_image(7, 5);
        let png = image.to_png().unwrap();
        assert_eq!(
            &png[0..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "PNG 魔数"
        );
        let back = RawImage::from_png(&png).unwrap();
        assert_eq!(back, image);
    }

    #[test]
    fn png_编码是确定性的() {
        let image = solid_image(9, 3);
        assert_eq!(image.to_png().unwrap(), image.to_png().unwrap());
    }

    #[test]
    fn 非法_png_报错不panic() {
        for bad in [&b""[..], &b"not png"[..], &[0x89, b'P', b'N', b'G'][..]] {
            assert!(RawImage::from_png(bad).is_err());
        }
    }

    #[test]
    fn 载荷往返文本与图片() {
        let text = ClipContent::Text("跨设备 🎉".to_string());
        let bytes = text.plain_bytes().unwrap();
        assert_eq!(bytes, "跨设备 🎉".as_bytes());
        assert_eq!(
            ClipContent::from_plain(ClipKind::Text, &bytes).unwrap(),
            text
        );

        let image = ClipContent::Image(solid_image(3, 2));
        let bytes = image.plain_bytes().unwrap();
        assert_eq!(
            ClipContent::from_plain(ClipKind::Image, &bytes).unwrap(),
            image
        );
    }

    #[test]
    fn 非法_utf8_被拒绝() {
        assert!(ClipContent::from_plain(ClipKind::Text, &[0xff, 0xfe]).is_err());
    }

    #[test]
    fn 体积估计与摘要() {
        let text = ClipContent::Text("abc".to_string());
        assert_eq!(text.approx_len(), 3);
        assert!(text.describe().contains("3 字节"));
        assert!(text.text().is_some());

        let image = ClipContent::Image(solid_image(10, 10));
        assert_eq!(image.approx_len(), 400);
        assert!(image.describe().contains("10x10"));
        assert!(image.text().is_none());
    }
}
