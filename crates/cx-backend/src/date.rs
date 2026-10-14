//! HTTP 日期解析。
//!
//! 为什么需要它：WebDAV 的 `getlastmodified` 是 HTTP-date（RFC 1123，
//! 形如 `Wed, 01 Oct 2025 13:06:38 GMT`），而 S3 的 `LastModified` 是 ISO 8601
//! （`2025-10-01T13:06:38.000Z`）。两种格式都要能解析成"毫秒时间戳"，
//! 才能做统一的增量判断。
//!
//! 刻意不引入 `chrono`/`time`：这里只需要两种固定格式，且都已经是 UTC。

use cx_proto::civil::days_from_civil;

/// 解析 HTTP-date 或 ISO 8601，返回 Unix 毫秒。
///
/// 无法识别时返回 `None`（调用方应当退化为"只依赖 ETag"）。
pub fn parse_http_date(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    parse_rfc1123(trimmed).or_else(|| parse_iso8601(trimmed))
}

/// `Wed, 01 Oct 2025 13:06:38 GMT`（星期可省略，时区可省略但必须是 UTC/GMT）。
fn parse_rfc1123(text: &str) -> Option<u64> {
    // 去掉可选的 "Www, " 前缀
    let rest = match text.split_once(',') {
        Some((weekday, rest)) if weekday.len() == 3 => rest.trim(),
        _ => text,
    };

    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let day: u32 = parts[0].parse().ok()?;
    let month = month_number(parts[1])?;
    let year: i64 = parts[2].parse().ok()?;
    let (hour, minute, second) = parse_clock(parts[3])?;
    if parts.len() >= 5 && !matches!(parts[4], "GMT" | "UTC" | "+0000" | "Z") {
        return None;
    }
    compose(year, month, day, hour, minute, second, 0)
}

/// `2025-10-01T13:06:38Z` / `2025-10-01T13:06:38.000Z` / 带 `+08:00` 偏移。
fn parse_iso8601(text: &str) -> Option<u64> {
    let (date, time_part) = text.split_once(['T', ' '])?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    // 拆出时区偏移
    let (clock, offset_minutes) = if let Some(stripped) = time_part.strip_suffix('Z') {
        (stripped, 0i64)
    } else if let Some((clock, offset)) = split_offset(time_part) {
        (clock, offset)
    } else {
        (time_part, 0)
    };

    let clock = clock.split('.').next().unwrap_or(clock);
    let (hour, minute, second) = parse_clock(clock)?;

    let base = compose(year, month, day, hour, minute, second, 0)?;
    // 时间戳是 UTC：本地时间减去偏移
    let adjusted = base as i64 - offset_minutes * 60_000;
    if adjusted < 0 {
        None
    } else {
        Some(adjusted as u64)
    }
}

/// 拆分 `+08:00` / `-0530` 形式的偏移，返回（时间部分, 分钟数）。
fn split_offset(time_part: &str) -> Option<(&str, i64)> {
    let index = time_part
        .char_indices()
        .filter(|(_, c)| *c == '+' || *c == '-')
        .map(|(i, _)| i)
        .next()?;
    let (clock, offset) = time_part.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let digits: String = offset.chars().filter(char::is_ascii_digit).collect();
    let minutes: i64 = match digits.len() {
        4 => {
            let hours: i64 = digits[0..2].parse().ok()?;
            let mins: i64 = digits[2..4].parse().ok()?;
            hours * 60 + mins
        }
        2 => digits.parse::<i64>().ok()? * 60,
        _ => return None,
    };
    Some((clock, sign * minutes))
}

fn parse_clock(clock: &str) -> Option<(u32, u32, u32)> {
    let mut parts = clock.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = parts.next().unwrap_or("0").parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| name.eq_ignore_ascii_case(m))
        .map(|index| index as u32 + 1)
}

/// 把 UTC 年月日时分秒拼成毫秒时间戳；还顺便校验日期真实存在。
fn compose(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    millis: u32,
) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // 回算一次，拒绝 2 月 30 日这类"看起来合法"的日期
    let (y2, m2, d2) = cx_proto::civil::civil_from_days(days);
    if (y2, m2, d2) != (year, month, day) {
        return None;
    }
    if days < 0 {
        return None;
    }
    let seconds = days as u64 * 86_400 + hour as u64 * 3_600 + minute as u64 * 60 + second as u64;
    Some(seconds * 1000 + millis as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2025-10-01T13:06:38Z（= day_start(20251001) + 13:06:38）
    const SAMPLE_MS: u64 = 1_759_323_998_000;

    #[test]
    fn rfc1123_解析() {
        assert_eq!(
            parse_http_date("Wed, 01 Oct 2025 13:06:38 GMT"),
            Some(SAMPLE_MS)
        );
        // 星期缺失、UTC 代替 GMT
        assert_eq!(parse_http_date("01 Oct 2025 13:06:38 UTC"), Some(SAMPLE_MS));
        assert_eq!(
            parse_http_date("Wed, 01 Oct 2025 13:06:38 GMT\n"),
            Some(SAMPLE_MS)
        );
    }

    #[test]
    fn iso8601_解析() {
        assert_eq!(parse_http_date("2025-10-01T13:06:38Z"), Some(SAMPLE_MS));
        assert_eq!(parse_http_date("2025-10-01T13:06:38.000Z"), Some(SAMPLE_MS));
        assert_eq!(parse_http_date("2025-10-01T13:06:38"), Some(SAMPLE_MS));
    }

    #[test]
    fn 带时区偏移的_iso8601() {
        // +08:00 的 21:06:38 就是 UTC 的 13:06:38
        assert_eq!(
            parse_http_date("2025-10-01T21:06:38+08:00"),
            Some(SAMPLE_MS)
        );
        assert_eq!(parse_http_date("2025-10-01T13:06:38+0000"), Some(SAMPLE_MS));
        // -05:30：本地 07:36:38 对应 UTC 13:06:38
        assert_eq!(
            parse_http_date("2025-10-01T07:36:38-05:30"),
            Some(SAMPLE_MS)
        );
    }

    #[test]
    fn 纪元起点与已知日期() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("2000-01-01T00:00:00Z"),
            Some(946_684_800_000)
        );
        assert_eq!(
            parse_http_date("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
    }

    #[test]
    fn 非法输入返回_none() {
        for bad in [
            "",
            "not a date",
            "Wed, 01 Foo 2025 13:06:38 GMT",
            "Wed, 32 Oct 2025 13:06:38 GMT",
            "Wed, 30 Feb 2025 13:06:38 GMT",
            "2025-13-01T00:00:00Z",
            "2025-10-01T25:00:00Z",
            "Wed, 01 Oct 2025 13:06:38 EST",
        ] {
            assert!(parse_http_date(bad).is_none(), "应当拒绝 {bad:?}");
        }
    }

    #[test]
    fn 随机输入不panic() {
        let mut state: u64 = 0x5DEE_CE66_D1CE_4E5B;
        for _ in 0..3000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = ((state >> 33) as usize) % 32;
            let text: String = (0..len)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let alphabet = b"0123456789 GMT+:-TZ.ABCDEFGHIJKLMNOPQRSTUVWXYZ";
                    alphabet[(state >> 33) as usize % alphabet.len()] as char
                })
                .collect();
            let _ = parse_http_date(&text);
        }
    }
}
