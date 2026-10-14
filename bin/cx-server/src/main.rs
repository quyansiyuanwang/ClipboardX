//! `cx-server` —— 可自建的 ClipboardX 同步服务端。
//!
//! 与客户端共用 `v1/` 对象布局，只保存密文；不依赖剪贴板与终端界面，
//! 可部署在无图形会话的服务器/容器中。
//!
//! 子命令分两类：
//! - `serve`：跑服务（前台阻塞；systemd / Docker 都按前台进程模型管理）；
//! - `space` / `token` / `devices`：离线管理数据目录（不需要服务在运行）。

use clap::{Parser, Subcommand};
use cx_server::{DEFAULT_LISTEN, Server, ServerConfig, ServerError, Store};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Parser)]
#[command(
    name = "cx-server",
    version,
    about = "ClipboardX 同步服务端（可自建）",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 运行服务（前台阻塞）
    Serve {
        /// 监听地址
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: String,
        /// 数据目录（默认取平台数据目录下的 cx-server）
        #[arg(long)]
        data: Option<PathBuf>,
        /// 并发连接上限
        #[arg(long, default_value_t = cx_server::DEFAULT_MAX_CONNECTIONS)]
        max_connections: usize,
        /// 不打日志到 stderr
        #[arg(long)]
        quiet: bool,
    },

    /// 空间管理
    Space {
        /// 子命令
        #[command(subcommand)]
        command: SpaceCommand,
    },

    /// 设备令牌管理
    Token {
        /// 子命令
        #[command(subcommand)]
        command: TokenCommand,
    },

    /// 列出某空间的设备（不含令牌明文）
    Devices {
        /// 空间 id
        space_id: String,
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },

    /// 打印版本与服务端默认参数
    Version {
        /// 以 JSON 输出，便于脚本消费
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SpaceCommand {
    /// 创建空间
    Create {
        /// 空间名（管理用）
        name: String,
        /// 配额（可写 512MB / 2GiB / 纯字节数）
        #[arg(long, default_value = "512MB")]
        quota: String,
        /// 对象保留天数（0 = 不自动清理）
        #[arg(long, default_value_t = 0)]
        retention_days: u32,
        /// 不生成注册密钥（只能由管理员签发设备令牌）
        #[arg(long)]
        no_enroll: bool,
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },
    /// 列出所有空间
    List {
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },
    /// 显示空间详情
    Show {
        /// 空间 id
        space_id: String,
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// 签发设备令牌（不用自助注册时）
    Issue {
        /// 空间 id
        space_id: String,
        /// 设备名
        #[arg(long)]
        device_name: String,
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },
    /// 吊销设备令牌（从标准输入读令牌，避免进命令行历史）
    Revoke {
        /// 空间 id
        space_id: String,
        /// 数据目录
        #[arg(long)]
        data: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cx-server: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<ExitCode, ServerError> {
    match command {
        Command::Version { json } => Ok(print_version(json)),
        Command::Serve {
            listen,
            data,
            max_connections,
            quiet,
        } => serve(&listen, data, max_connections, quiet),
        Command::Space { command } => space(command),
        Command::Token { command } => token(command),
        Command::Devices { space_id, data } => devices(&space_id, data),
    }
}

/// 解析数据目录：`--data` > `CX_SERVER_DATA` > 平台数据目录下的 `cx-server`。
fn resolve_data(explicit: Option<PathBuf>) -> Result<PathBuf, ServerError> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Ok(value) = std::env::var("CX_SERVER_DATA")
        && !value.trim().is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let paths = cx_core::Paths::resolve()
        .map_err(|err| ServerError::Internal(format!("无法确定平台数据目录：{err}")))?;
    Ok(paths.data_dir.join(cx_server::DEFAULT_DATA_SUBDIR))
}

fn serve(
    listen: &str,
    data: Option<PathBuf>,
    max_connections: usize,
    quiet: bool,
) -> Result<ExitCode, ServerError> {
    let data_dir = resolve_data(data)?;
    let listen: std::net::SocketAddr = listen
        .parse()
        .map_err(|err| ServerError::BadRequest(format!("监听地址非法：{listen}（{err}）")))?;

    // 服务端要长期跑，日志必须有落点（--quiet 只关 stderr 那一份）
    init_logging(&data_dir, !quiet)?;

    let mut config = ServerConfig::new(listen, &data_dir);
    config.max_connections = max_connections.max(8);
    let server = Server::bind(config)?;
    log::info!(
        "数据目录 {}；并发上限 {}；长轮询上限 {} 秒",
        data_dir.display(),
        max_connections,
        cx_server::LONG_POLL_MAX_SECS
    );
    for space in server.store().list_spaces()? {
        log::info!(
            "空间 {}（{}）：配额 {} 字节，自助注册 {}",
            space.name,
            space.id,
            space.quota_bytes,
            if space.enroll_secret_hash.is_some() {
                "开启"
            } else {
                "关闭"
            }
        );
    }

    // 退出：依赖 Ctrl+C（Windows 控制台 / Unix 终端都一样）。
    // systemd 侧用 StopSignal=SIGINT 覆盖默认的 SIGTERM（见 packaging/linux 的说明）。
    let shutdown = Arc::new(AtomicBool::new(false));
    server.serve(shutdown)?;
    Ok(ExitCode::SUCCESS)
}

fn space(command: SpaceCommand) -> Result<ExitCode, ServerError> {
    match command {
        SpaceCommand::Create {
            name,
            quota,
            retention_days,
            no_enroll,
            data,
        } => {
            let store = Store::open(resolve_data(data)?)?;
            let quota_bytes = parse_size(&quota)?;
            let (meta, enroll) =
                store.create_space(&name, quota_bytes, retention_days, !no_enroll)?;
            println!("空间已创建：{}", meta.id);
            println!("  名称：{}", meta.name);
            println!("  配额：{} 字节", meta.quota_bytes);
            if let Some(enroll) = enroll {
                println!("  注册密钥：{enroll}");
                println!("  （把空间 id 与注册密钥交给设备，设备用 `cx space join` 加入）");
            } else {
                println!(
                    "  自助注册：已关闭（用 `cx-server token issue {} --device-name 设备名` 签发令牌）",
                    meta.id
                );
            }
            println!();
            println!("客户端配置示例：");
            println!(
                "  cx backend add server myserver --url http://<主机>:8688 --space {}",
                meta.id
            );
            Ok(ExitCode::SUCCESS)
        }
        SpaceCommand::List { data } => {
            let store = Store::open(resolve_data(data)?)?;
            let spaces = store.list_spaces()?;
            if spaces.is_empty() {
                println!("还没有空间。用 `cx-server space create 名字` 创建。");
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{:<34} {:<16} {:>12} {:>12}  自助注册",
                "空间 id", "名称", "已用", "配额"
            );
            for space in spaces {
                println!(
                    "{:<34} {:<16} {:>12} {:>12}  {}",
                    space.id,
                    space.name,
                    space.used_bytes,
                    space.quota_bytes,
                    if space.enroll_secret_hash.is_some() {
                        "开"
                    } else {
                        "关"
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        SpaceCommand::Show { space_id, data } => {
            let store = Store::open(resolve_data(data)?)?;
            let Some(space) = store.space(&space_id)? else {
                eprintln!("没有这个空间：{space_id}");
                return Ok(ExitCode::FAILURE);
            };
            println!("空间 id：{}", space.id);
            println!("名称：{}", space.name);
            println!("创建时间：{}", space.created_ms);
            println!("配额：{} 字节", space.quota_bytes);
            println!("已用：{} 字节", store.used_bytes(&space.id)?);
            println!("保留天数：{}", space.retention_days);
            println!(
                "自助注册：{}",
                if space.enroll_secret_hash.is_some() {
                    "开启"
                } else {
                    "关闭"
                }
            );
            println!("设备数：{}", store.devices(&space.id)?.len());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn token(command: TokenCommand) -> Result<ExitCode, ServerError> {
    match command {
        TokenCommand::Issue {
            space_id,
            device_name,
            data,
        } => {
            let store = Store::open(resolve_data(data)?)?;
            let token = store.issue_token(&space_id, &device_name)?;
            println!("{token}");
            eprintln!("已为设备「{device_name}」签发令牌（服务端只保存哈希，请立即保存）");
            Ok(ExitCode::SUCCESS)
        }
        TokenCommand::Revoke { space_id, data } => {
            let store = Store::open(resolve_data(data)?)?;
            eprint!("请粘贴要吊销的令牌（回车结束）：");
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|err| ServerError::Internal(format!("读取令牌失败：{err}")))?;
            let token = line.trim().to_string();
            if token.is_empty() {
                return Err(ServerError::BadRequest("令牌不能为空".to_string()));
            }
            if store.revoke_token(&space_id, &token)? {
                println!("已吊销该令牌");
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!("没有找到匹配的令牌（可能已吊销，或不属于该空间）");
                Ok(ExitCode::FAILURE)
            }
        }
    }
}

fn devices(space_id: &str, data: Option<PathBuf>) -> Result<ExitCode, ServerError> {
    let store = Store::open(resolve_data(data)?)?;
    if store.space(space_id)?.is_none() {
        eprintln!("没有这个空间：{space_id}");
        return Ok(ExitCode::FAILURE);
    }
    let devices = store.devices(space_id)?;
    if devices.is_empty() {
        println!("该空间还没有设备。");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<16} {:>14} {:>14}  公钥",
        "设备名", "注册时间", "最近活动"
    );
    for device in devices {
        println!(
            "{:<16} {:>14} {:>14}  {}",
            device.name,
            device.created_ms,
            if device.last_seen_ms == 0 {
                "-".to_string()
            } else {
                device.last_seen_ms.to_string()
            },
            device.pubkey.as_deref().unwrap_or("-")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn print_version(json: bool) -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    if json {
        let payload = serde_json::json!({
            "name": "cx-server",
            "version": version,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "store_layout": cx_core::STORE_LAYOUT_VERSION,
            "protocol": cx_core::CONTROL_PROTOCOL_VERSION,
            "default_listen": DEFAULT_LISTEN,
            "long_poll_max_secs": cx_server::LONG_POLL_MAX_SECS,
            "max_object_bytes": cx_server::MAX_OBJECT_BYTES,
            "sealed_key": false,
        });
        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("cx-server: 无法序列化版本信息: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("cx-server {version}");
        println!(
            "platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        println!("store layout: v{}", cx_core::STORE_LAYOUT_VERSION);
        println!("default listen: {}", DEFAULT_LISTEN);
        println!("max object: {} 字节", cx_server::MAX_OBJECT_BYTES);
    }
    ExitCode::SUCCESS
}

/// 解析 "512MB" / "2GiB" / "1048576"。
fn parse_size(text: &str) -> Result<u64, ServerError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ServerError::BadRequest("配额不能为空".to_string()));
    }
    let (number, multiplier) = if let Some(value) = trimmed.strip_suffix("GiB") {
        (value, 1024u64 * 1024 * 1024)
    } else if let Some(value) = trimmed.strip_suffix("MiB") {
        (value, 1024 * 1024)
    } else if let Some(value) = trimmed.strip_suffix("KiB") {
        (value, 1024)
    } else if let Some(value) = trimmed.strip_suffix("GB") {
        (value, 1000 * 1000 * 1000)
    } else if let Some(value) = trimmed.strip_suffix("MB") {
        (value, 1000 * 1000)
    } else if let Some(value) = trimmed.strip_suffix("KB") {
        (value, 1000)
    } else if let Some(value) = trimmed.strip_suffix('B') {
        (value, 1)
    } else {
        (trimmed, 1)
    };
    let number: f64 = number
        .trim()
        .parse()
        .map_err(|_| ServerError::BadRequest(format!("配额无法解析：{text}")))?;
    if number < 0.0 {
        return Err(ServerError::BadRequest("配额不能为负".to_string()));
    }
    Ok((number * multiplier as f64) as u64)
}

/// 初始化日志：写文件（轮转交给外部工具），可选同时写 stderr。
fn init_logging(data_dir: &std::path::Path, stderr: bool) -> Result<(), ServerError> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|err| ServerError::io(&log_dir, err))?;
    cx_server::logging::init(&log_dir, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 配额解析() {
        assert_eq!(parse_size("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_size("512MB").unwrap(), 512_000_000);
        assert_eq!(parse_size("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("100KiB").unwrap(), 102_400);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_500_000);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
    }
}
