//! UTC 日期换算（毫秒时间戳 ↔ `yyyyMMdd` 目录名）。
//!
//! 刻意不引入 `chrono`/`time`：我们只需要"某毫秒属于哪一天"这一个换算，
//! 用 Howard Hinnant 的 civil-from-days 算法二十行就能解决，且可以完整单测。

/// 一天的毫秒数。
pub const MS_PER_DAY: u64 = 86_400_000;

/// 由 Unix 毫秒得到 UTC 日期字符串 `yyyyMMdd`。
///
/// 负数毫秒（1970 年之前）按 UTC 截断到 0 —— 剪贴板内容不可能来自 1970 年前。
pub fn day_from_ms(ms: u64) -> String {
    let (y, m, d) = civil_from_days((ms / MS_PER_DAY) as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// 由 `yyyyMMdd` 得到当天 00:00:00 UTC 的毫秒；格式非法时返回 `None`。
pub fn day_start_ms(day: &str) -> Option<u64> {
    let (y, m, d) = parse_day(day)?;
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * MS_PER_DAY)
}

/// 解析 `yyyyMMdd`。
pub fn parse_day(day: &str) -> Option<(i64, u32, u32)> {
    if day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = day[0..4].parse().ok()?;
    let m: u32 = day[4..6].parse().ok()?;
    let d: u32 = day[6..8].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // 校验日期真实存在（例如 20250230 非法）
    let days = days_from_civil(y, m, d);
    let (y2, m2, d2) = civil_from_days(days);
    if (y, m, d) != (y2, m2, d2) {
        return None;
    }
    Some((y, m, d))
}

/// 天数（相对 1970-01-01）→ (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → 天数（相对 1970-01-01）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 纪元起点() {
        assert_eq!(day_from_ms(0), "19700101");
        assert_eq!(day_start_ms("19700101"), Some(0));
    }

    #[test]
    fn 已知时间点() {
        // 2025-10-01T13:06:38.712Z
        assert_eq!(day_from_ms(1_759_299_998_712), "20251001");
        // 2000-01-01T00:00:00Z
        assert_eq!(day_from_ms(946_684_800_000), "20000101");
        // 闰日 2024-02-29
        assert_eq!(day_from_ms(1_709_164_800_000), "20240229");
    }

    #[test]
    fn 当天最后一毫秒仍是同一天() {
        let start = day_start_ms("20251001").unwrap();
        assert_eq!(day_from_ms(start), "20251001");
        assert_eq!(day_from_ms(start + MS_PER_DAY - 1), "20251001");
        assert_eq!(day_from_ms(start + MS_PER_DAY), "20251002");
    }

    #[test]
    fn 日期往返一致() {
        for day in ["19700101", "20000229", "20250228", "20240301", "20991231"] {
            let start = day_start_ms(day).unwrap();
            assert_eq!(day_from_ms(start), day, "day={day} 往返失败");
        }
    }

    #[test]
    fn 非法日期被拒绝() {
        for bad in [
            "",
            "2025100",
            "202510011",
            "20251301",
            "20250132",
            "20250230",
            "abcdefgh",
        ] {
            assert!(day_start_ms(bad).is_none(), "应当拒绝 {bad}");
        }
    }
}
