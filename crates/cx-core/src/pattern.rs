//! 极简 glob 匹配（`*` 任意序列、`?` 单个字符）。
//!
//! 为什么不用 `regex`：配置里"忽略某些剪贴板内容"只需要最简单的通配，
//! 引入正则引擎（连同它的编译期开销与体积）不划算。这里实现 30 行的
//! 两指针回溯算法，且按 UTF-8 字符推进（`?` 吃一个完整字符而不是一个字节）。

/// 判断 `text` 是否匹配 `pattern`（整串匹配，区分大小写）。
///
/// - `*` 匹配任意长度（含空）的序列
/// - `?` 匹配任意单个字符（超出一个 UTF-8 字符）
/// - 其余字符按字面比较
///
/// 需要"包含"语义时写 `*关键字*`。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            ti = skip_char(t, ti);
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star_p {
            // 回溯：让 `*` 多吃一个字符
            pi = sp + 1;
            star_t = skip_char(t, star_t);
            ti = star_t;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// 跳过 `t[index]` 处的一个完整字符（含续字节），返回下一个字符的起始位置。
fn skip_char(t: &[u8], index: usize) -> usize {
    let mut next = index + 1;
    while next < t.len() && (t[next] & 0xC0) == 0x80 {
        next += 1;
    }
    next
}

/// 文本是否为空白（用于 `ignore_blank_text`）。
pub fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字面匹配() {
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abd"));
        assert!(!glob_match("abc", "abcd"));
        assert!(!glob_match("abcd", "abc"));
    }

    #[test]
    fn 星号匹配任意序列() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "任何内容"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "a中间很多字符c"));
        assert!(glob_match("*abc*", "xxabcxx"));
        assert!(!glob_match("a*c", "abd"));
    }

    #[test]
    fn 问号匹配单个字符() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
        // 一个 ? 对应一个完整的中文字符（3 字节）
        assert!(glob_match("a?c", "a中c"));
        assert!(!glob_match("a?c", "a中中c"));
    }

    #[test]
    fn 连续星号与尾部星号() {
        assert!(glob_match("**", "abc"));
        assert!(glob_match("a**b", "ab"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("a*b*", "aXbY"));
        assert!(!glob_match("a*b", "abX"));
    }

    #[test]
    fn 空模式只匹配空串() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn 空白判断() {
        assert!(is_blank(""));
        assert!(is_blank("   \n\t "));
        assert!(!is_blank("a"));
        assert!(!is_blank(" a "));
    }

    #[test]
    fn 随机输入不panic() {
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        let letters = ['a', 'b', '*', '?', '中', ' '];
        for _ in 0..2000 {
            let plen = next() % 8;
            let tlen = next() % 12;
            let pattern: String = (0..plen).map(|_| letters[next() % letters.len()]).collect();
            let text: String = (0..tlen).map(|_| letters[next() % letters.len()]).collect();
            let _ = glob_match(&pattern, &text);
        }
    }
}
