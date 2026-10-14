//! XML 小工具（WebDAV 与 S3 的响应解析共用）。
//!
//! 为什么单独成模块：两个后端都要处理"实体引用"这个坑——`quick-xml` 0.42 会把
//! `&quot;`、`&amp;` 这类引用作为**独立事件**（`Event::GeneralRef`）发出来。只在
//! `Text` 分支里取文本就会丢掉它们，把带引号的 ETag、带 `&` 的键名悄悄改掉。

/// 解析实体引用名（不含 `&` 与 `;`）。
///
/// 支持五个预定义实体与数字字符引用（`&#34;` / `&#x22;`）；未知实体返回 `None`，
/// 由调用方决定是原样保留还是丢弃。
pub fn resolve_entity(name: &str) -> Option<String> {
    match name {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" => Some("'".to_string()),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                digits.parse::<u32>().ok()?
            };
            char::from_u32(code).map(|c| c.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 实体解析() {
        assert_eq!(resolve_entity("quot").as_deref(), Some("\""));
        assert_eq!(resolve_entity("amp").as_deref(), Some("&"));
        assert_eq!(resolve_entity("lt").as_deref(), Some("<"));
        assert_eq!(resolve_entity("gt").as_deref(), Some(">"));
        assert_eq!(resolve_entity("apos").as_deref(), Some("'"));
        // 数字引用（十进制与十六进制）
        assert_eq!(resolve_entity("#34").as_deref(), Some("\""));
        assert_eq!(resolve_entity("#x22").as_deref(), Some("\""));
        assert_eq!(resolve_entity("#X22").as_deref(), Some("\""));
        assert_eq!(resolve_entity("#20013").as_deref(), Some("中"));
        // 未知或非法
        assert!(resolve_entity("nbsp").is_none());
        assert!(resolve_entity("#999999999").is_none());
        assert!(resolve_entity("").is_none());
    }
}
