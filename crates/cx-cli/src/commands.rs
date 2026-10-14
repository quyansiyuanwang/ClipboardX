//! 子命令定义与分发。

use crate::{doctor, output};
use clap::Subcommand;
use cx_core::{Config, Identity, LocalSpace, Paths};
use cx_daemon::{ControlClient, Daemon, DaemonError, DaemonOptions};
use cx_proto::control::cmd as commands;
use serde_json::{Map, json};
use std::process::ExitCode;

/// 顶层子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// 打印版本与协议信息
    Version {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 前台运行守护进程（调试用；systemd / LaunchAgent 也调用它）
    Daemon {
        /// 显式声明前台运行（`daemon` 本来就是前台；保留该参数让安装脚本与文档自解释）
        #[arg(long)]
        foreground: bool,
        /// 纯中继模式：不读写本机剪贴板（无图形会话时使用）
        #[arg(long)]
        no_clipboard: bool,
        /// 覆盖轮询间隔（毫秒）
        #[arg(long)]
        poll_interval_ms: Option<u64>,
        /// 不把日志输出到 stderr（只写日志文件）
        #[arg(long)]
        quiet: bool,
    },

    /// 在后台启动守护进程（分离进程，关掉终端也不受影响）
    Start {
        /// 纯中继模式
        #[arg(long)]
        no_clipboard: bool,
        /// 不打印任何输出（登录自启时使用）
        #[arg(long)]
        silent: bool,
    },

    /// 停止后台守护进程
    Stop,

    /// 重启守护进程
    Restart {
        /// 纯中继模式
        #[arg(long)]
        no_clipboard: bool,
    },

    /// 查看运行状态（设备、后端、计数）
    Status {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 本机诊断（守护进程起不来时也能运行）
    Doctor {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 列出剪贴板历史
    History {
        /// 条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 查看某条历史的内容预览（图片只显示尺寸）
    Show {
        /// 位置：0 = 最新
        position: usize,
        /// 文本预览的最大字符数
        #[arg(long, default_value_t = 2000)]
        max_chars: usize,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 把历史内容写回本机剪贴板
    Copy {
        /// 位置：0 = 最新
        position: usize,
    },

    /// 删除一条本机历史（只动本机：不删后端对象、不影响其他设备）
    Delete {
        /// 位置：0 = 最新
        position: usize,
    },

    /// 立即发布内容（默认发布当前剪贴板）
    Push {
        /// 直接指定文本
        #[arg(long)]
        text: Option<String>,
        /// 从标准输入读取文本
        #[arg(long)]
        stdin: bool,
        /// 即使与上次发布内容相同也重新发布
        #[arg(long)]
        force: bool,
    },

    /// 立即拉取一次远端内容
    Pull,

    /// 暂停同步（不再发布，也不应用远端内容）
    Pause,

    /// 恢复同步
    Resume,

    /// 重新读取配置文件
    Reload,

    /// 配置相关
    Config {
        /// 子命令
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// 登录自启注册
    Service {
        /// 子命令
        #[command(subcommand)]
        command: ServiceCommand,
    },

    /// 空间（同步密钥）管理
    Space {
        /// 子命令
        #[command(subcommand)]
        command: SpaceCommand,
    },

    /// 同步后端管理（自建服务端 / WebDAV / S3）
    Backend {
        /// 子命令
        #[command(subcommand)]
        command: BackendCommand,
    },

    /// 打开终端界面
    Tui {
        /// 只渲染一帧到标准输出后退出（不需要终端；给脚本与 CI 验证用）
        #[arg(long)]
        snapshot: bool,
    },
}

/// `cx config` 的子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    /// 打印配置文件路径
    Path,
    /// 打印当前配置（含默认值）
    Show,
    /// 打印配置问题清单（等同 doctor 的配置部分）
    Check,
}

/// `cx backend` 的子命令。
///
/// 只改配置文件与凭据文件，**不碰网络**（`test` 除外，它为了能在守护进程未运行时
/// 也能排障而直接连一次）。
#[derive(Debug, Clone, Subcommand)]
pub enum BackendCommand {
    /// 列出已配置的后端
    List {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 添加后端
    Add {
        /// 后端种类与其参数
        #[command(subcommand)]
        kind: BackendAddCommand,
    },
    /// 移除后端（连同它的凭据文件）
    Remove {
        /// 后端名
        name: String,
    },
    /// 测试连通性（直连一次，不需要守护进程在运行）
    Test {
        /// 后端名
        name: String,
    },
}

/// `cx backend add` 的子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum BackendAddCommand {
    /// WebDAV（Nextcloud / ownCloud / Apache mod_dav / nginx dav）
    Webdav {
        /// 后端名（唯一）
        name: String,
        /// 集合 URL（Nextcloud 形如 https://cloud.example.com/remote.php/dav/files/<用户>/clipboardx）
        url: String,
        /// 用户名（匿名 WebDAV 可省略）
        #[arg(long)]
        username: Option<String>,
        /// 从标准输入读入口令（避免出现在命令行历史里）
        #[arg(long)]
        password_stdin: bool,
        /// 自定义 CA 证书文件名（放在 secrets/ 下）
        #[arg(long)]
        ca_file: Option<String>,
        /// 跳过 TLS 校验（仅调试）
        #[arg(long)]
        insecure: bool,
    },
    /// 自建服务端（cx-server）
    Server {
        /// 后端名（唯一）
        name: String,
        /// 服务端基址（例如 http://192.168.1.10:8688）
        url: String,
        /// 空间 id（`cx-server space create` 输出）
        #[arg(long)]
        space: String,
        /// 从标准输入读设备令牌（管理员用 `cx-server token issue` 签发）
        #[arg(long)]
        token_stdin: bool,
        /// 注册密钥（给了就直接自助注册，不用 --token-stdin）
        #[arg(long)]
        enroll_secret: Option<String>,
        /// 自定义 CA 证书文件名
        #[arg(long)]
        ca_file: Option<String>,
        /// 跳过 TLS 校验（仅调试）
        #[arg(long)]
        insecure: bool,
    },
    /// S3 兼容对象存储（AWS S3 / MinIO / Cloudflare R2 / 阿里云 OSS）
    S3 {
        /// 后端名（唯一）
        name: String,
        /// 桶名
        #[arg(long)]
        bucket: String,
        /// 自定义端点（MinIO/R2 等；AWS 官方端点请省略）
        #[arg(long)]
        endpoint: Option<String>,
        /// 区域（默认 us-east-1）
        #[arg(long)]
        region: Option<String>,
        /// 键前缀（建议形如 clipboardx/<空间>）
        #[arg(long)]
        prefix: Option<String>,
        /// 访问密钥 id
        #[arg(long)]
        access_key_id: String,
        /// 从标准输入读私有密钥（避免出现在命令行历史里）
        #[arg(long)]
        secret_stdin: bool,
        /// 用 path-style 寻址（MinIO 通常需要；省略时按"自建端点即 path-style"推断）
        #[arg(long)]
        path_style: bool,
        /// 自定义 CA 证书文件名
        #[arg(long)]
        ca_file: Option<String>,
        /// 跳过 TLS 校验（仅调试）
        #[arg(long)]
        insecure: bool,
    },
}

/// `cx service` 的子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommand {
    /// 注册登录自启
    Install,
    /// 取消登录自启
    Uninstall,
    /// 查看自启状态
    Status,
}

/// `cx space` 的子命令。
#[derive(Debug, Clone, Subcommand)]
pub enum SpaceCommand {
    /// 创建新空间（口令经标准输入传入）
    Init {
        /// 不加密（对象存储里可直接读到内容，仅调试用）
        #[arg(long)]
        no_encrypt: bool,
        /// 从标准输入读取口令（不提示）
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// 把本机空间元数据上传到某个后端（首次创建空间后执行一次）
    Publish {
        /// 后端名
        backend: String,
    },
    /// 从某个后端加入已有空间（需要口令）
    Join {
        /// 后端名
        backend: String,
        /// 从标准输入读取口令（不提示）
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// 打印本机空间信息
    Show {
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },
    /// 导出空间密钥（用于备份，请妥善保管）
    ExportKey,
}

/// 分发执行。
pub fn dispatch(paths: &Paths, command: Command) -> Result<ExitCode, DaemonError> {
    match command {
        Command::Version { json } => {
            output::version(json);
            Ok(ExitCode::SUCCESS)
        }

        Command::Daemon {
            foreground,
            no_clipboard,
            poll_interval_ms,
            quiet,
        } => {
            // `--foreground` 只是显式声明（daemon 子命令本身就是前台运行）
            let _ = foreground;
            run_daemon(paths, no_clipboard, poll_interval_ms, quiet)
        }

        Command::Start {
            no_clipboard,
            silent,
        } => {
            let pid = cx_daemon::spawn_background(paths, no_clipboard)?;
            if !silent {
                println!("守护进程已在后台运行（pid {pid}）");
                println!("用 `cx status` 查看状态，`cx stop` 停止。");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Stop => {
            if cx_daemon::stop_running(paths)? {
                println!("守护进程已停止");
            } else {
                println!("守护进程未运行");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Restart { no_clipboard } => {
            let was_running = cx_daemon::stop_running(paths)?;
            let pid = cx_daemon::spawn_background(paths, no_clipboard)?;
            println!(
                "{}（pid {pid}）",
                if was_running {
                    "已重启守护进程"
                } else {
                    "守护进程原未运行，现已启动"
                }
            );
            Ok(ExitCode::SUCCESS)
        }

        Command::Status { json } => {
            let mut client = ControlClient::connect(paths)?;
            let data = client.simple(commands::STATUS)?;
            output::status(&data, json)
        }

        Command::Doctor { json } => doctor::run(paths, json),

        Command::History { limit, json } => {
            let mut client = ControlClient::connect(paths)?;
            let mut args = Map::new();
            args.insert("limit".to_string(), json!(limit));
            let data = client.request(commands::HISTORY_LIST, args)?;
            output::history(&data, json, limit)
        }

        Command::Show {
            position,
            max_chars,
            json,
        } => {
            let mut client = ControlClient::connect(paths)?;
            let mut args = Map::new();
            args.insert("position".to_string(), json!(position));
            args.insert("max_chars".to_string(), json!(max_chars));
            let data = client.request(commands::HISTORY_SHOW, args)?;
            output::preview(&data, json)
        }

        Command::Copy { position } => {
            let mut client = ControlClient::connect(paths)?;
            let mut args = Map::new();
            args.insert("position".to_string(), json!(position));
            client.request(commands::COPY, args)?;
            println!("已把第 {position} 条（0 = 最新）写回剪贴板");
            Ok(ExitCode::SUCCESS)
        }

        Command::Delete { position } => {
            let mut client = ControlClient::connect(paths)?;
            let mut args = Map::new();
            args.insert("position".to_string(), json!(position));
            client.request(commands::HISTORY_DELETE, args)?;
            // 删除只作用于本机历史：把边界说清楚，别让人以为别的设备上也没了
            println!(
                "已删除第 {position} 条（0 = 最新）的本机历史。\n\
                 注意：这是本机动作——后端对象与其他设备上的副本不受影响。"
            );
            Ok(ExitCode::SUCCESS)
        }

        Command::Push { text, stdin, force } => {
            if text.is_some() && stdin {
                return Err(DaemonError::Protocol(
                    "--text 与 --stdin 只能选一个".to_string(),
                ));
            }
            let config = Config::load(paths).unwrap_or_default();
            let content = match (text, stdin) {
                (Some(text), _) => Some(text),
                (None, true) => Some(read_stdin_text(config.sync.max_item_bytes)?),
                (None, false) => None,
            };

            let mut client = ControlClient::connect(paths)?;
            let mut args = Map::new();
            if let Some(text) = &content {
                args.insert("text".to_string(), json!(text));
            }
            if force {
                args.insert("force".to_string(), json!(true));
            }
            client.request(commands::PUSH, args)?;
            match content {
                Some(text) => println!("已发布 {} 字节的文本", text.len()),
                None => println!("已请求发布当前剪贴板内容"),
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Pull => {
            let mut client = ControlClient::connect(paths)?;
            client.simple(commands::PULL)?;
            println!("已触发一次拉取");
            Ok(ExitCode::SUCCESS)
        }

        Command::Pause => {
            let mut client = ControlClient::connect(paths)?;
            client.simple(commands::PAUSE)?;
            println!("已暂停同步（`cx resume` 恢复）");
            Ok(ExitCode::SUCCESS)
        }

        Command::Resume => {
            let mut client = ControlClient::connect(paths)?;
            client.simple(commands::RESUME)?;
            println!("已恢复同步");
            Ok(ExitCode::SUCCESS)
        }

        Command::Reload => {
            let mut client = ControlClient::connect(paths)?;
            client.simple(commands::RELOAD)?;
            println!("配置已重载（部分设置需重启守护进程才生效）");
            Ok(ExitCode::SUCCESS)
        }

        Command::Backend { command } => run_backend(paths, command),

        Command::Config { command } => match command {
            ConfigCommand::Path => {
                println!("{}", paths.config_file().display());
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Show => {
                let config = Config::load(paths)?;
                let text = config.to_toml()?;
                print!("{text}");
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Check => {
                let config = Config::load(paths)?;
                let problems = config.problems();
                if problems.is_empty() {
                    println!("配置无问题");
                    Ok(ExitCode::SUCCESS)
                } else {
                    for problem in &problems {
                        println!("- {problem}");
                    }
                    Ok(ExitCode::FAILURE)
                }
            }
        },

        Command::Service { command } => match command {
            ServiceCommand::Install => {
                println!("{}", cx_daemon::install_autostart(paths)?);
                Ok(ExitCode::SUCCESS)
            }
            ServiceCommand::Uninstall => {
                println!("{}", cx_daemon::uninstall_autostart(paths)?);
                Ok(ExitCode::SUCCESS)
            }
            ServiceCommand::Status => {
                println!("自启：{}", cx_daemon::autostart_status());
                Ok(ExitCode::SUCCESS)
            }
        },

        Command::Space { command } => match command {
            SpaceCommand::Init {
                no_encrypt,
                passphrase_stdin,
            } => {
                let passphrase = if no_encrypt {
                    None
                } else if passphrase_stdin {
                    Some(read_stdin_text(4096)?.trim().to_string())
                } else {
                    return Err(DaemonError::Protocol(
                        "请用 `--passphrase-stdin` 提供口令（避免出现在命令历史里），\
                         或显式 `--no-encrypt`"
                            .to_string(),
                    ));
                };
                if let Some(passphrase) = &passphrase
                    && passphrase.is_empty()
                {
                    return Err(DaemonError::Protocol("口令不能为空".to_string()));
                }
                let space = LocalSpace::create(paths, passphrase.as_deref())?;
                println!("已创建空间 {}", space.short_id());
                println!(
                    "加密：{}",
                    if space.cipher.space_key().is_some() {
                        "XChaCha20-Poly1305"
                    } else {
                        "未加密"
                    }
                );
                println!("下一步：用 `cx backend add server|webdav|s3` 配置同步后端。");
                Ok(ExitCode::SUCCESS)
            }
            SpaceCommand::Publish { backend } => ok_or_failure(publish_space(paths, &backend)?),
            SpaceCommand::Join {
                backend,
                passphrase_stdin,
            } => {
                let passphrase = if passphrase_stdin {
                    Some(read_stdin_text(4096)?.trim().to_string())
                } else {
                    None
                };
                if let Some(text) = &passphrase
                    && text.is_empty()
                {
                    return Err(DaemonError::Protocol("口令不能为空".to_string()));
                }
                ok_or_failure(join_space(paths, &backend, passphrase.as_deref())?)
            }
            SpaceCommand::Show { json } => {
                let space = LocalSpace::load(paths)?;
                if json {
                    println!("{}", space.meta.to_json()?);
                } else {
                    println!("空间 id：{}", space.meta.space_id);
                    println!(
                        "加密：{}",
                        if space.meta.is_encrypted() {
                            "XChaCha20-Poly1305"
                        } else {
                            "未加密"
                        }
                    );
                    println!("元数据：{}", paths.space_file().display());
                }
                Ok(ExitCode::SUCCESS)
            }
            SpaceCommand::ExportKey => {
                let space = LocalSpace::load(paths)?;
                match space.cipher.space_key() {
                    Some(key) => {
                        println!("{}", key.to_hex());
                        eprintln!("注意：这是空间密钥，任何人拿到它都能解密你的剪贴板内容。");
                        Ok(ExitCode::SUCCESS)
                    }
                    None => Err(DaemonError::Protocol("该空间未加密，没有密钥".to_string())),
                }
            }
        },

        Command::Tui { snapshot } => {
            if snapshot {
                // 不需要终端：渲染一帧到 stdout 就退出（脚本与 CI 用它验证界面读到了什么）
                cx_tui::print_snapshot(paths)
                    .map_err(|err| DaemonError::Protocol(format!("渲染快照失败：{err}")))?;
                return Ok(ExitCode::SUCCESS);
            }
            // 从快捷方式启动时守护进程通常还没起来。这里先确保它在跑，
            // 否则界面会停在"未连接"上，用户看到的就是"黑窗一闪"。
            ensure_daemon_running(paths)?;
            match cx_tui::run(paths)
                .map_err(|err| DaemonError::Protocol(format!("终端界面退出：{err}")))?
            {
                cx_tui::TuiExit::Quit => Ok(ExitCode::SUCCESS),
                cx_tui::TuiExit::ToggleBackend(name) => {
                    let enabled = set_backend_enabled(paths, &name)?;
                    let state = if enabled { "已启用" } else { "已停用" };
                    println!("后端 {name} {state}");
                    if enabled {
                        println!("（内容会重新同步到它；首次同步可能拉取较多历史）");
                    } else {
                        println!("（已上传到它的对象不会被删除；其他后端不受影响）");
                    }
                    restart_daemon(paths)?;
                    Ok(ExitCode::SUCCESS)
                }
                cx_tui::TuiExit::RemoveBackend(name) => {
                    remove_backend(paths, &name)?;
                    println!("已移除后端 {name}（已上传的对象不在本机，未做删除）");
                    restart_daemon(paths)?;
                    Ok(ExitCode::SUCCESS)
                }
                cx_tui::TuiExit::AddBackend(draft) => {
                    // 界面只负责收集，落盘与自测在这里做（同一进程内的库调用）
                    add_backend_from_draft(paths, &draft)?;
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}

/// `cx daemon`：前台运行。
fn run_daemon(
    paths: &Paths,
    no_clipboard: bool,
    poll_interval_ms: Option<u64>,
    quiet: bool,
) -> Result<ExitCode, DaemonError> {
    paths.ensure()?;
    // 尽早初始化日志：后台自启时 stderr 是丢弃的，配置写错这类**启动期失败**
    // 只有记进日志文件才可能被看到（否则表现为"进程起来又不见了"）。
    cx_daemon::logging::init(paths, None, !quiet)?;

    let config = match Config::load_or_create(paths) {
        Ok(config) => config,
        Err(err) => {
            log::error!("配置无法加载：{err}");
            log::error!(
                "配置文件：{}（可执行 `cx config check` 查看问题）",
                paths.config_file().display()
            );
            return Err(err.into());
        }
    };
    let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
    let space = match LocalSpace::load(paths) {
        Ok(space) => Some(space),
        Err(err) => {
            log::info!("本机还没有空间（{err}），先以本机历史模式运行");
            None
        }
    };

    let options = DaemonOptions {
        no_clipboard,
        stderr_logs: !quiet,
        poll_interval_ms,
    };
    let daemon = Daemon::new(paths.clone(), config, identity, space, options);
    let code = daemon.run()?;
    Ok(ExitCode::from(code as u8))
}

/// 从标准输入读取文本（有上限，避免误操作把巨大文件塞进剪贴板通道）。
fn read_stdin_text(limit: u64) -> Result<String, DaemonError> {
    use std::io::Read;
    let stdin = std::io::stdin();
    let mut buffer = Vec::new();
    stdin
        .lock()
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(DaemonError::io)?;
    if buffer.len() as u64 > limit {
        return Err(DaemonError::Protocol(format!(
            "标准输入超过 {limit} 字节上限"
        )));
    }
    String::from_utf8(buffer)
        .map_err(|_| DaemonError::Protocol("标准输入不是合法 UTF-8".to_string()))
}

/// 按名字构造一个后端（`backend test` / `space publish` / `space join` 共用）。
fn build_named_backend(
    paths: &Paths,
    name: &str,
) -> Result<Box<dyn cx_backend::RemoteBackend>, DaemonError> {
    let config = Config::load(paths)?;
    let Some(section) = config.backends.iter().find(|backend| backend.name == name) else {
        return Err(DaemonError::Protocol(format!(
            "没有名为 {name} 的后端（用 `cx backend list` 查看）"
        )));
    };
    cx_backend::build_backend(section, paths, std::time::Duration::from_secs(30))
        .map_err(|err| DaemonError::Protocol(format!("无法构造后端 {name}：{err}")))
}

/// 把本机空间元数据上传到某个后端。
fn publish_space(paths: &Paths, backend: &str) -> Result<bool, DaemonError> {
    let space = LocalSpace::load(paths)?;
    let mut remote = build_named_backend(paths, backend)?;
    let json = space.meta.to_json()?;
    cx_backend::store_space_meta(&mut *remote, &json)
        .map_err(|err| DaemonError::Protocol(format!("上传空间元数据失败：{err}")))?;
    println!(
        "已把空间元数据上传到后端 {backend}（空间 {}）",
        space.short_id()
    );
    println!("其他设备现在可以用 `cx space join {backend} --passphrase-stdin` 加入。");
    Ok(true)
}

/// 从某个后端加入已有空间。
fn join_space(paths: &Paths, backend: &str, passphrase: Option<&str>) -> Result<bool, DaemonError> {
    let mut remote = build_named_backend(paths, backend)?;
    let json = cx_backend::load_space_meta(&mut *remote)
        .map_err(|err| DaemonError::Protocol(format!("读取空间元数据失败：{err}")))?
        .ok_or_else(|| {
            DaemonError::Protocol(format!(
                "后端 {backend} 上还没有空间元数据（先在已加入的设备上执行 `cx space publish {backend}`）"
            ))
        })?;
    let space = LocalSpace::join(paths, &json, passphrase)?;
    println!("已加入空间 {}", space.short_id());
    println!(
        "加密：{}",
        if space.cipher.space_key().is_some() {
            "XChaCha20-Poly1305"
        } else {
            "未加密"
        }
    );
    Ok(true)
}
/// `cx backend` 的实现。
///
/// 设计取舍：这里**只改配置与凭据文件**，不碰网络（`Test` 例外——它存在的意义
/// 就是"守护进程没起来时也能排障"，因此直接连一次）。附带的好处是"添加后端"
/// 不需要守护进程在运行，安装脚本可以先写好配置再启动。
/// 确保守护进程在运行（TUI 入口用）。
///
/// 已经连得上就直接返回；连不上就按 `cx start` 的方式拉起一个后台守护进程，
/// 最多等 5 秒等它开始监听。
/// 翻转某个后端的启用状态，返回新状态。
///
/// 停用只是不在这次运行里为它起 worker：已上传的对象不动、其他后端不受影响，
/// 想恢复按同一个键即可，所以不需要"确认"这一层。
fn set_backend_enabled(paths: &Paths, name: &str) -> Result<bool, DaemonError> {
    let mut config = Config::load_or_create(paths)?;
    let Some(backend) = config
        .backends
        .iter_mut()
        .find(|backend| backend.name == name)
    else {
        return Err(DaemonError::Protocol(format!("没有名为 {name} 的后端")));
    };
    backend.enabled = !backend.enabled;
    let enabled = backend.enabled;
    config.save(paths)?;
    Ok(enabled)
}

/// 从配置里移除一个后端（界面里移除后端的落地口）。
fn remove_backend(paths: &Paths, name: &str) -> Result<(), DaemonError> {
    let mut config = Config::load_or_create(paths)?;
    let before = config.backends.len();
    config.backends.retain(|backend| backend.name != name);
    if config.backends.len() == before {
        return Err(DaemonError::Protocol(format!("没有名为 {name} 的后端")));
    }
    config.save(paths)?;
    Ok(())
}

/// 重启守护进程（后端 worker 是启动时创建的，改配置后必须重启才生效）。
fn restart_daemon(paths: &Paths) -> Result<(), DaemonError> {
    if cx_daemon::stop_running(paths)? {
        println!("已重启守护进程以应用改动");
    }
    let _ = cx_daemon::spawn_background(paths, false);
    Ok(())
}

/// 把 TUI 表单收集到的草稿落成配置（界面里添加后端的落地口）。
///
/// 与 `cx backend add` 走同一套核心逻辑：写 `secrets/` 下的凭据文件 + 追加
/// `[[backend]]` 段。差别只是凭据来自表单而不是标准输入。
fn add_backend_from_draft(
    paths: &Paths,
    draft: &cx_tui::TuiBackendDraft,
) -> Result<(), DaemonError> {
    use cx_core::config::{BackendKind, BackendSection};
    use cx_tui::WizardKind;

    let name = draft.name().trim().to_string();
    if name.is_empty() {
        return Err(DaemonError::Protocol("后端名不能为空".to_string()));
    }
    let mut config = Config::load_or_create(paths)?;
    if config.backends.iter().any(|backend| backend.name == name) {
        return Err(DaemonError::Protocol(format!(
            "已经有名为 {name} 的后端了（用 `cx backend remove {name}` 先删掉）"
        )));
    }

    let mut section = BackendSection {
        name: name.clone(),
        kind: match draft.kind {
            WizardKind::Server => BackendKind::Server,
            WizardKind::Webdav => BackendKind::Webdav,
            WizardKind::S3 => BackendKind::S3,
        },
        enabled: true,
        url: None,
        space: None,
        token_file: None,
        username: None,
        password_file: None,
        password_env: None,
        ca_file: None,
        insecure: false,
        bucket: None,
        prefix: None,
        region: None,
        endpoint: None,
        access_key_id: None,
        secret_access_key_file: None,
        secret_access_key_env: None,
        path_style: None,
        poll_interval_ms: None,
    };

    // 凭据一律写进 secrets/（0600），不落进 config.toml
    let mut written: Vec<std::path::PathBuf> = Vec::new();
    let mut save_secret = |label: &str, value: &str| -> Result<String, DaemonError> {
        let file_name = format!("{name}.{label}");
        write_secret_file(paths, &file_name, value)?;
        written.push(paths.secret_file(&file_name));
        Ok(file_name)
    };

    let result = (|| -> Result<(), DaemonError> {
        match draft.kind {
            WizardKind::Server => {
                section.url = Some(draft.get("服务端地址").trim().to_string());
                section.space = Some(draft.get("空间 id").trim().to_string());
                let secret = draft.get("注册密钥").trim();
                if !secret.is_empty() {
                    section.token_file = Some(save_secret("token", secret)?);
                }
            }
            WizardKind::Webdav => {
                section.url = Some(draft.get("URL").trim().to_string());
                let user = draft.get("用户名").trim();
                if !user.is_empty() {
                    section.username = Some(user.to_string());
                }
                let password = draft.get("口令").trim();
                if !password.is_empty() {
                    section.password_file = Some(save_secret("password", password)?);
                } else if user.is_empty() {
                    return Err(DaemonError::Protocol(
                        "匿名 WebDAV 也要留空用户名，但至少填 URL".to_string(),
                    ));
                }
            }
            WizardKind::S3 => {
                section.bucket = Some(draft.get("桶名").trim().to_string());
                let endpoint = draft.get("端点").trim();
                if !endpoint.is_empty() {
                    section.endpoint = Some(endpoint.to_string());
                }
                section.region = Some(draft.get("区域").trim().to_string());
                section.access_key_id = Some(draft.get("AccessKeyId").trim().to_string());
                let secret = draft.get("SecretAccessKey").trim();
                if !secret.is_empty() {
                    section.secret_access_key_file = Some(save_secret("secret", secret)?);
                }
                let prefix = draft.get("前缀").trim();
                if !prefix.is_empty() {
                    section.prefix = Some(prefix.to_string());
                }
            }
        }
        Ok(())
    })();

    if let Err(err) = result {
        // 落盘失败就把已经写下的凭据清掉，别留半截文件
        for path in written {
            let _ = std::fs::remove_file(path);
        }
        return Err(err);
    }

    config.backends.push(section);
    config.save(paths)?;
    println!("已添加后端 {name}（凭据写入 secrets/，权限 0600）");

    // 直连自测：让用户在界面里就能立刻知道配得对不对
    // 真实探测一次（与 `cx backend test` 同一条路径）：配得对不对当场就知道，
    // 而不是等守护进程下一轮轮询才在面板里看到红色。
    match build_named_backend(paths, &name).and_then(|backend| {
        let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
        let mut sync = cx_sync::RemoteSync::new(backend, &identity.device_id, &identity.name);
        sync.probe()
            .map_err(|err| DaemonError::Protocol(err.to_string()))
    }) {
        Ok(summary) => println!("连通性自测：通过（{summary}）"),
        Err(err) => {
            println!("连通性自测：失败——{err}");
            println!("后端已保存，改好后可以再测：cx backend test {name}");
        }
    }

    // 后端 worker 是启动时创建的，因此新增后端要重启守护进程才生效
    restart_daemon(paths)?;
    Ok(())
}
fn ensure_daemon_running(paths: &Paths) -> Result<(), DaemonError> {
    if ControlClient::connect(paths).is_ok() {
        return Ok(());
    }
    cx_daemon::spawn_background(paths, false)?;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if ControlClient::connect(paths).is_ok() {
            return Ok(());
        }
    }
    // 起不来也不拦着进界面：界面会显示"未连接"并持续重试
    Ok(())
}
fn run_backend(paths: &Paths, command: BackendCommand) -> Result<ExitCode, DaemonError> {
    match command {
        BackendCommand::List { json } => {
            let config = Config::load_or_create(paths)?;
            if json {
                let items: Vec<serde_json::Value> = config
                    .backends
                    .iter()
                    .map(|backend| {
                        json!({
                            "name": backend.name,
                            "kind": backend.kind.as_str(),
                            "enabled": backend.enabled,
                            "target": backend_target(backend),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
                );
            } else if config.backends.is_empty() {
                println!("还没有配置任何后端。");
                println!(
                    "例如：cx backend add webdav dav --url https://cloud.example.com/remote.php/dav/files/alice/clipboardx --username alice --password-stdin"
                );
                println!(
                    "      cx backend add s3 oss --bucket clipboardx --endpoint http://127.0.0.1:9000 --access-key-id minioadmin --secret-stdin --path-style"
                );
            } else {
                for backend in &config.backends {
                    let state = if backend.enabled { "启用" } else { "停用" };
                    println!(
                        "{:<12} {:<7} {:<4} {}",
                        backend.name,
                        backend.kind.as_str(),
                        state,
                        backend_target(backend)
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        BackendCommand::Add { kind } => ok_or_failure(add_backend(paths, kind)?),

        BackendCommand::Remove { name } => {
            let mut config = Config::load_or_create(paths)?;
            let before = config.backends.len();
            config.backends.retain(|backend| backend.name != name);
            if config.backends.len() == before {
                eprintln!("没有名为 {name} 的后端（用 `cx backend list` 查看）");
                return Ok(ExitCode::FAILURE);
            }
            config.save(paths)?;
            // 凭据文件只删我们自己两种命名约定下的
            for candidate in [format!("{name}.pass"), format!("{name}.key")] {
                let path = paths.secret_file(&candidate);
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
            }
            println!("已移除后端 {name}");
            println!("提示：`cx reload` 让运行中的守护进程重新读取配置。");
            Ok(ExitCode::SUCCESS)
        }

        BackendCommand::Test { name } => ok_or_failure(test_backend(paths, &name)?),
    }
}

/// 把"成功与否"映射成退出码。
fn ok_or_failure(ok: bool) -> Result<ExitCode, DaemonError> {
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// 后端的一句话描述（`list` 与错误信息复用）。
fn backend_target(backend: &cx_core::BackendSection) -> String {
    match backend.kind {
        cx_core::BackendKind::Webdav | cx_core::BackendKind::Server => backend
            .url
            .clone()
            .unwrap_or_else(|| "（缺 url）".to_string()),
        cx_core::BackendKind::S3 => {
            let endpoint = backend
                .endpoint
                .clone()
                .unwrap_or_else(|| "AWS".to_string());
            let bucket = backend
                .bucket
                .clone()
                .unwrap_or_else(|| "（缺 bucket）".to_string());
            let prefix = backend.prefix.clone().unwrap_or_default();
            format!("{endpoint}/{bucket}/{prefix}")
        }
    }
}

/// 添加后端。
fn add_backend(paths: &Paths, kind: BackendAddCommand) -> Result<bool, DaemonError> {
    let mut config = Config::load_or_create(paths)?;
    let section = match kind {
        BackendAddCommand::Webdav {
            name,
            url,
            username,
            password_stdin,
            ca_file,
            insecure,
        } => {
            let mut section = empty_section(name, cx_core::BackendKind::Webdav);
            section.url = Some(url);
            section.username = username;
            section.ca_file = ca_file;
            section.insecure = insecure;
            if password_stdin {
                if section.username.is_some() {
                    let file = format!("{}.pass", section.name);
                    write_secret_file(paths, &file, &read_secret_stdin("口令")?)?;
                    section.password_file = Some(file);
                } else {
                    eprintln!(
                        "注意：没有 --username 时忽略 --password-stdin（匿名 WebDAV 不需要口令）"
                    );
                }
            }
            section
        }
        BackendAddCommand::Server {
            name,
            url,
            space,
            token_stdin,
            enroll_secret,
            ca_file,
            insecure,
        } => {
            let mut section = empty_section(name, cx_core::BackendKind::Server);
            section.url = Some(url.clone());
            section.space = Some(space.clone());
            section.insecure = insecure;
            let ca_path = ca_file.clone().map(|file| paths.secret_file(&file));
            section.ca_file = ca_file;

            let token = if let Some(secret) = enroll_secret {
                let identity = Identity::load_or_create(paths, None)?;
                let token = cx_backend::enroll(
                    &url,
                    &space,
                    &secret,
                    &identity.name,
                    ca_path,
                    insecure,
                    std::time::Duration::from_secs(30),
                )
                .map_err(|err| DaemonError::Protocol(format!("自助注册失败：{err}")))?;
                println!("自助注册成功，已获取设备令牌");
                token
            } else if token_stdin {
                read_secret_stdin("设备令牌")?
            } else {
                return Err(DaemonError::Protocol(
                    "请用 --token-stdin 提供设备令牌（管理员签发），或用 --enroll-secret 自助注册"
                        .to_string(),
                ));
            };

            let file = format!("{}.token", section.name);
            write_secret_file(paths, &file, &token)?;
            section.token_file = Some(file);
            section
        }
        BackendAddCommand::S3 {
            name,
            bucket,
            endpoint,
            region,
            prefix,
            access_key_id,
            secret_stdin,
            path_style,
            ca_file,
            insecure,
        } => {
            let mut section = empty_section(name, cx_core::BackendKind::S3);
            section.bucket = Some(bucket);
            section.endpoint = endpoint;
            section.region = region;
            section.prefix = prefix;
            section.access_key_id = Some(access_key_id);
            section.ca_file = ca_file;
            section.insecure = insecure;
            // 省略时由后端按"有无自定义端点"推断（自建端点默认 path-style）
            section.path_style = if path_style { Some(true) } else { None };
            if secret_stdin {
                let file = format!("{}.key", section.name);
                write_secret_file(paths, &file, &read_secret_stdin("私有密钥")?)?;
                section.secret_access_key_file = Some(file);
            } else {
                eprintln!(
                    "提示：没给 --secret-stdin；之后可重跑本命令或手写 secret_access_key_file"
                );
            }
            section
        }
    };

    if config
        .backends
        .iter()
        .any(|backend| backend.name == section.name)
    {
        eprintln!(
            "已经有名为 {} 的后端了（先 `cx backend remove {}`）",
            section.name, section.name
        );
        return Ok(false);
    }

    let name = section.name.clone();
    config.backends.push(section);
    // 先校验再落盘：宁可不保存，也不要留下一份自己都通不过的配置
    let problems = config.problems();
    if !problems.is_empty() {
        eprintln!("配置有问题，未保存：");
        for problem in &problems {
            eprintln!("  - {problem}");
        }
        return Ok(false);
    }
    config.save(paths)?;

    println!("已添加后端 {name}");
    println!("下一步：cx backend test {name}   然后 cx reload（或 cx restart）");
    Ok(true)
}

/// 造一个只有 name/kind 的后端配置。
fn empty_section(name: String, kind: cx_core::BackendKind) -> cx_core::BackendSection {
    cx_core::BackendSection {
        name,
        kind,
        enabled: true,
        url: None,
        space: None,
        token_file: None,
        username: None,
        password_file: None,
        password_env: None,
        ca_file: None,
        insecure: false,
        bucket: None,
        prefix: None,
        region: None,
        endpoint: None,
        access_key_id: None,
        secret_access_key_file: None,
        secret_access_key_env: None,
        path_style: None,
        poll_interval_ms: None,
    }
}

/// 从标准输入读一条凭据（去掉首尾空白）。
fn read_secret_stdin(label: &str) -> Result<String, DaemonError> {
    eprint!("请输入{label}（回车结束）：");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| DaemonError::Protocol(format!("读取{label}失败：{err}")))?;
    let value = line.trim().to_string();
    if value.is_empty() {
        return Err(DaemonError::Protocol(format!("{label}不能为空")));
    }
    Ok(value)
}

/// 写入凭据文件（尽量收紧权限）。
fn write_secret_file(paths: &Paths, file_name: &str, value: &str) -> Result<(), DaemonError> {
    let path = paths.secret_file(file_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| DaemonError::Protocol(format!("创建凭据目录失败：{err}")))?;
    }
    cx_core::fsutil::write_atomic(&path, value.as_bytes())
        .map_err(|err| DaemonError::Protocol(format!("写入凭据文件失败：{err}")))?;
    let _ = cx_core::fsutil::tighten_permissions(&path, false);
    Ok(())
}

/// 直连测试一个后端（不需要守护进程在运行）。
fn test_backend(paths: &Paths, name: &str) -> Result<bool, DaemonError> {
    let config = Config::load(paths)?;
    let Some(section) = config.backends.iter().find(|backend| backend.name == name) else {
        eprintln!("没有名为 {name} 的后端（用 `cx backend list` 查看）");
        return Ok(false);
    };

    let backend = cx_backend::build_backend(section, paths, std::time::Duration::from_secs(30))
        .map_err(|err| DaemonError::Protocol(format!("无法构造后端 {name}：{err}")))?;
    let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
    let mut sync = cx_sync::RemoteSync::new(backend, &identity.device_id, &identity.name);

    match sync.probe() {
        Ok(summary) => {
            println!("后端 {name} 可用：{summary}");
            Ok(true)
        }
        Err(err) => {
            eprintln!("后端 {name} 不可用：{err}");
            if err.is_auth() {
                eprintln!("提示：检查用户名/口令（S3 则是 access key 与 secret）是否正确。");
            } else if err.kind() == "config" {
                eprintln!("提示：`cx backend remove {name}` 后用 `cx backend add` 重新添加。");
            } else {
                eprintln!("提示：确认网络可达、地址正确；自建服务端需先建好桶或目录。");
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use cx_backend::s3_testserver::TestS3Server;
    use cx_backend::testserver::TestWebDavServer;

    fn temp_paths(tag: &str) -> Paths {
        let root = std::env::temp_dir().join(format!(
            "cx-cli-{tag}-{}-{}",
            std::process::id(),
            cx_core::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::from_root(&root);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn 界面移除后端会写回配置且重复移除报错() {
        use cx_core::config::{BackendKind, BackendSection};
        let paths = temp_paths("remove-backend");
        let mut config = Config::load_or_create(&paths).unwrap();
        config.backends.push(BackendSection {
            name: "home".to_string(),
            kind: BackendKind::Server,
            enabled: true,
            url: Some("http://127.0.0.1:8688".to_string()),
            space: Some("space-1".to_string()),
            token_file: None,
            username: None,
            password_file: None,
            password_env: None,
            ca_file: None,
            insecure: false,
            bucket: None,
            prefix: None,
            region: None,
            endpoint: None,
            access_key_id: None,
            secret_access_key_file: None,
            secret_access_key_env: None,
            path_style: None,
            poll_interval_ms: None,
        });
        config.save(&paths).unwrap();

        super::remove_backend(&paths, "home").unwrap();
        let after = Config::load(&paths).unwrap();
        assert!(
            !after.backends.iter().any(|backend| backend.name == "home"),
            "移除后配置里不该还有它"
        );

        // 再移除一次应当明确报错，而不是静默成功
        let err = super::remove_backend(&paths, "home").unwrap_err();
        assert!(err.to_string().contains("没有名为 home"), "{err}");
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }
    #[test]
    fn 停用启用后端会翻转并写回配置() {
        use cx_core::config::{BackendKind, BackendSection};
        let paths = temp_paths("toggle-backend");
        let mut config = Config::load_or_create(&paths).unwrap();
        config.backends.push(BackendSection {
            name: "home".to_string(),
            kind: BackendKind::Server,
            enabled: true,
            url: Some("http://127.0.0.1:8688".to_string()),
            space: Some("space-1".to_string()),
            token_file: None,
            username: None,
            password_file: None,
            password_env: None,
            ca_file: None,
            insecure: false,
            bucket: None,
            prefix: None,
            region: None,
            endpoint: None,
            access_key_id: None,
            secret_access_key_file: None,
            secret_access_key_env: None,
            path_style: None,
            poll_interval_ms: None,
        });
        config.save(&paths).unwrap();

        assert!(
            !super::set_backend_enabled(&paths, "home").unwrap(),
            "第一次应当停用"
        );
        let after = Config::load(&paths).unwrap();
        assert!(!after.backends[0].enabled);
        assert!(
            super::set_backend_enabled(&paths, "home").unwrap(),
            "再按一次应当启用"
        );
        assert!(Config::load(&paths).unwrap().backends[0].enabled);

        let err = super::set_backend_enabled(&paths, "nope").unwrap_err();
        assert!(err.to_string().contains("没有名为 nope"), "{err}");
        let _ = std::fs::remove_dir_all(&paths.data_dir);
    }
    #[test]
    fn 后端描述覆盖三种种类() {
        let mut webdav = empty_section("dav".to_string(), cx_core::BackendKind::Webdav);
        webdav.url = Some("https://cloud.example.com/dav".to_string());
        assert_eq!(backend_target(&webdav), "https://cloud.example.com/dav");

        let mut s3 = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        s3.endpoint = Some("http://127.0.0.1:9000".to_string());
        s3.bucket = Some("clipboardx".to_string());
        s3.prefix = Some("space-a".to_string());
        assert_eq!(
            backend_target(&s3),
            "http://127.0.0.1:9000/clipboardx/space-a"
        );

        // 没写端点时显示 AWS
        s3.endpoint = None;
        assert!(backend_target(&s3).starts_with("AWS/clipboardx"));
    }

    #[test]
    fn 添加_webdav_后端会写配置与口令文件() {
        let paths = temp_paths("add-webdav");
        let ok = add_backend(
            &paths,
            BackendAddCommand::Webdav {
                name: "dav".to_string(),
                url: "https://cloud.example.com/remote.php/dav/files/alice/clipboardx".to_string(),
                username: Some("alice".to_string()),
                password_stdin: false,
                ca_file: None,
                insecure: false,
            },
        )
        .unwrap();
        assert!(ok, "应当添加成功");

        let config = Config::load(&paths).unwrap();
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].kind, cx_core::BackendKind::Webdav);
        assert_eq!(config.backends[0].name, "dav");

        // 重复添加应当被拒绝
        let again = add_backend(
            &paths,
            BackendAddCommand::Webdav {
                name: "dav".to_string(),
                url: "https://other.example.com/dav".to_string(),
                username: None,
                password_stdin: false,
                ca_file: None,
                insecure: false,
            },
        )
        .unwrap();
        assert!(!again, "同名后端应当被拒绝");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 添加_s3_后端时缺少密钥会被校验拦下() {
        let paths = temp_paths("add-s3");
        // 没有 --secret-stdin ⇒ 缺少 secret 来源 ⇒ 配置校验不通过 ⇒ 不予保存
        let ok = add_backend(
            &paths,
            BackendAddCommand::S3 {
                name: "oss".to_string(),
                bucket: "clipboardx".to_string(),
                endpoint: Some("http://127.0.0.1:9000".to_string()),
                region: Some("us-east-1".to_string()),
                prefix: Some("space-a".to_string()),
                access_key_id: "minioadmin".to_string(),
                secret_stdin: false,
                path_style: true,
                ca_file: None,
                insecure: false,
            },
        )
        .unwrap();
        assert!(!ok, "缺密钥应当被拦下");
        let config = Config::load(&paths).unwrap();
        assert!(config.backends.is_empty(), "不合法配置不应落盘");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 移除后端会删掉配置与凭据文件() {
        let paths = temp_paths("remove");
        write_secret_file(&paths, "oss.key", "minioadmin").unwrap();
        let mut config = Config::load_or_create(&paths).unwrap();
        let mut section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        section.bucket = Some("clipboardx".to_string());
        section.access_key_id = Some("minioadmin".to_string());
        section.secret_access_key_file = Some("oss.key".to_string());
        config.backends.push(section);
        config.save(&paths).unwrap();
        assert!(paths.secret_file("oss.key").exists());

        let code = run_backend(
            &paths,
            BackendCommand::Remove {
                name: "oss".to_string(),
            },
        )
        .unwrap();
        assert!(matches!(code, _));
        let config = Config::load(&paths).unwrap();
        assert!(config.backends.is_empty());
        assert!(!paths.secret_file("oss.key").exists(), "凭据文件也应被删除");
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }

    #[test]
    fn 直连测试能识别可用的_webdav_与_s3() {
        let paths = temp_paths("probe");

        // WebDAV：对着仓库内的测试服务端
        let dav = TestWebDavServer::start();
        let mut config = Config::load_or_create(&paths).unwrap();
        let mut webdav = empty_section("dav".to_string(), cx_core::BackendKind::Webdav);
        webdav.url = Some(dav.url());
        config.backends.push(webdav);

        // S3：对着会校验签名的测试服务端（桶还不存在，probe 会顺手建出来）
        let s3 = TestS3Server::start("minioadmin", "minioadmin", "clipboardx");
        let mut s3_section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        s3_section.bucket = Some("clipboardx".to_string());
        s3_section.endpoint = Some(s3.endpoint());
        s3_section.access_key_id = Some("minioadmin".to_string());
        s3_section.secret_access_key_env = Some("CX_TEST_S3_SECRET".to_string());
        s3_section.path_style = Some(true);
        config.backends.push(s3_section);
        config.save(&paths).unwrap();

        unsafe { std::env::set_var("CX_TEST_S3_SECRET", "minioadmin") };
        assert!(test_backend(&paths, "dav").unwrap(), "WebDAV 后端应当可用");
        assert!(test_backend(&paths, "oss").unwrap(), "S3 后端应当可用");
        assert_eq!(s3.buckets_created(), 1, "probe 应当建出桶");
        assert!(
            s3.signature_failures().is_empty(),
            "{:?}",
            s3.signature_failures()
        );
        unsafe { std::env::remove_var("CX_TEST_S3_SECRET") };

        // 不存在的后端名：给出明确失败而不是 panic
        assert!(!test_backend(&paths, "nope").unwrap());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
