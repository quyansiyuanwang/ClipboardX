//! 用户级自启注册。
//!
//! **三端都必须是用户级**，这是硬约束而不是偷懒：
//! - Windows 服务运行在 Session 0，没有交互式桌面，读不到剪贴板；
//! - macOS LaunchDaemon 同理（系统会话里没有 `NSPasteboard`）；
//! - Linux system 服务没有 `DISPLAY` / `WAYLAND_DISPLAY` 与会话总线。
//!
//! 因此：Windows 写 `HKCU\...\Run`；macOS 写用户 LaunchAgent；Linux 写
//! systemd 用户单元并跟随 `graphical-session.target`。三者都不需要管理员权限。

use crate::error::DaemonError;
use cx_core::Paths;

/// 自启项名称。
pub const AUTOSTART_NAME: &str = "ClipboardX";

/// 当前可执行文件路径（自启命令里必须写绝对路径）。
fn exe_path() -> Result<String, DaemonError> {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .map_err(DaemonError::io)
}

/// Windows 自启项里要写的完整命令（`"<exe>" start --silent`）。
///
/// 仅 Windows 使用：macOS 写 LaunchAgent 的 `ProgramArguments`、Linux 写 systemd 单元的
/// `ExecStart`，两者都直接引用可执行文件，而不是 `start` 子命令。
/// 用 `#[cfg(windows)]` 而不是 `#[allow(dead_code)]`：用不到的平台不该把它编进来，
/// 否则 `clippy -D warnings` 会在 Unix 上判它 dead code。
#[cfg(windows)]
fn start_command() -> Result<String, DaemonError> {
    Ok(format!("\"{}\" start --silent", exe_path()?))
}

// ---------------------------------------------------------------- Windows

/// Windows：写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
#[cfg(windows)]
pub fn install(_paths: &Paths) -> Result<String, DaemonError> {
    let command = start_command()?;
    let output = std::process::Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            AUTOSTART_NAME,
            "/t",
            "REG_SZ",
            "/d",
            &command,
            "/f",
        ])
        .output()
        .map_err(DaemonError::io)?;
    if !output.status.success() {
        return Err(DaemonError::Service(format!(
            "reg add 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(format!(
        "已写入 HKCU\\...\\Run（{AUTOSTART_NAME} = {command}）；下次登录自动在后台运行"
    ))
}

/// Windows：删除 Run 键。
#[cfg(windows)]
pub fn uninstall(_paths: &Paths) -> Result<String, DaemonError> {
    let output = std::process::Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            AUTOSTART_NAME,
            "/f",
        ])
        .output()
        .map_err(DaemonError::io)?;
    if output.status.success() {
        Ok("已删除 HKCU\\...\\Run 中的自启项".to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unable to find") || stderr.contains("找不到") {
            Ok("自启项本就不存在".to_string())
        } else {
            Err(DaemonError::Service(format!(
                "reg delete 失败：{}",
                stderr.trim()
            )))
        }
    }
}

/// Windows：查询自启状态。
#[cfg(windows)]
pub fn status() -> String {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            AUTOSTART_NAME,
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let value = text
                .lines()
                .find(|line| line.contains(AUTOSTART_NAME))
                .map(|line| line.trim().to_string())
                .unwrap_or_else(|| "已注册".to_string());
            format!("已启用（{value}）")
        }
        _ => "未启用".to_string(),
    }
}

// ---------------------------------------------------------------- macOS

/// macOS：写用户 LaunchAgent。
#[cfg(target_os = "macos")]
pub fn install(_paths: &Paths) -> Result<String, DaemonError> {
    let home = std::env::var("HOME").map_err(|_| DaemonError::Service("缺少 HOME".to_string()))?;
    let dir = std::path::PathBuf::from(&home)
        .join("Library")
        .join("LaunchAgents");
    std::fs::create_dir_all(&dir).map_err(DaemonError::io)?;
    let plist = dir.join("com.clipboardx.agent.plist");
    let exe = exe_path()?;
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.clipboardx.agent</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>daemon</string><string>--foreground</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>5</integer>
</dict>
</plist>
"#
    );
    cx_core::fsutil::write_atomic(&plist, body.as_bytes())?;

    // 立即装载（失败不算致命：下次登录也会加载）
    let uid = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .map_err(DaemonError::io)?
            .stdout,
    )
    .unwrap_or_default()
    .trim()
    .to_string();
    let loaded = std::process::Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist.display().to_string(),
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    Ok(format!(
        "已写入 {}（{}）；下次登录自动运行",
        plist.display(),
        if loaded {
            "并已立即装载"
        } else {
            "将在下次登录加载"
        }
    ))
}

/// macOS：卸载 LaunchAgent。
#[cfg(target_os = "macos")]
pub fn uninstall(_paths: &Paths) -> Result<String, DaemonError> {
    let home = std::env::var("HOME").map_err(|_| DaemonError::Service("缺少 HOME".to_string()))?;
    let plist = std::path::PathBuf::from(&home)
        .join("Library")
        .join("LaunchAgents")
        .join("com.clipboardx.agent.plist");
    let uid = String::from_utf8(
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .map_err(DaemonError::io)?
            .stdout,
    )
    .unwrap_or_default()
    .trim()
    .to_string();
    let _ = std::process::Command::new("launchctl")
        .args([
            "bootout",
            &format!("gui/{uid}"),
            &plist.display().to_string(),
        ])
        .output();
    match std::fs::remove_file(&plist) {
        Ok(()) => Ok(format!("已移除 {}", plist.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok("LaunchAgent 本就不存在".to_string())
        }
        Err(err) => Err(DaemonError::io(err)),
    }
}

/// macOS：查询状态。
#[cfg(target_os = "macos")]
pub fn status() -> String {
    let Ok(home) = std::env::var("HOME") else {
        return "无法确定 HOME".to_string();
    };
    let plist = std::path::PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join("com.clipboardx.agent.plist");
    if plist.exists() {
        format!("已启用（{}）", plist.display())
    } else {
        "未启用".to_string()
    }
}

// ---------------------------------------------------------------- Linux

/// Linux：写 systemd 用户单元。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn install(_paths: &Paths) -> Result<String, DaemonError> {
    let home = std::env::var("HOME").map_err(|_| DaemonError::Service("缺少 HOME".to_string()))?;
    let dir = std::path::PathBuf::from(&home)
        .join(".config")
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&dir).map_err(DaemonError::io)?;
    let unit = dir.join("clipboardx.service");
    let exe = exe_path()?;

    let body = format!(
        "[Unit]\n\
         Description=ClipboardX 剪贴板同步守护进程\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} daemon --foreground\n\
         PassEnvironment=DISPLAY WAYLAND_DISPLAY XAUTHORITY XDG_RUNTIME_DIR XDG_SESSION_TYPE DBUS_SESSION_BUS_ADDRESS\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         Nice=5\n\
         MemoryMax=256M\n\
         CPUQuota=25%\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    );
    cx_core::fsutil::write_atomic(&unit, body.as_bytes())?;

    let enabled = std::process::Command::new("systemctl")
        .args(["--user", "enable", "clipboardx.service"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    Ok(format!(
        "已写入 {}（{}）；桌面登录后随 graphical-session.target 启动",
        unit.display(),
        if enabled {
            "并已 systemctl --user enable"
        } else {
            "请手动执行 systemctl --user enable clipboardx.service"
        }
    ))
}

/// Linux：移除用户单元。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn uninstall(_paths: &Paths) -> Result<String, DaemonError> {
    let home = std::env::var("HOME").map_err(|_| DaemonError::Service("缺少 HOME".to_string()))?;
    let unit = std::path::PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join("clipboardx.service");
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "clipboardx.service"])
        .output();
    match std::fs::remove_file(&unit) {
        Ok(()) => Ok(format!("已移除 {}", unit.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok("用户单元本就不存在".to_string())
        }
        Err(err) => Err(DaemonError::io(err)),
    }
}

/// Linux：查询状态。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn status() -> String {
    let Ok(home) = std::env::var("HOME") else {
        return "无法确定 HOME".to_string();
    };
    let unit = std::path::PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user")
        .join("clipboardx.service");
    if !unit.exists() {
        return "未启用".to_string();
    }
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "clipboardx.service"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    format!("已启用（{}，状态 {active}）", unit.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn 启动命令是绝对路径且带_start() {
        let command = start_command().unwrap();
        assert!(command.contains("\" start --silent"), "命令格式：{command}");
        assert!(command.starts_with('"'));
    }

    #[test]
    fn 状态查询不panic() {
        // 各平台都应返回一段说明文本（未启用时也要有输出）
        let text = status();
        assert!(!text.is_empty());
    }

    #[test]
    fn 自启项名称与_msi_一致() {
        // MSI 里写的是 HKCU\...\Run 的 ClipboardX；改名要保持一致
        assert_eq!(AUTOSTART_NAME, "ClipboardX");
    }
}
