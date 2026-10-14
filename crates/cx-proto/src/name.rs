//! item 对象名：`<created_ms:013>-<device8>-<hash8>.cxi`
//!
//! 命名即索引，三个性质必须同时成立：
//! 1. **不可变**：名字里含内容哈希与设备号，任何设备都不会覆盖别人的对象；
//! 2. **可排序**：时间戳零填充到 13 位，字典序 == 时间序，列目录即可按时间增量拉取；
//! 3. **可校验**：对象名同时充当载荷加密的 AAD，改名即解密失败，防止对象被搬移或伪造。

use crate::civil;
use crate::error::ProtoError;

/// item 对象后缀。
pub const ITEM_SUFFIX: &str = ".cxi";

/// 生成时使用的设备号前缀长度。
pub const DEVICE_SHORT_LEN: usize = 8;

/// 生成时使用的哈希前缀长度。
pub const HASH_SHORT_LEN: usize = 8;

/// 时间戳固定宽度（13 位十进制毫秒，可用到公元 2286 年）。
pub const TS_WIDTH: usize = 13;

/// 已解析的 item 对象名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemName {
    /// 产生时间（毫秒，UTC）。
    pub created_ms: u64,
    /// 设备 id 前缀（小写十六进制）。
    pub device_short: String,
    /// 内容哈希前缀（小写十六进制）。
    pub hash_short: String,
}

impl ItemName {
    /// 由时间戳、设备 id、内容哈希生成对象名。
    ///
    /// `device_id_hex` 与 `plain_hash_hex` 至少要有对应的前缀长度。
    pub fn build(created_ms: u64, device_id_hex: &str, plain_hash_hex: &str) -> String {
        let device = &device_id_hex[..device_id_hex.len().min(DEVICE_SHORT_LEN)];
        let hash = &plain_hash_hex[..plain_hash_hex.len().min(HASH_SHORT_LEN)];
        format!(
            "{created_ms:0width$}-{device}-{hash}{ITEM_SUFFIX}",
            width = TS_WIDTH
        )
    }

    /// 校验并构造（非法输入返回错误，便于在写入前就发现设备 id/哈希异常）。
    pub fn new(
        created_ms: u64,
        device_id_hex: &str,
        plain_hash_hex: &str,
    ) -> Result<Self, ProtoError> {
        if device_id_hex.len() < DEVICE_SHORT_LEN {
            return Err(ProtoError::BadName(format!(
                "设备 id 过短：{}",
                device_id_hex.len()
            )));
        }
        if plain_hash_hex.len() < HASH_SHORT_LEN {
            return Err(ProtoError::BadName(format!(
                "内容哈希过短：{}",
                plain_hash_hex.len()
            )));
        }
        if !device_id_hex.bytes().all(|b| b.is_ascii_hexdigit())
            || !plain_hash_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ProtoError::BadName(
                "设备 id 或哈希含非十六进制字符".to_string(),
            ));
        }
        let device_short = device_id_hex[..DEVICE_SHORT_LEN].to_ascii_lowercase();
        let hash_short = plain_hash_hex[..HASH_SHORT_LEN].to_ascii_lowercase();
        Ok(Self {
            created_ms,
            device_short,
            hash_short,
        })
    }

    /// 解析对象名。任何不符格式的输入都返回错误（包括别的程序留下的文件）。
    pub fn parse(name: &str) -> Result<Self, ProtoError> {
        let stem = name
            .strip_suffix(ITEM_SUFFIX)
            .ok_or_else(|| ProtoError::BadName(format!("缺少 {ITEM_SUFFIX} 后缀：{name}")))?;

        let parts: Vec<&str> = stem.splitn(3, '-').collect();
        if parts.len() != 3 {
            return Err(ProtoError::BadName(format!("字段数不为 3：{name}")));
        }
        let (ts, device, hash) = (parts[0], parts[1], parts[2]);

        if ts.len() != TS_WIDTH || !ts.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ProtoError::BadName(format!(
                "时间戳必须是 {TS_WIDTH} 位十进制：{ts}"
            )));
        }
        if device.len() != DEVICE_SHORT_LEN || !device.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtoError::BadName(format!("设备号非法：{device}")));
        }
        if hash.len() != HASH_SHORT_LEN || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtoError::BadName(format!("哈希前缀非法：{hash}")));
        }

        let created_ms: u64 = ts
            .parse()
            .map_err(|_| ProtoError::BadName(format!("时间戳溢出：{ts}")))?;

        Ok(Self {
            created_ms,
            device_short: device.to_ascii_lowercase(),
            hash_short: hash.to_ascii_lowercase(),
        })
    }

    /// 完整对象名。
    pub fn file_name(&self) -> String {
        format!(
            "{:0width$}-{}-{}{}",
            self.created_ms,
            self.device_short,
            self.hash_short,
            ITEM_SUFFIX,
            width = TS_WIDTH
        )
    }

    /// 该对象所属的日期目录（`yyyyMMdd`）。
    pub fn day(&self) -> String {
        civil::day_from_ms(self.created_ms)
    }

    /// 相对空间根目录的对象路径：`v1/items/<day>/<name>`。
    pub fn object_path(&self) -> String {
        format!("v1/items/{}/{}", self.day(), self.file_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "ab12cd34ef5678901234567890abcdef";
    const HASH: &str = "9f3c1e77aa55bb66cc77dd88ee99ff00112233445566778899aabbccddeeff00";

    #[test]
    fn 生成与解析往返() {
        let name = ItemName::build(1_759_299_998_712, DEV, HASH);
        assert_eq!(name, "1759299998712-ab12cd34-9f3c1e77.cxi");

        let parsed = ItemName::parse(&name).unwrap();
        assert_eq!(parsed.created_ms, 1_759_299_998_712);
        assert_eq!(parsed.device_short, "ab12cd34");
        assert_eq!(parsed.hash_short, "9f3c1e77");
        assert_eq!(parsed.file_name(), name);
    }

    #[test]
    fn 时间戳零填充使字典序等于时间序() {
        let a = ItemName::build(1_000_000_000_000, DEV, HASH);
        let b = ItemName::build(999_999_999_999, DEV, HASH);
        assert!(b < a, "字典序应与时间序一致：{b} < {a}");
    }

    #[test]
    fn 日期目录正确() {
        let name = ItemName::new(1_759_299_998_712, DEV, HASH).unwrap();
        assert_eq!(name.day(), "20251001");
        assert_eq!(
            name.object_path(),
            "v1/items/20251001/1759299998712-ab12cd34-9f3c1e77.cxi"
        );
    }

    #[test]
    fn 大小写归一() {
        let upper_dev = DEV.to_ascii_uppercase();
        let upper_hash = HASH.to_ascii_uppercase();
        let name = ItemName::new(1_759_299_998_712, &upper_dev, &upper_hash).unwrap();
        assert_eq!(name.device_short, "ab12cd34");
        assert_eq!(name.hash_short, "9f3c1e77");
    }

    #[test]
    fn 构造时拒绝过短输入() {
        assert!(ItemName::new(1, "ab12", HASH).is_err());
        assert!(ItemName::new(1, DEV, "9f").is_err());
        assert!(ItemName::new(1, "zzzzzzzzzz", HASH).is_err());
    }

    #[test]
    fn 解析拒绝各种非法名() {
        let bad = [
            "",
            "1759299998712-ab12cd34-9f3c1e77",           // 缺后缀
            "1759299998712-ab12cd34-9f3c1e77.txt",       // 错后缀
            "175929999871-ab12cd34-9f3c1e77.cxi",        // 时间戳位数不足
            "17592999987123-ab12cd34-9f3c1e77.cxi",      // 时间戳位数过多
            "1759299998712-ab12cd34.cxi",                // 字段数不足
            "1759299998712-ab12cd34-9f3c1e77-extra.cxi", // 字段数过多
            "1759299998712-ab12cd-9f3c1e77.cxi",         // 设备号过短
            "1759299998712-ab12cd34-9f3c1e.cxi",         // 哈希过短
            "1759299998712-zzzzzzzz-9f3c1e77.cxi",       // 设备号非十六进制
            "1759299998712-ab12cd34-zzzzzzzz.cxi",       // 哈希非十六进制
            "1a59299998712-ab12cd34-9f3c1e77.cxi",       // 时间戳含字母
        ];
        for name in bad {
            assert!(ItemName::parse(name).is_err(), "应当拒绝 {name}");
        }
    }

    #[test]
    fn 解析对随机字符串不panic() {
        // 简单的确定性伪随机，覆盖各种字符组合
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let alphabet = b"0123456789abcdef.cxi-";
        let mut buf = String::new();
        for _ in 0..2000 {
            buf.clear();
            let len = {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 33) as usize % 40
            };
            for _ in 0..len {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                buf.push(alphabet[(state >> 33) as usize % alphabet.len()] as char);
            }
            let _ = ItemName::parse(&buf);
        }
    }
}
