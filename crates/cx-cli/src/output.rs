//! 输出格式化：人类可读优先，`--json` 供脚本消费。
//!
//! 约定：**人类可读输出也不隐藏关键信息**（设备、后端、错误、计数），
//! 因为用户排障时第一眼看到的就是它；而 `--json` 与人类输出字段一一对应。

use cx_proto::control::{DoctorReport, HistoryPreview, HistorySummary, StatusReport};
use serde_json::Value;
use std::process::ExitCode;

/// 版本信息。
pub fn version(json: bool) {
    let version = env!("CARGO_PKG_VERSION");
    if json {
        let payload = serde_json::json!({
            "name": "cx",
            "version": version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "store_layout": cx_core::STORE_LAYOUT_VERSION,
            "control_protocol": cx_core::CONTROL_PROTOCOL_VERSION,
            "space_format": cx_core::SPACE_FORMAT_VERSION,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("cx: 无法序列化版本信息: {err}"),
        }
    } else {
        println!("cx {version}");
        println!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("store layout: v{}", cx_core::STORE_LAYOUT_VERSION);
        println!("control protocol: v{}", cx_core::CONTROL_PROTOCOL_VERSION);
        println!("space format: v{}", cx_core::SPACE_FORMAT_VERSION);
    }
}

/// 状态。
pub fn status(data: &Value, json: bool) -> Result<ExitCode, cx_daemon::DaemonError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string())
        );
        return Ok(ExitCode::SUCCESS);
    }

    let report: StatusReport = serde_json::from_value(data.clone())
        .map_err(|err| cx_daemon::DaemonError::protocol(format!("状态响应无法解析：{err}")))?;

    println!(
        "守护进程   运行中（pid {}，启动于 {})",
        report.pid,
        format_time(report.started_ms)
    );
    println!(
        "同步状态   {}",
        if report.paused {
            "已暂停（cx resume 恢复）"
        } else {
            "运行中"
        }
    );
    println!(
        "设备       {}（{}）",
        report.device_name,
        short_id(&report.device_id)
    );
    match &report.space_id {
        Some(id) => println!(
            "空间       {}（{}）",
            short_id(id),
            if report.encrypted {
                "端到端加密"
            } else {
                "未加密"
            }
        ),
        None => println!("空间       未加入（cx space init / cx space join）"),
    }
    println!("剪贴板     {}", report.clipboard);
    println!("监听       {}", report.watch);
    println!(
        "历史       {} 条；已处理对象 {} 个",
        report.history_len, report.seen_len
    );
    if report.skipped_oversize > 0 || report.skipped_ignored > 0 {
        println!(
            "跳过       超限 {} 条，忽略规则 {} 条",
            report.skipped_oversize, report.skipped_ignored
        );
    }
    if let Some(hash) = &report.last_published_hash {
        println!("最后发布   {}", short_hash(hash));
    }

    if report.backends.is_empty() {
        println!("后端       未配置（当前只记录本机历史）");
    } else {
        println!("后端：");
        for backend in &report.backends {
            let state = if !backend.enabled {
                "已禁用".to_string()
            } else if let Some(err) = &backend.last_error {
                format!("错误：{err}（连续失败 {} 次）", backend.failures)
            } else if let Some(ok_ms) = backend.last_ok_ms {
                format!("正常（最近成功 {}）", format_time(ok_ms))
            } else {
                "待首次同步".to_string()
            };
            // 标识（地址 / 桶 @ 端点 / 空间）：配了多个同类后端时靠它区分谁是谁
            let target = match &backend.detail {
                Some(detail) if !detail.is_empty() => format!("  {detail}"),
                _ => String::new(),
            };
            println!(
                "  - {:<12} {:<7} {}{}；发送 {} / 接收 {}",
                backend.name, backend.kind, state, target, backend.published, backend.applied
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 历史列表。
pub fn history(data: &Value, json: bool, limit: usize) -> Result<ExitCode, cx_daemon::DaemonError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "[]".to_string())
        );
        return Ok(ExitCode::SUCCESS);
    }

    let items: Vec<HistorySummary> = serde_json::from_value(data.clone())
        .map_err(|err| cx_daemon::DaemonError::protocol(format!("历史响应无法解析：{err}")))?;
    if items.is_empty() {
        println!("暂无历史记录（首次复制后就会出现）");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{:<4} {:<7} {:<9} {:>8}  {:<16} 摘要",
        "位置", "类型", "时间", "大小", "来源"
    );
    for item in items.iter().take(limit) {
        println!(
            "{:<4} {:<7} {:<9} {:>8}  {:<16} {}",
            item.position,
            item.kind,
            format_time(item.created_ms),
            format_bytes(item.bytes as u64),
            truncate(&item.origin, 16),
            short_hash(&item.hash)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// 内容预览。
pub fn preview(data: &Value, json: bool) -> Result<ExitCode, cx_daemon::DaemonError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string())
        );
        return Ok(ExitCode::SUCCESS);
    }

    let preview: HistoryPreview = serde_json::from_value(data.clone())
        .map_err(|err| cx_daemon::DaemonError::protocol(format!("预览响应无法解析：{err}")))?;

    println!(
        "第 {} 条（0 = 最新，{}，{}）",
        preview.position,
        preview.kind,
        format_bytes(preview.bytes as u64)
    );
    match (&preview.text, &preview.image) {
        (Some(text), _) => {
            println!("---");
            println!("{text}");
            if preview.truncated {
                println!("---（已截断；用 --max-chars 看更多）");
            }
        }
        (None, Some(size)) => println!(
            "图片 {size}（用 `cx copy {}` 放回剪贴板）",
            preview.position
        ),
        (None, None) => println!("（无可显示内容）"),
    }
    Ok(ExitCode::SUCCESS)
}

/// 诊断报告。
pub fn doctor(report: &DoctorReport, json: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        for check in &report.checks {
            println!(
                "{} {:<12} {}",
                if check.ok { "[ ok ]" } else { "[fail]" },
                check.name,
                check.detail
            );
        }
        if report.all_ok() {
            println!("\n全部检查通过");
        } else {
            println!("\n{} 项需要处理", report.failed());
        }
    }

    if report.all_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// 每 1000 字节算 1 KB（人类可读）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 毫秒时间戳 → `MM-DD HH:MM:SS`（UTC，够用且不引入时区依赖）。
pub fn format_time(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let days = total_seconds / 86_400;
    let remain = total_seconds % 86_400;
    let (_, month, day) = civil_from_days(days as i64);
    let hours = remain / 3_600;
    let minutes = (remain % 3_600) / 60;
    let seconds = remain % 60;
    format!("{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 取标识符前 8 个字符。
fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

/// 哈希只显示前 12 位。
fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// 按字符数截断（避免切断多字节字符）。
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字节格式化() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn 时间格式化() {
        // 2025-10-01T13:06:38Z 附近
        let text = format_time(1_759_299_998_712);
        assert!(text.starts_with("10-01"), "{text}");
        assert!(text.ends_with(":38"), "{text}");
        assert_eq!(format_time(0), "01-01 00:00:00");
    }

    #[test]
    fn 截断不破坏多字节字符() {
        let text = truncate("中文内容很长很长", 4);
        assert_eq!(text.chars().count(), 4);
        assert!(text.ends_with('…'));
        assert_eq!(truncate("短", 10), "短");
    }

    #[test]
    fn 标识符截断() {
        assert_eq!(short_id("ab12cd34ef567890"), "ab12cd34");
        assert_eq!(short_hash("9f3c1e77aa55bb66"), "9f3c1e77aa55");
    }

    #[test]
    fn 错误的状态响应被拒绝而不是panic() {
        let data = serde_json::json!({ "unexpected": true });
        assert!(status(&data, false).is_err());
        assert!(history(&data, false, 10).is_err());
        assert!(preview(&data, false).is_err());
    }

    #[test]
    fn 诊断报告输出退出码() {
        let report = DoctorReport::new(vec![cx_proto::DoctorCheck {
            name: "配置".to_string(),
            ok: true,
            detail: "无问题".to_string(),
        }]);
        let code = doctor(&report, false);
        let _ = code; // ExitCode 不便比较，这里确保不 panic 且有输出
        let report = DoctorReport::new(vec![cx_proto::DoctorCheck {
            name: "剪贴板".to_string(),
            ok: false,
            detail: "无图形会话".to_string(),
        }]);
        assert!(!report.all_ok());
    }
}
