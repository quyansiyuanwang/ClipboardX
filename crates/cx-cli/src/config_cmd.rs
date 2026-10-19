//! `cx config get/set`：把"必须手改 config.toml"的设置搬进命令行。
//!
//! 为什么要有它：改一个轮询间隔、开一下局域网、调历史条数上限，原来都得
//! `cx config path` 找到文件、手改、再 `cx restart`——改错一个字段就是启动失败，
//! 而且没有校验、没有范围检查、没有撤销提示。
//!
//! 设计取舍：
//! - **只暴露白名单里的键**，而不是通用的 TOML 编辑器：每个键都有明确的类型与范围，
//!   写错会当场报错，而不是等守护进程起不来；
//! - 值一律用文本传，**统一走 `parse`**（`true/false`、数字、枚举字面值、逗号分隔列表），
//!   与 README 里的写法一致；
//! - 写盘前跑一遍整份配置的 `problems()`：许可性检查不过就整体拒绝，不留半份坏配置；
//! - 结果里告诉用户**要不要重启**（轮询间隔这类是热重载就生效，端口/加密这类必须重启）。

use cx_core::config::{ApplyRemoteMode, Config};

/// 一个可设置项的描述。
struct Spec {
    key: &'static str,
    hint: &'static str,
}

/// 白名单：键 → 值写法提示。
const SPECS: &[Spec] = &[
    Spec {
        key: "device.name",
        hint: "设备显示名，例如 笔记本",
    },
    Spec {
        key: "space.encrypt",
        hint: "true/false（关掉就是明文上传，界面上会一直提示）",
    },
    Spec {
        key: "sync.watch_interval_ms",
        hint: "监视剪贴板的兜底轮询间隔（毫秒，≥50）",
    },
    Spec {
        key: "sync.debounce_ms",
        hint: "去抖窗口（毫秒，≥0 表示连续变化合并）",
    },
    Spec {
        key: "sync.poll_active_ms",
        hint: "活跃期的远端轮询间隔（毫秒，≥100）",
    },
    Spec {
        key: "sync.poll_idle_ms",
        hint: "空闲期的远端轮询间隔（毫秒，≥100）",
    },
    Spec {
        key: "sync.max_item_bytes",
        hint: "单条内容上限（字节，≥1024）",
    },
    Spec {
        key: "sync.history_max_items",
        hint: "本机历史保留条数（≥1）",
    },
    Spec {
        key: "sync.history_max_days",
        hint: "本机历史保留天数（0 表示不按时间清理）",
    },
    Spec {
        key: "sync.apply_remote",
        hint: "远端内容如何应用：latest / only_if_idle / manual",
    },
    Spec {
        key: "sync.normalize_newlines",
        hint: "true/false（把 CRLF 归一化成 LF）",
    },
    Spec {
        key: "sync.ignore_blank_text",
        hint: "true/false（忽略纯空白内容）",
    },
    Spec {
        key: "sync.ignore_globs",
        hint: "逗号分隔的 glob 列表，例如 \"*password*,*secret*\"；传空哨兵 -- 或 \"\" 清空",
    },
    Spec {
        key: "lan.enabled",
        hint: "true/false（局域网直传）",
    },
    Spec {
        key: "lan.port",
        hint: "局域网监听端口（1–65535）",
    },
    Spec {
        key: "lan.use_mdns",
        hint: "true/false（用 mDNS 自动发现对端）",
    },
];

/// 全部可设置的键（`cx config get` 不带参数时列出）。
pub fn keys() -> Vec<&'static str> {
    SPECS.iter().map(|spec| spec.key).collect()
}

/// 值写法的提示（`cx config set` 报错时一并给出）。
pub fn hint(key: &str) -> Option<&'static str> {
    SPECS.iter().find(|spec| spec.key == key).map(|s| s.hint)
}

/// 这个键改了之后要不要重启守护进程才生效。
///
/// 轮询/去抖/忽略规则这类由引擎读一次配置就用上新值（`cx reload` 即可）；
/// 端口、加密开关、设备名这类要在启动时装配，必须重启。
fn needs_restart(key: &str) -> bool {
    matches!(
        key,
        "device.name" | "space.encrypt" | "lan.enabled" | "lan.port" | "lan.use_mdns"
    )
}

/// 读取一个键的当前值（写成命令行里能直接敲回去的形式）。
pub fn get(config: &Config, key: &str) -> Option<String> {
    let value = match key {
        "device.name" => config.device.name.clone().unwrap_or_default(),
        "space.encrypt" => config.space.encrypt.to_string(),
        "sync.watch_interval_ms" => config.sync.watch_interval_ms.to_string(),
        "sync.debounce_ms" => config.sync.debounce_ms.to_string(),
        "sync.poll_active_ms" => config.sync.poll_active_ms.to_string(),
        "sync.poll_idle_ms" => config.sync.poll_idle_ms.to_string(),
        "sync.max_item_bytes" => config.sync.max_item_bytes.to_string(),
        "sync.history_max_items" => config.sync.history_max_items.to_string(),
        "sync.history_max_days" => config.sync.history_max_days.to_string(),
        "sync.apply_remote" => config.sync.apply_remote.as_str().to_string(),
        "sync.normalize_newlines" => config.sync.normalize_newlines.to_string(),
        "sync.ignore_blank_text" => config.sync.ignore_blank_text.to_string(),
        "sync.ignore_globs" => config.sync.ignore_globs.join(","),
        "lan.enabled" => config.lan.enabled.to_string(),
        "lan.port" => config.lan.port.to_string(),
        "lan.use_mdns" => config.lan.use_mdns.to_string(),
        _ => return None,
    };
    Some(value)
}

/// 设置一个键；返回"是否需要重启"。
pub fn set(config: &mut Config, key: &str, raw: &str) -> Result<bool, String> {
    let value = raw.trim();
    match key {
        "device.name" => {
            if value.is_empty() {
                config.device.name = None;
            } else {
                config.device.name = Some(value.to_string());
            }
        }
        "space.encrypt" => {
            let encrypt = parse_bool(value)?;
            if !encrypt {
                return Err(
                    "关闭加密会把剪贴板内容以明文上传到后端；本工具不提供这个开关。\
                     确实需要明文请直接改 config.toml 里的 [space] encrypt = false"
                        .to_string(),
                );
            }
            config.space.encrypt = true;
        }
        "sync.watch_interval_ms" => {
            config.sync.watch_interval_ms = parse_u64(value, 50, 3_600_000)?
        }
        "sync.debounce_ms" => config.sync.debounce_ms = parse_u64(value, 0, 60_000)?,
        "sync.poll_active_ms" => config.sync.poll_active_ms = parse_u64(value, 100, 3_600_000)?,
        "sync.poll_idle_ms" => config.sync.poll_idle_ms = parse_u64(value, 100, 3_600_000)?,
        "sync.max_item_bytes" => {
            config.sync.max_item_bytes = parse_u64(value, 1024, 512 * 1024 * 1024)?
        }
        "sync.history_max_items" => {
            config.sync.history_max_items = parse_u64(value, 1, 1_000_000)? as usize
        }
        "sync.history_max_days" => config.sync.history_max_days = parse_u64(value, 0, 36_500)?,
        "sync.apply_remote" => {
            config.sync.apply_remote = match value.to_ascii_lowercase().as_str() {
                "latest" => ApplyRemoteMode::Latest,
                "only_if_idle" => ApplyRemoteMode::OnlyIfIdle,
                "manual" => ApplyRemoteMode::Manual,
                other => {
                    return Err(format!(
                        "无法识别的 apply_remote 取值「{other}」（可选 latest / only_if_idle / manual）"
                    ));
                }
            }
        }
        "sync.normalize_newlines" => config.sync.normalize_newlines = parse_bool(value)?,
        "sync.ignore_blank_text" => config.sync.ignore_blank_text = parse_bool(value)?,
        "sync.ignore_globs" => {
            let globs: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect();
            for glob in &globs {
                // 空规则会匹配一切：那等于把所有内容都忽略掉，必须拦住
                if glob.is_empty() {
                    return Err("忽略规则不能为空".to_string());
                }
                if glob == "*" || glob == "**" {
                    return Err(format!(
                        "忽略规则「{glob}」会匹配所有内容，等于关闭同步；请写具体一点"
                    ));
                }
            }
            config.sync.ignore_globs = globs;
        }
        "lan.enabled" => config.lan.enabled = parse_bool(value)?,
        "lan.port" => {
            let port = parse_u64(value, 1, 65_535)?;
            config.lan.port = port as u16;
        }
        "lan.use_mdns" => config.lan.use_mdns = parse_bool(value)?,
        other => return Err(format!("不认识的配置键「{other}」")),
    }

    // 写完跑一遍整份配置的许可性检查：有问题就整体拒绝，别留半份坏配置
    let problems = config.problems();
    if let Some(problem) = problems.first() {
        return Err(format!("这个值会让配置不合法：{problem}"));
    }
    Ok(needs_restart(key))
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("期望 true/false，实际是「{other}」")),
    }
}

fn parse_u64(value: &str, min: u64, max: u64) -> Result<u64, String> {
    let parsed: u64 = value
        .parse()
        .map_err(|_| format!("期望 {min}–{max} 之间的整数，实际是「{value}」"))?;
    if parsed < min || parsed > max {
        return Err(format!("取值 {parsed} 超出允许范围 {min}–{max}"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn 每个白名单键都能读也能写() {
        for key in keys() {
            let mut config = config();
            let before = get(&config, key).unwrap_or_else(|| panic!("{key} 读不到"));
            // 用当前值写回去必须是合法的且值不变（除 space.encrypt 被有意锁成 true）
            let _ = set(&mut config, key, &before);
            if key != "space.encrypt" {
                assert_eq!(get(&config, key).as_deref(), Some(before.as_str()), "{key}");
            }
            assert!(hint(key).is_some(), "{key} 缺少写法提示");
        }
    }

    #[test]
    fn 数值与布尔都做范围检查() {
        let mut config = config();
        assert!(set(&mut config, "lan.port", "70000").is_err());
        assert!(set(&mut config, "lan.port", "0").is_err());
        assert!(set(&mut config, "lan.port", "8699").is_ok());
        assert_eq!(config.lan.port, 8699);

        assert!(
            set(&mut config, "sync.watch_interval_ms", "10").is_err(),
            "太快"
        );
        assert!(set(&mut config, "sync.history_max_items", "0").is_err());
        assert!(set(&mut config, "sync.apply_remote", "sometimes").is_err());
        assert!(set(&mut config, "lan.enabled", "maybe").is_err());

        // 报错不能落地：上面所有失败都该保持原值
        assert!(config.lan.enabled == Config::default().lan.enabled);
    }

    #[test]
    fn 开关类键接受常见写法() {
        let mut config = config();
        set(&mut config, "sync.normalize_newlines", "off").unwrap();
        assert!(!config.sync.normalize_newlines);
        set(&mut config, "sync.normalize_newlines", "YES").unwrap();
        assert!(config.sync.normalize_newlines);
    }

    #[test]
    fn 忽略规则按逗号切分且拒绝清空一切的规则() {
        let mut config = config();
        set(&mut config, "sync.ignore_globs", "*password*, *secret*").unwrap();
        assert_eq!(config.sync.ignore_globs, vec!["*password*", "*secret*"]);
        assert_eq!(
            get(&config, "sync.ignore_globs").unwrap(),
            "*password*,*secret*"
        );

        assert!(
            set(&mut config, "sync.ignore_globs", "*").is_err(),
            "会关掉所有同步"
        );
        assert!(set(&mut config, "sync.ignore_globs", "**").is_err());
        set(&mut config, "sync.ignore_globs", "").unwrap();
        assert!(config.sync.ignore_globs.is_empty(), "空值表示清空");
    }

    #[test]
    fn 不允许用命令行关掉加密() {
        let mut config = config();
        let err = set(&mut config, "space.encrypt", "false").unwrap_err();
        assert!(err.contains("明文"), "{err}");
        assert!(config.space.encrypt, "开了就不能从命令行关掉");
    }

    #[test]
    fn 不认识的键与非法值都给出可读的错误() {
        let mut config = config();
        assert!(
            set(&mut config, "sync.nope", "1")
                .unwrap_err()
                .contains("不认识")
        );
        assert!(
            set(&mut config, "lan.port", "http")
                .unwrap_err()
                .contains("整数")
        );
        assert!(hint("sync.nope").is_none());
        assert!(get(&config, "sync.nope").is_none());
    }

    #[test]
    fn 重启要求按字段区分() {
        assert!(needs_restart("lan.port"));
        assert!(needs_restart("space.encrypt"));
        assert!(!needs_restart("sync.poll_active_ms"));
        assert!(!needs_restart("sync.ignore_globs"));
    }
}
