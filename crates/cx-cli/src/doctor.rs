//! 本机诊断（`cx doctor`）。
//!
//! 关键设计：**守护进程没起来时也要能用**。用户最需要诊断的场景恰恰是
//! "守护进程起不来"（没有图形会话、目录权限不对、配置写错）。因此：
//!
//! - 守护进程在跑 → 问它（它能报告剪贴板就绪情况、后端错误、计数器）；
//! - 没在跑 → 就地检查配置、目录、身份、空间、剪贴板可用性、历史完整性。

use crate::output;
use cx_clip::ClipBackend;
use cx_core::{Config, History, Identity, LocalSpace, Paths};
use cx_daemon::{ControlClient, DaemonError, DaemonInfo};
use cx_proto::control::{DoctorCheck, DoctorReport, cmd as commands};
use std::process::ExitCode;

/// 执行诊断。
pub fn run(paths: &Paths, json: bool) -> Result<ExitCode, DaemonError> {
    if let Some(info) = DaemonInfo::load(paths)
        && info.is_alive()
    {
        let mut client = ControlClient::connect(paths)?;
        let data = client.simple(commands::DOCTOR)?;
        let report: DoctorReport = serde_json::from_value(data)
            .map_err(|err| DaemonError::protocol(format!("诊断响应无法解析：{err}")))?;
        return Ok(output::doctor(&report, json));
    }

    let report = local_report(paths);
    Ok(output::doctor(&report, json))
}

/// 就地检查（守护进程未运行）。
fn local_report(paths: &Paths) -> DoctorReport {
    let mut checks = Vec::new();

    // 目录
    match paths.ensure() {
        Ok(()) => checks.push(DoctorCheck {
            name: "目录".to_string(),
            ok: true,
            detail: format!(
                "配置 {} / 数据 {}",
                paths.config_dir.display(),
                paths.data_dir.display()
            ),
        }),
        Err(err) => checks.push(DoctorCheck {
            name: "目录".to_string(),
            ok: false,
            detail: format!("无法创建或访问目录：{err}"),
        }),
    }

    // 配置
    match Config::load(paths) {
        Ok(config) => {
            let problems = config.problems();
            checks.push(DoctorCheck {
                name: "配置".to_string(),
                ok: problems.is_empty(),
                detail: if problems.is_empty() {
                    format!("{}（无问题）", paths.config_file().display())
                } else {
                    problems.join("；")
                },
            });
        }
        Err(err) => checks.push(DoctorCheck {
            name: "配置".to_string(),
            ok: false,
            detail: format!("{err}"),
        }),
    }

    // 身份
    match Identity::load_or_create(paths, None) {
        Ok(identity) => checks.push(DoctorCheck {
            name: "设备身份".to_string(),
            ok: true,
            detail: format!(
                "{}（{}…）",
                identity.name,
                &identity.device_id[..8.min(identity.device_id.len())]
            ),
        }),
        Err(err) => checks.push(DoctorCheck {
            name: "设备身份".to_string(),
            ok: false,
            detail: format!("{err}"),
        }),
    }

    // 空间
    match LocalSpace::load(paths) {
        Ok(space) => checks.push(DoctorCheck {
            name: "空间".to_string(),
            ok: space.meta.is_encrypted(),
            detail: format!(
                "{}（{}）",
                space.short_id(),
                if space.meta.is_encrypted() {
                    "端到端加密"
                } else {
                    "未加密：对象存储里可直接读到内容"
                }
            ),
        }),
        Err(err) => checks.push(DoctorCheck {
            name: "空间".to_string(),
            ok: false,
            detail: format!("{err}（cx space init 创建，或 cx space join 加入）"),
        }),
    }

    // 密钥文件权限：这是"安全"里最容易出错、也最容易被忽略的一项——
    // 空间密钥被人读到，等于所有剪贴板内容都被人读到。
    checks.push(secret_permissions_check(paths));

    // 剪贴板
    match cx_clip::SystemClipboard::new() {
        Ok(clipboard) => {
            let caps = clipboard.capabilities();
            checks.push(DoctorCheck {
                name: "剪贴板".to_string(),
                ok: caps.text,
                detail: format!(
                    "{}；平台事件通知：{}",
                    caps.note,
                    if caps.watch {
                        "可用"
                    } else {
                        "不可用（轮询兜底）"
                    }
                ),
            });
        }
        Err(err) => checks.push(DoctorCheck {
            name: "剪贴板".to_string(),
            ok: false,
            detail: format!("{err}"),
        }),
    }

    // 历史
    match History::open(paths) {
        Ok(history) => checks.push(DoctorCheck {
            name: "历史存储".to_string(),
            ok: history.skipped_frames() == 0,
            detail: format!(
                "{} 条，{} 字节；坏帧 {} 个",
                history.len(),
                history.log_size(),
                history.skipped_frames()
            ),
        }),
        Err(err) => checks.push(DoctorCheck {
            name: "历史存储".to_string(),
            ok: false,
            detail: format!("{err}"),
        }),
    }

    // 守护进程
    checks.push(DoctorCheck {
        name: "守护进程".to_string(),
        ok: false,
        detail: "未运行（cx start 后台启动，或 cx daemon --foreground 前台调试）".to_string(),
    });

    // 自启
    checks.push(DoctorCheck {
        name: "登录自启".to_string(),
        ok: true,
        detail: cx_daemon::autostart_status(),
    });

    DoctorReport::new(checks)
}

/// 检查密钥文件的权限。
///
/// Unix 上要求 0600（只有本人可读写）；Windows 上权限由 ACL 表达，
/// 这里只确认文件在用户的配置/数据目录下（不做 ACL 解析，避免给出假的安全感）。
fn secret_permissions_check(paths: &Paths) -> DoctorCheck {
    let mut watched: Vec<std::path::PathBuf> = vec![
        paths.space_key_file(),
        paths.control_token_file(),
        paths.config_dir.join("lan.key"),
    ];
    // secrets/ 下的口令、令牌、S3 私钥
    if let Ok(entries) = std::fs::read_dir(paths.secrets_dir()) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                watched.push(entry.path());
            }
        }
    }

    // 非 Unix 平台上不会往里 push（权限由 ACL 表达），因此那里的 mut 是多余的
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut complaints: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for path in &watched {
        if !path.exists() {
            continue;
        }
        checked += 1;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(path) {
                Ok(meta) => {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        complaints.push(format!(
                            "{} 权限是 {mode:o}（应为 600）",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ));
                    }
                }
                Err(err) => complaints.push(format!("{} 无法读取元数据：{err}", path.display())),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    let detail = if checked == 0 {
        "还没有需要保护的密钥文件".to_string()
    } else if complaints.is_empty() {
        #[cfg(unix)]
        {
            format!("{checked} 个密钥文件都是 600")
        }
        #[cfg(not(unix))]
        {
            format!("{checked} 个密钥文件都在用户目录下（Windows 权限由 ACL 表达）")
        }
    } else {
        complaints.join("；")
    };

    DoctorCheck {
        name: "密钥权限".to_string(),
        ok: complaints.is_empty(),
        detail,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("cx-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 未运行时也能给出诊断() {
        let paths = temp_paths("local");
        let report = local_report(&paths);
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        for expected in [
            "目录",
            "配置",
            "设备身份",
            "空间",
            "剪贴板",
            "历史存储",
            "守护进程",
        ] {
            assert!(
                names.contains(&expected),
                "缺少检查项 {expected}：{names:?}"
            );
        }
        // 未加入空间 + 守护进程未运行 ⇒ 必然有失败项
        assert!(!report.all_ok());
        let daemon = report.checks.iter().find(|c| c.name == "守护进程").unwrap();
        assert!(daemon.detail.contains("cx start"));
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 坏配置被报告为失败() {
        let paths = temp_paths("badconfig");
        std::fs::write(paths.config_file(), "[[[ 不是 toml").unwrap();
        let report = local_report(&paths);
        let config = report.checks.iter().find(|c| c.name == "配置").unwrap();
        assert!(!config.ok);
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
