//! 子命令定义与分发。

use crate::{doctor, output};
use clap::Subcommand;
use cx_core::{Config, Identity, LocalSpace, Paths};
use cx_daemon::{ControlClient, Daemon, DaemonError, DaemonOptions};
use cx_proto::control::cmd as commands;
use serde_json::{Map, json};
use std::path::PathBuf;
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
        /// 跳过交互确认（脚本/无终端环境必须显式给出）
        #[arg(long, short = 'y')]
        yes: bool,
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
    /// 读取一个设置项（不带键名时列出全部可设置项）
    Get {
        /// 键名，例如 sync.poll_active_ms
        key: Option<String>,
    },
    /// 修改一个设置项（代你改 config.toml，写盘前整体校验）
    Set {
        /// 键名，例如 lan.port
        key: String,
        /// 新值；列表类用逗号分隔
        value: String,
    },
    /// 把整套配置（后端、凭据、空间、空间密钥）导出成一个文件
    Export {
        /// 输出路径（默认 ./clipboardx-bundle.json）
        path: Option<PathBuf>,
    },
    /// 从导出文件恢复整套配置（新机器一条命令配好）
    Import {
        /// 文件路径
        path: PathBuf,
        /// 覆盖本机已有配置
        #[arg(long)]
        force: bool,
    },
}

/// `cx backend` 的子命令。
///
/// 普通添加/移除只改配置与凭据；`test` 是只读直连探测，server enrollment
/// 需要显式注册请求。TUI 的保存并测试在配置提交前探测。
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
    /// 修改一个已存在的后端（只改给出的字段）
    Edit {
        /// 后端名
        name: String,
        /// 桶名（S3）
        #[arg(long)]
        bucket: Option<String>,
        /// 端点（S3）
        #[arg(long)]
        endpoint: Option<String>,
        /// 区域（S3）
        #[arg(long)]
        region: Option<String>,
        /// 前缀：两台设备必须一致（对象存放的子目录，可留空表示桶根）
        #[arg(long)]
        prefix: Option<String>,
        /// AccessKeyId（S3）
        #[arg(long)]
        access_key_id: Option<String>,
        /// 改为虚拟主机式寻址（OSS/COS）
        #[arg(long)]
        virtual_host: bool,
        /// 改为路径式寻址（MinIO）
        #[arg(long)]
        path_style: bool,
        /// URL（webdav / server）
        #[arg(long)]
        url: Option<String>,
        /// 用户名（webdav）
        #[arg(long)]
        username: Option<String>,
        /// 从标准输入读取新的私有密钥 / 口令 / 令牌
        #[arg(long)]
        secret_stdin: bool,
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
        /// 从标准输入读 STS 会话令牌（临时凭据用；长期 RAM Key 不需要）
        #[arg(long)]
        session_token_stdin: bool,
        /// 强制 path-style 寻址（MinIO 通常需要；默认自动识别 OSS）
        #[arg(long, conflicts_with = "virtual_host")]
        path_style: bool,
        /// 强制 virtual-hosted 寻址（阿里云 OSS 要求）
        #[arg(long, conflicts_with = "path_style")]
        virtual_host: bool,
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
        /// 已有空间时也重建（会换成新密钥，旧对象再也解不开）
        #[arg(long)]
        force: bool,
        /// 从标准输入读取口令（不提示）
        #[arg(long)]
        passphrase_stdin: bool,
    },
    /// 把本机空间元数据上传到某个后端（首次创建空间后执行一次）
    Publish {
        /// 后端名
        backend: String,
    },
    /// 从某个后端加入已有空间（口令或直接给空间密钥）
    Join {
        /// 后端名
        backend: String,
        /// 从标准输入读取口令（不提示）
        #[arg(long)]
        passphrase_stdin: bool,
        /// 从标准输入读取**空间密钥**（64 位十六进制，见 `cx space export-key`）；
        /// 与 --passphrase-stdin 二选一
        #[arg(long)]
        key_stdin: bool,
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
            let no_clipboard = no_clipboard || running_no_clipboard(paths)?.unwrap_or(false);
            let was_running = cx_daemon::stop_running(paths)?;
            let pid = cx_daemon::spawn_background(paths, no_clipboard)?;
            wait_daemon_ready(paths)?;
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
            let mut args = checked_history_args(&mut client, position)?;
            args.insert("max_chars".to_string(), json!(max_chars));
            let data = client.request(commands::HISTORY_SHOW_CHECKED, args)?;
            output::preview(&data, json)
        }

        Command::Copy { position } => {
            let mut client = ControlClient::connect(paths)?;
            let args = checked_history_args(&mut client, position)?;
            client.request(commands::COPY_CHECKED, args)?;
            println!("已把第 {position} 条（0 = 最新）写回剪贴板");
            Ok(ExitCode::SUCCESS)
        }

        Command::Delete { position, yes } => {
            let mut client = ControlClient::connect(paths)?;
            let args = checked_history_args(&mut client, position)?;
            if !yes {
                use std::io::IsTerminal;
                if !std::io::stdin().is_terminal() {
                    return Err(DaemonError::Protocol(
                        "当前输入不是交互终端，无法确认删除；确认后请显式加 --yes".to_string(),
                    ));
                }
                eprint!(
                    "删除第 {position} 条本机历史 {}？云端及其他设备不受影响。[y/N] ",
                    args.get("expected_name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                );
                let mut answer = String::new();
                std::io::stdin()
                    .read_line(&mut answer)
                    .map_err(DaemonError::io)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    println!("已取消删除");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            client.request(commands::HISTORY_DELETE_CHECKED, args)?;
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
                Some(text) => println!(
                    "已提交 {} 字节文本的发布请求；是否排队/上传请查看状态和日志",
                    text.len()
                ),
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
            ConfigCommand::Export { path } => {
                let out = path.unwrap_or_else(|| PathBuf::from("clipboardx-bundle.json"));
                let bundle = crate::bundle::Bundle::collect(paths, cx_core::now_ms())?;
                crate::bundle::write_file(&out, &bundle)?;
                println!("已导出到 {}", out.display());
                println!(
                    "含 {} 个凭据{}；这个文件等价于空间密钥，请妥善保管",
                    bundle.secrets.len(),
                    if bundle.has_space_key() {
                        "与空间密钥"
                    } else {
                        ""
                    }
                );
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Import { path: file, force } => {
                let bundle = crate::bundle::read_file(&file)?;
                bundle.restore(paths, force)?;
                println!("已从 {} 导入配置", file.display());
                println!("重启守护进程使其生效：cx restart");
                Ok(ExitCode::SUCCESS)
            }
            ConfigCommand::Get { key } => {
                let config = Config::load(paths)?;
                match key {
                    Some(key) => match crate::config_cmd::get(&config, key.trim()) {
                        Some(value) => {
                            println!("{value}");
                            Ok(ExitCode::SUCCESS)
                        }
                        None => Err(DaemonError::Protocol(format!(
                            "不认识的配置键「{key}」；`cx config get`（不带键名）可列出全部可设置项"
                        ))),
                    },
                    // 不带键名：把可设置项连同当前值一起列出来，用户不必先猜键名
                    None => {
                        for key in crate::config_cmd::keys() {
                            let value = crate::config_cmd::get(&config, key).unwrap_or_default();
                            let hint = crate::config_cmd::hint(key).unwrap_or("");
                            println!("{key} = {value}");
                            println!("    {hint}");
                        }
                        Ok(ExitCode::SUCCESS)
                    }
                }
            }
            ConfigCommand::Set { key, value } => {
                let key = key.trim().to_string();
                // 用 load_or_create：配置文件还不存在时，先按默认值建一份再改，
                // 而不是让用户先去跑一遍 cx start 才能设置
                let mut config = Config::load_or_create(paths)?;
                let needs_restart = crate::config_cmd::set(&mut config, &key, &value)
                    .map_err(DaemonError::Protocol)?;
                config.save(paths)?;
                println!("{key} = {}", value.trim());
                if needs_restart {
                    println!("已写入配置；这个设置需要重启守护进程才生效：cx restart");
                } else {
                    println!("已写入配置；`cx reload` 即可让守护进程用上新值（无需重启）");
                }
                Ok(ExitCode::SUCCESS)
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
                force,
            } => {
                let passphrase = if no_encrypt {
                    None
                } else if passphrase_stdin {
                    Some(read_stdin_until(4096, Some("空间口令"))?)
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
                // 已经有空间时**默认拒绝创建**：新建会生成新密钥并覆盖 space.json/space.key，
                // 旧对象从此再也解不开（远端那份也会被 publish 覆盖）。要重建必须显式 --force。
                if paths.space_file().exists() && !force {
                    let existing = LocalSpace::load(paths)
                        .map(|space| space.short_id())
                        .unwrap_or_else(|_| "（读取失败）".to_string());
                    return Err(DaemonError::Protocol(format!(
                        "本机已有空间 {existing}；重建会换成新密钥、已上传的对象再也解不开。\
                         确实要重建请先确认不再需要旧数据，再加 --force"
                    )));
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

                Ok(ExitCode::SUCCESS)
            }
            SpaceCommand::Publish { backend } => ok_or_failure(publish_space(paths, &backend)?),
            SpaceCommand::Join {
                backend,
                passphrase_stdin,
                key_stdin,
            } => {
                if passphrase_stdin && key_stdin {
                    return Err(DaemonError::Protocol(
                        "--passphrase-stdin 与 --key-stdin 只能选一个".to_string(),
                    ));
                }
                if key_stdin {
                    let text = read_stdin_until(4096, Some("空间密钥"))?;
                    let key = parse_space_key(&text)?;
                    ok_or_failure(join_space_with_key(paths, &backend, key)?)
                } else {
                    let passphrase = if passphrase_stdin {
                        Some(read_stdin_until(4096, Some("空间口令"))?)
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
            // 面板里的停用 / 移除 / 添加都在界面内执行：改一次配置不必退出界面
            // 再进来一次（连面板选中位置、诊断内容都一起丢掉）。
            // 出问题（配置写不进去、重启失败）也只回填一行提示，界面继续留在原地。
            let mut admin = TuiBackendAdmin { paths };
            let _ = cx_tui::run_with_admin(paths, &mut cx_tui::App::new(), Some(&mut admin))
                .map_err(|err| DaemonError::Protocol(format!("终端界面退出：{err}")))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// 把后端管理动作接到命令行层：界面只负责按键，落盘与重启守护进程在这里做。
struct TuiBackendAdmin<'a> {
    paths: &'a Paths,
}

impl cx_tui::BackendAdmin for TuiBackendAdmin<'_> {
    /// 面板里按 C：列出可设置的配置项（键名 + 当前值）。
    fn config_items(&mut self) -> Result<Vec<(String, String)>, String> {
        let config = Config::load(self.paths).map_err(|err| err.to_string())?;
        Ok(crate::config_cmd::keys()
            .into_iter()
            .map(|key| {
                (
                    key.to_string(),
                    crate::config_cmd::get(&config, key).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// 面板里改一个配置项：与 `cx config set` 走同一个 set（含范围检查与写盘校验）。
    fn set_config(&mut self, key: &str, value: &str) -> Result<String, String> {
        let mut config = Config::load(self.paths).map_err(|err| err.to_string())?;
        let needs_restart = crate::config_cmd::set(&mut config, key, value)?;
        config.save(self.paths).map_err(|err| err.to_string())?;
        let shown = crate::config_cmd::get(&config, key).unwrap_or_default();
        Ok(if needs_restart {
            format!("{key} = {shown}（重启后生效）")
        } else {
            format!("{key} = {shown}")
        })
    }

    /// 面板里按 R：重启守护进程（等价 `cx restart`）。
    fn restart_daemon(&mut self) -> Result<String, String> {
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok("守护进程已重启".to_string())
    }
    /// 面板里按 E：把某个后端的现值装进表单，供用户改。
    fn edit_draft(&mut self, name: &str) -> Result<cx_tui::TuiBackendDraft, String> {
        let config = Config::load(self.paths).map_err(|err| err.to_string())?;
        let section = config
            .backends
            .iter()
            .find(|section| section.name == name)
            .ok_or_else(|| format!("没有名为 {name} 的后端"))?;
        Ok(draft_from_section(section))
    }

    /// 面板里保存对已有后端的修改：与新增走同一条保存/自测路径，只是目标已存在。
    fn update(
        &mut self,
        original: &str,
        draft: &cx_tui::TuiBackendDraft,
    ) -> Result<String, String> {
        update_draft_with_probe(self.paths, original, draft, probe_section)
            .map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(format!("已保存 {original}，守护进程已重启"))
    }

    /// 面板里按 P：把空间元数据重新发布到该后端（等价 `cx space publish`）。
    fn publish_space(&mut self, backend: &str) -> Result<String, String> {
        if !self.paths.space_file().exists() {
            return Err("本机还没有空间，没有可发布的元数据".to_string());
        }
        publish_space(self.paths, backend).map_err(|err| err.to_string())?;
        Ok(format!("已把空间元数据发布到 {backend}"))
    }

    /// 面板里按 O：整套配置导出到配置目录，返回路径。
    fn export_bundle(&mut self) -> Result<String, String> {
        let path = self.paths.config_dir.join("clipboardx-bundle.json");
        let bundle = crate::bundle::Bundle::collect(self.paths, cx_core::now_ms())
            .map_err(|err| err.to_string())?;
        crate::bundle::write_file(&path, &bundle).map_err(|err| err.to_string())?;
        Ok(format!(
            "已导出到 {}（含凭据与空间密钥，0600）",
            path.display()
        ))
    }

    /// 面板里按 I：从配置目录里的包恢复（覆盖本机配置，界面已先确认）。
    fn import_bundle(&mut self) -> Result<String, String> {
        let path = self.paths.config_dir.join("clipboardx-bundle.json");
        let bundle = crate::bundle::read_file(&path).map_err(|err| err.to_string())?;
        bundle
            .restore(self.paths, true)
            .map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(format!("已从 {} 导入，守护进程已重启", path.display()))
    }
    fn toggle(&mut self, name: &str) -> Result<String, String> {
        let enabled = set_backend_enabled(self.paths, name).map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(if enabled {
            format!("后端 {name} 已启用")
        } else {
            format!("后端 {name} 已停用，云端对象和局域网设置未改变")
        })
    }

    fn remove(&mut self, name: &str) -> Result<String, String> {
        remove_backend(self.paths, name).map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(format!("后端 {name} 已移除，云端对象未删除"))
    }

    fn add(&mut self, draft: &cx_tui::BackendDraft) -> Result<String, String> {
        add_backend_from_draft(self.paths, draft).map_err(|err| err.to_string())?;
        Ok(format!("后端 {} 已保存并自测通过", draft.name().trim()))
    }

    /// 面板里按 e：导出空间密钥（与 `cx space export-key` 同一个值）。
    fn export_key(&mut self) -> Result<String, String> {
        let space = LocalSpace::load(self.paths).map_err(|err| err.to_string())?;
        space
            .cipher
            .space_key()
            .map(|key| key.to_hex())
            .ok_or_else(|| "该空间未加密，没有密钥".to_string())
    }

    /// 密钥页按 x：把空间密钥写进一个只有本人可读的文件，返回路径。
    ///
    /// 为什么不是"复制到剪贴板"：剪贴板本身就是被同步的东西，把主密钥放进去
    /// 等于把它写进本机历史并上传到每个后端——这是最不该做的搬法。
    fn write_key_file(&mut self) -> Result<String, String> {
        let key = self.export_key()?;
        let path = self.paths.config_dir.join("space-key.txt");
        cx_core::fsutil::write_atomic(&path, key.as_bytes()).map_err(|err| err.to_string())?;
        // 0600：这个文件等价于空间密钥本身
        cx_core::fsutil::tighten_permissions(&path, false).map_err(|err| err.to_string())?;
        Ok(format!(
            "{}（权限 0600，用 `cat` 或文本编辑器打开；用完请删除）",
            path.display()
        ))
    }

    /// 面板里按 i：用空间密钥加入已有空间（先试解远端条目验证密钥）。
    fn join_space(&mut self, backend: &str, key: &str) -> Result<String, String> {
        let parsed = parse_space_key(key).map_err(|err| err.to_string())?;
        join_space_with_key(self.paths, backend, parsed).map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(format!("已加入 {backend} 上的空间，守护进程已重启"))
    }

    /// 面板里按 s：本机建加密空间 + 把元数据发布到该后端。
    ///
    /// 口令由这里随机生成并**在结果里回显一次**——界面里让用户当场想强口令不现实，
    /// 留空又会退化成明文上传。用户拿这句口令去 `cx space join`，也可以
    /// `cx space export-key` 备份空间密钥。
    fn create_space(&mut self, backend: &str, force: bool) -> Result<String, String> {
        // 重建（force）由界面上的确认页保证"用户确实点了同意"；
        // 没带 force 时仍然拒绝覆盖——这条和命令行 `cx space init` 完全一致。
        if !force && self.paths.space_file().exists() {
            let existing = LocalSpace::load(self.paths)
                .map(|space| space.short_id())
                .unwrap_or_else(|_| "（读取失败）".to_string());
            return Err(format!(
                "本机已有空间 {existing}；重建会换成新密钥、已上传的对象再也解不开。\
                 确实要重建请用命令行：cx space init --force"
            ));
        }
        // 36 个十六进制字符的随机口令：够强，也够短到能抄下来
        let passphrase = cx_core::random_passphrase().map_err(|err| err.to_string())?;
        let space =
            LocalSpace::create(self.paths, Some(&passphrase)).map_err(|err| err.to_string())?;
        publish_space(self.paths, backend).map_err(|err| err.to_string())?;
        restart_daemon(self.paths).map_err(|err| err.to_string())?;
        Ok(format!(
            "已建加密空间 {} 并上传到 {backend}；空间口令（加第二台设备要用，请记下）：{passphrase}",
            space.short_id()
        ))
    }
}

fn checked_history_args(
    client: &mut ControlClient,
    position: usize,
) -> Result<Map<String, serde_json::Value>, DaemonError> {
    if position >= 10_000 {
        return Err(DaemonError::Protocol("历史位置超出可查询上限".into()));
    }
    let mut query = Map::new();
    query.insert("limit".into(), json!(position + 1));
    let data = client.request(commands::HISTORY_LIST, query)?;
    let items: Vec<cx_proto::control::HistorySummary> = serde_json::from_value(data)
        .map_err(|_| DaemonError::Protocol("历史响应无法解析".into()))?;
    let item = items
        .get(position)
        .ok_or_else(|| DaemonError::Protocol("历史位置不存在".into()))?;
    let mut args = Map::new();
    args.insert("position".into(), json!(position));
    args.insert("expected_name".into(), json!(item.name));
    Ok(args)
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
    let space = load_optional_space(paths)?;

    let options = DaemonOptions {
        no_clipboard,
        stderr_logs: !quiet,
        poll_interval_ms,
    };
    let daemon = Daemon::new(paths.clone(), config, identity, space, options);
    let code = daemon.run()?;
    Ok(ExitCode::from(code as u8))
}

fn load_optional_space(paths: &Paths) -> Result<Option<LocalSpace>, DaemonError> {
    // An orphan key or unreadable/corrupt metadata is not a first-run state.
    if cx_core::fsutil::read_to_string_optional(&paths.space_file())?.is_none() {
        if cx_core::fsutil::read_to_string_optional(&paths.space_key_file())?.is_some() {
            return Err(DaemonError::Protocol(
                "空间元数据缺失但密钥仍存在；请恢复空间备份，不会退回明文同步".to_string(),
            ));
        }
        log::info!("尚未创建或加入空间，仅运行本机历史；加密同步需先初始化空间");
        return Ok(None);
    }
    LocalSpace::load(paths).map(Some).map_err(Into::into)
}

/// 从标准输入读取文本（有上限，避免误操作把巨大文件塞进剪贴板通道）。
fn read_stdin_text(limit: u64) -> Result<String, DaemonError> {
    read_stdin_until(limit, None)
}

/// 读标准输入，返回原始文本（不去换行——调用方自己决定要不要 trim）。
///
/// **终端里读一行，管道/重定向里读到 EOF**。之前一律读到 EOF，于是
/// `cx space join <后端> --key-stdin` 在终端里粘完密钥按回车毫无反应——
/// 它在等 EOF（Windows 上得按 Ctrl+Z 再回车），看起来就是"卡住退不出来"。
fn read_stdin_until(limit: u64, prompt: Option<&str>) -> Result<String, DaemonError> {
    use std::io::{BufRead, IsTerminal, Read, Write};

    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        if let Some(prompt) = prompt {
            // 提示走 stderr：stdout 留给命令输出，脚本重定向时不会混进数据
            eprint!("{prompt}: ");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(DaemonError::io)?;
        if line.len() as u64 > limit {
            return Err(DaemonError::Protocol(format!(
                "标准输入超过 {limit} 字节上限"
            )));
        }
        return Ok(strip_line_terminator(line));
    }

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

    Ok(true)
}

/// 解析用户粘进来的空间密钥（64 位十六进制；容忍空白与换行）。
fn parse_space_key(text: &str) -> Result<cx_core::SpaceKey, DaemonError> {
    let trimmed = text.trim();
    cx_core::SpaceKey::from_hex(trimmed).map_err(|_| {
        DaemonError::Protocol(format!(
            "空间密钥应当是 64 位十六进制（在已加入的设备上 `cx space export-key` 可得到），\
             当前是 {} 个字符",
            trimmed.chars().count()
        ))
    })
}

/// 用空间密钥加入（口令方案之外的替代路径）。
fn join_space_with_key(
    paths: &Paths,
    backend: &str,
    key: cx_core::SpaceKey,
) -> Result<bool, DaemonError> {
    let mut remote = build_named_backend(paths, backend)?;
    let json = cx_backend::load_space_meta(&mut *remote)
        .map_err(|err| DaemonError::Protocol(format!("读取空间元数据失败：{err}")))?
        .ok_or_else(|| DaemonError::Protocol(space_meta_missing_message(paths, backend)))?;
    let space = LocalSpace::join_with_key(paths, &json, key)?;
    // 立刻校验这把钥匙对不对：拿远端任意一个条目试着解开。
    // 否则用户要等到同步报错才发现密钥抄错了，而那时空间已经被写下来了。
    // 校验的取样必须来自**清单里引用的条目**：桶里可能还留着上一个空间遗留的孤儿对象，
    // 那些用任何新密钥都解不开，拿它们做校验只会误报"密钥错了"。
    let mut remote = build_named_backend(paths, backend)?;
    let mut probe: Option<(String, cx_proto::Item)> = None;
    'outer: for manifest in remote
        .list(cx_backend::layout::MANIFESTS_PREFIX)
        .map_err(|err| DaemonError::Protocol(format!("列举远端清单失败：{err}")))?
    {
        let Some(bytes) = remote
            .get(&manifest.path)
            .map_err(|err| DaemonError::Protocol(format!("读取远端清单失败：{err}")))?
        else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(manifest) = cx_proto::Manifest::from_json(text) else {
            continue;
        };
        for entry in manifest.items.iter().rev() {
            let Some(path) = cx_backend::layout::item_path(&entry.name) else {
                continue;
            };
            let Some(bytes) = remote
                .get(&path)
                .map_err(|err| DaemonError::Protocol(format!("读取远端条目失败：{err}")))?
            else {
                continue;
            };
            if let Ok(item) = cx_proto::Item::decode(&bytes) {
                probe = Some((entry.name.clone(), item));
                break 'outer;
            }
        }
    }
    if let Some((name, item)) = probe {
        if space.cipher.open(&name, &item).is_err() {
            // 密钥不对：把刚写下的空间删掉，别留下一个"看起来配好了"的坏状态
            let _ = std::fs::remove_file(paths.space_file());
            let _ = std::fs::remove_file(paths.space_key_file());
            return Err(DaemonError::Protocol(
                "这把空间密钥解不开远端已有条目：多半是抄错了，或者两台机器不在同一个空间。\
                 已撤销本次加入，请重新 `cx space export-key` 核对"
                    .to_string(),
            ));
        }
        println!(
            "已用空间密钥加入 {}（并验证过能解开远端条目）",
            space.short_id()
        );
    } else {
        println!("已用空间密钥加入 {}", space.short_id());
        println!("远端还没有任何条目，暂时无法验证这把钥匙（等第一台设备上传后自然会验证）");
    }
    println!("加密：XChaCha20-Poly1305");
    println!("cx start 后生效");
    Ok(true)
}
/// "找不到空间元数据"到底该怎么说。
///
/// 实测最常见的不是"没上传"，而是**这台机器的前缀/桶和另一台不一致**：
/// 于是它去 `…/clip/cx-space.json` 找不到，而对象其实在 `…/clip/cx-space.json`。
/// 所以这里必须把"去哪里找的"写出来，否则用户只会反复 publish 却找不到原因。
fn space_meta_missing_message(paths: &Paths, backend: &str) -> String {
    let location = Config::load(paths)
        .ok()
        .and_then(|config| {
            config
                .backends
                .iter()
                .find(|section| section.name == backend)
                .map(backend_target)
        })
        .unwrap_or_else(|| "（读不到该后端配置）".to_string());
    format!(
        "后端 {backend} 上找不到 cx-space.json：查的是 {location}/cx-space.json。\n\
         先确认这台机器的桶/前缀与另一台一致（cx backend list 对照）；\
         一致的话，在已加入的设备上执行 cx space publish {backend}。\n\
         更省事的做法：另一台执行 cx config export，这台 cx config import <文件>，连密钥一起搬过来。"
    )
}
/// 从某个后端加入已有空间。
fn join_space(paths: &Paths, backend: &str, passphrase: Option<&str>) -> Result<bool, DaemonError> {
    let mut remote = build_named_backend(paths, backend)?;
    let json = cx_backend::load_space_meta(&mut *remote)
        .map_err(|err| DaemonError::Protocol(format!("读取空间元数据失败：{err}")))?
        .ok_or_else(|| DaemonError::Protocol(space_meta_missing_message(paths, backend)))?;
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

/// 校验后端名。
///
/// 名字会成为凭据文件名（`secrets/<名>.<后缀>`），因此这条规则必须
/// **在写任何文件之前**执行；含路径分隔符或 `..` 的名字否则可能把凭据写到
/// `secrets/` 之外。两条添加入口都调用它，规则只有一份。
fn check_backend_name(name: &str) -> Result<(), DaemonError> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(DaemonError::Protocol(format!(
            "后端名不合法（不能为空，也不能含路径分隔符或 ..）：{name}"
        )));
    }
    Ok(())
}

/// Only credential references are owned by a backend; CA files are never removed.
fn credential_files(section: &cx_core::BackendSection) -> impl Iterator<Item = &str> {
    [
        section.token_file.as_deref(),
        section.password_file.as_deref(),
        section.secret_access_key_file.as_deref(),
    ]
    .into_iter()
    .flatten()
}

fn referenced_paths(paths: &Paths, section: &cx_core::BackendSection) -> Vec<std::path::PathBuf> {
    credential_files(section)
        .chain(section.ca_file.as_deref())
        .map(|file| paths.secret_file(file))
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

/// Remove only exact, unshared credential references, never filename prefixes or CA files.
fn remove_backend(paths: &Paths, name: &str) -> Result<(), DaemonError> {
    let mut config = Config::load(paths)?;
    let index = config
        .backends
        .iter()
        .position(|backend| backend.name == name)
        .ok_or_else(|| DaemonError::Protocol(format!("没有名为 {name} 的后端")))?;
    let removed = config.backends.remove(index);
    let protected: std::collections::HashSet<_> = config
        .backends
        .iter()
        .flat_map(|section| referenced_paths(paths, section))
        .chain(removed.ca_file.as_deref().map(|file| {
            let path = paths.secret_file(file);
            std::fs::canonicalize(&path).unwrap_or(path)
        }))
        .collect();
    config.save(paths)?;
    let mut failures = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for file in credential_files(&removed) {
        let path = paths.secret_file(file);
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if protected.contains(&identity) || !visited.insert(identity) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => failures.push(format!("{}：{err}", path.display())),
        }
    }
    if !failures.is_empty() {
        return Err(DaemonError::Protocol(format!(
            "后端已移除，但凭据清理失败：{}",
            failures.join("；")
        )));
    }
    Ok(())
}

fn running_no_clipboard(paths: &Paths) -> Result<Option<bool>, DaemonError> {
    let mut client = match ControlClient::connect(paths) {
        Ok(client) => client,
        Err(err) => {
            if cx_daemon::DaemonInfo::load(paths).is_some_and(|info| info.is_alive()) {
                return Err(err);
            }
            return Ok(None);
        }
    };
    let status = client.simple(commands::STATUS)?;
    status
        .get("no_clipboard")
        .and_then(serde_json::Value::as_bool)
        .map(Some)
        .ok_or_else(|| DaemonError::Protocol("无法确认守护进程的剪贴板模式，取消重启".to_string()))
}

fn wait_daemon_ready(paths: &Paths) -> Result<(), DaemonError> {
    for _ in 0..50 {
        if let Ok(mut client) = ControlClient::connect(paths)
            && client.simple(commands::STATUS).is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(DaemonError::Protocol(
        "守护进程启动后未在 5 秒内就绪；请检查运行日志".to_string(),
    ))
}

/// Restart only a running daemon, preserving its relay/clipboard mode.
fn restart_daemon(paths: &Paths) -> Result<(), DaemonError> {
    restart_with_mode(
        running_no_clipboard(paths)?,
        || cx_daemon::stop_running(paths).map(|_| ()),
        |mode| cx_daemon::spawn_background(paths, mode).map(|_| ()),
        || wait_daemon_ready(paths),
    )
}

fn restart_with_mode(
    mode: Option<bool>,
    stop: impl FnOnce() -> Result<(), DaemonError>,
    spawn: impl FnOnce(bool) -> Result<(), DaemonError>,
    ready: impl FnOnce() -> Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    let Some(mode) = mode else {
        return Ok(());
    };
    stop()?;
    spawn(mode)?;
    ready()?;
    println!("已重启守护进程以应用改动");
    Ok(())
}

/// 把界面表单草稿映射成后端配置段（新增与编辑共用一份，避免两条路径走岔）。
///
/// `is_new` 只影响"服务端注册密钥"这一项：新增时必须填（要靠它换设备令牌），
/// 编辑时留空表示沿用已有令牌。
fn draft_to_section(
    draft: &cx_tui::TuiBackendDraft,
    is_new: bool,
) -> Result<(cx_core::BackendSection, Vec<(String, String)>), DaemonError> {
    use cx_core::BackendKind;
    use cx_tui::WizardKind;
    let name = draft.name().trim().to_string();
    check_backend_name(&name)?;
    let kind = match draft.kind {
        WizardKind::Server => BackendKind::Server,
        WizardKind::Webdav => BackendKind::Webdav,
        WizardKind::S3 => BackendKind::S3,
    };
    let mut section = empty_section(name.clone(), kind);
    let mut secrets = Vec::new();
    match draft.kind {
        WizardKind::Server => {
            section.url = Some(draft.get("服务端地址").trim().to_string());
            section.space = Some(draft.get("空间 id").trim().to_string());
            section.token_file = Some(format!("{name}.token"));
            if is_new && draft.get("注册密钥").is_empty() {
                return Err(DaemonError::Protocol(
                    "请填写注册密钥以获取设备令牌".to_string(),
                ));
            }
        }
        WizardKind::Webdav => {
            section.url = Some(draft.get("URL").trim().to_string());
            section.username = nonempty(draft.get("用户名"));
            let password = draft.get("口令");
            if !password.is_empty() {
                let file = format!("{name}.password");
                section.password_file = Some(file.clone());
                secrets.push((file, password.to_string()));
            }
        }
        WizardKind::S3 => {
            section.bucket = Some(draft.get("桶名").trim().to_string());
            section.endpoint = nonempty(draft.get("端点"));
            section.region = nonempty(draft.get("区域"));
            section.access_key_id = Some(draft.get("AccessKeyId").trim().to_string());
            section.prefix = nonempty(draft.get("前缀"));
            section.path_style = match draft.get("寻址").trim() {
                "" | "auto" => None,
                "virtual-host" => Some(false),
                "path" => Some(true),
                _ => {
                    return Err(DaemonError::Protocol(
                        "寻址必须为 auto、virtual-host 或 path".into(),
                    ));
                }
            };
            let secret = draft.get("SecretAccessKey");
            if !secret.is_empty() {
                let file = format!("{name}.secret");
                section.secret_access_key_file = Some(file.clone());
                secrets.push((file, secret.to_string()));
            }
            let session_token = draft.get("会话令牌");
            if !session_token.is_empty() {
                let file = format!("{name}.sessiontoken");
                section.session_token_file = Some(file.clone());
                secrets.push((file, session_token.to_string()));
            }
        }
    }
    Ok((section, secrets))
}

/// 把已有后端装成表单草稿（编辑时预填现值）。
fn draft_from_section(section: &cx_core::BackendSection) -> cx_tui::TuiBackendDraft {
    use cx_core::BackendKind;
    use cx_tui::WizardKind;
    let addressing = match section.path_style {
        Some(true) => "path",
        Some(false) => "virtual-host",
        None => "",
    };
    let kind = match section.kind {
        BackendKind::Server => WizardKind::Server,
        BackendKind::Webdav => WizardKind::Webdav,
        BackendKind::S3 => WizardKind::S3,
    };
    // 顺序必须与 wizard 的字段顺序一致；密钥类字段留空（不回显，留空即沿用旧值）
    let values: Vec<String> = match kind {
        cx_tui::WizardKind::Server => vec![
            section.name.clone(),
            section.url.clone().unwrap_or_default(),
            section.space.clone().unwrap_or_default(),
            String::new(),
        ],
        cx_tui::WizardKind::Webdav => vec![
            section.name.clone(),
            section.url.clone().unwrap_or_default(),
            section.username.clone().unwrap_or_default(),
            String::new(),
        ],
        cx_tui::WizardKind::S3 => vec![
            section.name.clone(),
            section.bucket.clone().unwrap_or_default(),
            String::new(), // 模板：编辑时不重选
            section.endpoint.clone().unwrap_or_default(),
            section.region.clone().unwrap_or_default(),
            section.access_key_id.clone().unwrap_or_default(),
            String::new(), // SecretAccessKey
            section.prefix.clone().unwrap_or_default(),
            addressing.to_string(),
            String::new(), // 会话令牌
        ],
    };
    cx_tui::TuiBackendDraft { kind, values }
}

/// 保存"编辑已有后端"：位置字段以草稿为准，凭据字段留空则沿用旧值。
fn update_draft_with_probe(
    paths: &Paths,
    original: &str,
    draft: &cx_tui::TuiBackendDraft,
    probe: impl FnOnce(&Paths, &Config, &cx_core::BackendSection) -> Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    let mut config = Config::load(paths)?;
    let index = config
        .backends
        .iter()
        .position(|section| section.name == original)
        .ok_or_else(|| {
            DaemonError::Protocol(format!(
                "没有名为 {original} 的后端（cx backend list 查看）"
            ))
        })?;
    let old = config.backends[index].clone();
    let (mut section, secrets) = draft_to_section(draft, false)?;
    if section.name != old.name
        && config
            .backends
            .iter()
            .any(|other| other.name == section.name)
    {
        return Err(DaemonError::Protocol(format!(
            "已经有名为 {} 的后端了",
            section.name
        )));
    }
    // 凭据：草稿留空就沿用旧文件，避免"改个前缀把密钥弄丢"
    if section.secret_access_key_file.is_none() {
        section.secret_access_key_file = old.secret_access_key_file.clone();
        section.secret_access_key_env = old.secret_access_key_env.clone();
    }
    if section.session_token_file.is_none() {
        section.session_token_file = old.session_token_file.clone();
        section.session_token_env = old.session_token_env.clone();
    }
    if section.password_file.is_none() {
        section.password_file = old.password_file.clone();
        section.password_env = old.password_env.clone();
    }
    if section.token_file.is_none() {
        section.token_file = old.token_file.clone();
    }
    if section.access_key_id.is_none() {
        section.access_key_id = old.access_key_id.clone();
    }
    if section.username.is_none() {
        section.username = old.username.clone();
    }
    if section.ca_file.is_none() {
        section.ca_file = old.ca_file.clone();
    }
    section.enabled = old.enabled;
    config.backends[index] = section.clone();
    config.validate()?;
    // 新的密钥（如果有）先落盘：探测需要读它
    for (file, value) in &secrets {
        write_secret_file(paths, file, value)?;
    }
    if let Err(err) = probe(paths, &config, &section) {
        // 探测失败：新写的凭据删掉，配置保持原样
        for (file, _) in &secrets {
            let _ = std::fs::remove_file(paths.secret_file(file));
        }
        return Err(err);
    }
    config.save(paths)?;
    // 改名后旧名字的凭据文件不会再被引用：删掉，别把旧密钥留在磁盘上
    if section.name != old.name {
        for file in [
            old.secret_access_key_file.as_deref(),
            old.session_token_file.as_deref(),
            old.password_file.as_deref(),
            old.token_file.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            // 只删确实不再被任何后端引用的
            let still_used = config.backends.iter().any(|other| {
                other.secret_access_key_file.as_deref() == Some(file)
                    || other.session_token_file.as_deref() == Some(file)
                    || other.password_file.as_deref() == Some(file)
                    || other.token_file.as_deref() == Some(file)
            });
            if !still_used {
                let _ = std::fs::remove_file(paths.secret_file(file));
            }
        }
    }
    Ok(())
}
/// Prepare and test a draft before committing configuration. Failures roll back secrets.
fn add_backend_from_draft(
    paths: &Paths,
    draft: &cx_tui::TuiBackendDraft,
) -> Result<(), DaemonError> {
    add_draft_with_probe(paths, draft, probe_section)?;
    restart_daemon(paths)
}

fn add_draft_with_probe(
    paths: &Paths,
    draft: &cx_tui::TuiBackendDraft,
    probe: impl FnOnce(&Paths, &Config, &cx_core::BackendSection) -> Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    use cx_tui::WizardKind;
    let (section, secrets) = draft_to_section(draft, true)?;
    let name = section.name.clone();
    let mut config = Config::load(paths)?;
    validate_addition(&config, &section)?;
    let mut secrets = secrets;
    if draft.kind == WizardKind::Server {
        let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
        let token = cx_backend::enroll(
            section.url.as_deref().unwrap_or_default(),
            section.space.as_deref().unwrap_or_default(),
            draft.get("注册密钥"),
            &identity.name,
            None,
            false,
            std::time::Duration::from_secs(30),
        )
        .map_err(|err| {
            DaemonError::Protocol(format!("自助注册失败：{err}；{}", backend_error_tip(&err)))
        })?;
        secrets.push((section.token_file.clone().unwrap(), token));
    }
    commit_backend(
        paths,
        &mut config,
        section,
        secrets,
        probe,
        |config, paths| config.save(paths).map_err(Into::into),
    )?;
    println!("已添加后端 {name}（只读连通性测试通过；写入能力尚未验证）");
    Ok(())
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn ensure_unique_name(config: &Config, name: &str) -> Result<(), DaemonError> {
    if config.backend(name).is_some() {
        return Err(DaemonError::Protocol(format!(
            "已经有名为 {name} 的后端了；添加不会覆盖现有凭据"
        )));
    }
    Ok(())
}

/// Validate without reading credentials or touching the network.
fn validate_addition(
    config: &Config,
    section: &cx_core::BackendSection,
) -> Result<(), DaemonError> {
    check_backend_name(&section.name)?;
    ensure_unique_name(config, &section.name)?;
    let mut candidate = config.clone();
    candidate.backends.push(section.clone());
    candidate.validate()?;
    if section.kind == cx_core::BackendKind::Server
        && section
            .space
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
    {
        return Err(DaemonError::Protocol("服务端空间 id 不能为空".to_string()));
    }
    for (label, value) in [
        ("URL", section.url.as_deref()),
        ("端点", section.endpoint.as_deref()),
    ] {
        if let Some(value) = value {
            let parsed = url::Url::parse(value)
                .map_err(|_| DaemonError::Protocol(format!("{label} 必须是合法的 HTTP(S) URL")))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
                || (label == "端点" && parsed.path() != "/" && !parsed.path().is_empty())
            {
                return Err(DaemonError::Protocol(format!(
                    "{label} 格式不合法：不能包含凭据、query 或 fragment；S3 端点不能包含路径"
                )));
            }
        }
    }
    Ok(())
}

/// Own only files successfully created by this operation. Never overwrite old files.
#[derive(Default)]
struct CredentialTransaction {
    created: Vec<std::path::PathBuf>,
}

impl CredentialTransaction {
    fn write(&mut self, paths: &Paths, file: &str, value: &str) -> Result<(), DaemonError> {
        write_secret_file(paths, file, value)?;
        self.created.push(paths.secret_file(file));
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), DaemonError> {
        let mut failures = Vec::new();
        self.created
            .retain(|path| match std::fs::remove_file(path) {
                Ok(()) => false,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                Err(err) => {
                    failures.push(format!("{}：{err}", path.display()));
                    true
                }
            });
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DaemonError::Protocol(format!(
                "凭据回滚失败：{}",
                failures.join("；")
            )))
        }
    }
}

impl Drop for CredentialTransaction {
    fn drop(&mut self) {
        for path in &self.created {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The injected probe/save seams keep failure tests completely offline.
fn commit_backend(
    paths: &Paths,
    config: &mut Config,
    section: cx_core::BackendSection,
    secrets: Vec<(String, String)>,
    probe: impl FnOnce(&Paths, &Config, &cx_core::BackendSection) -> Result<(), DaemonError>,
    save: impl FnOnce(&Config, &Paths) -> Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    validate_addition(config, &section)?;
    let original = cx_core::fsutil::read_to_string_optional(&paths.config_file())?;
    let mut transaction = CredentialTransaction::default();
    let result = (|| {
        for (file, value) in &secrets {
            transaction.write(paths, file, value)?;
        }
        probe(paths, config, &section)?;
        let mut candidate = config.clone();
        candidate.backends.push(section);
        if let Err(err) = save(&candidate, paths) {
            // Config::save can fail after rename (e.g. tightening permissions).
            let now = cx_core::fsutil::read_to_string_optional(&paths.config_file())?;
            if now != original {
                match &original {
                    Some(text) => {
                        cx_core::fsutil::write_atomic(&paths.config_file(), text.as_bytes())?
                    }
                    None => std::fs::remove_file(paths.config_file()).map_err(DaemonError::io)?,
                }
            }
            return Err(err);
        }
        *config = candidate;
        Ok(())
    })();
    if let Err(err) = result {
        return match transaction.rollback() {
            Ok(()) => Err(err),
            Err(rollback) => Err(DaemonError::Protocol(format!("{err}；{rollback}"))),
        };
    }
    transaction.created.clear();
    Ok(())
}

fn probe_section(
    paths: &Paths,
    config: &Config,
    section: &cx_core::BackendSection,
) -> Result<(), DaemonError> {
    let backend = cx_backend::build_backend(section, paths, std::time::Duration::from_secs(30))
        .map_err(|err| {
            DaemonError::Protocol(format!("无法构造后端：{err}；{}", backend_error_tip(&err)))
        })?;
    let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
    let mut sync = cx_sync::RemoteSync::new(backend, &identity.device_id, &identity.name);
    sync.probe().map(|_| ()).map_err(|err| {
        DaemonError::Protocol(format!(
            "连通性测试失败：{err}；{}",
            backend_error_tip(&err)
        ))
    })
}
fn ensure_daemon_running(paths: &Paths) -> Result<(), DaemonError> {
    if ControlClient::connect(paths).is_ok() {
        return Ok(());
    }
    cx_daemon::spawn_background(paths, false)?;
    wait_daemon_ready(paths)
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

        BackendCommand::Edit {
            name,
            bucket,
            endpoint,
            region,
            prefix,
            access_key_id,
            virtual_host,
            path_style,
            url,
            username,
            secret_stdin,
        } => {
            if virtual_host && path_style {
                return Err(DaemonError::Protocol(
                    "--virtual-host 与 --path-style 只能选一个".to_string(),
                ));
            }
            let mut config = Config::load(paths)?;
            let index = config
                .backends
                .iter()
                .position(|section| section.name == name)
                .ok_or_else(|| {
                    DaemonError::Protocol(format!(
                        "没有名为 {name} 的后端（用 `cx backend list` 查看）"
                    ))
                })?;
            if let Some(value) = bucket {
                config.backends[index].bucket = Some(value.trim().to_string());
            }
            if let Some(value) = endpoint {
                config.backends[index].endpoint = nonempty(&value);
            }
            if let Some(value) = region {
                config.backends[index].region = nonempty(&value);
            }
            if let Some(value) = prefix {
                // 空前缀合法（放桶根）；顺手去掉首尾斜杠，免得拼出 //
                config.backends[index].prefix = nonempty(value.trim().trim_matches('/'));
            }
            if let Some(value) = access_key_id {
                config.backends[index].access_key_id = Some(value.trim().to_string());
            }
            if virtual_host {
                config.backends[index].path_style = Some(false);
            }
            if path_style {
                config.backends[index].path_style = Some(true);
            }
            if let Some(value) = url {
                config.backends[index].url = Some(value.trim().to_string());
            }
            if let Some(value) = username {
                config.backends[index].username = nonempty(&value);
            }
            if secret_stdin {
                let text = read_stdin_until(4096, Some("私有密钥/口令"))?;
                let file = match config.backends[index].kind {
                    cx_core::BackendKind::S3 => format!("{name}.key"),
                    cx_core::BackendKind::Webdav => format!("{name}.password"),
                    cx_core::BackendKind::Server => format!("{name}.token"),
                };
                write_secret_file(paths, &file, text.trim())?;
                match config.backends[index].kind {
                    cx_core::BackendKind::S3 => {
                        config.backends[index].secret_access_key_file = Some(file)
                    }
                    cx_core::BackendKind::Webdav => {
                        config.backends[index].password_file = Some(file)
                    }
                    cx_core::BackendKind::Server => config.backends[index].token_file = Some(file),
                }
            }
            config.save(paths)?;
            println!(
                "已更新后端 {name}：{}",
                backend_target(&config.backends[index])
            );
            println!("重启后生效：cx restart");
            Ok(ExitCode::SUCCESS)
        }
        BackendCommand::Remove { name } => {
            // 与界面里的移除共用一份实现（含凭据清理）
            if let Err(err) = remove_backend(paths, &name) {
                eprintln!("{err}（用 `cx backend list` 查看）");
                return Ok(ExitCode::FAILURE);
            }
            println!("已移除后端 {name}");
            // 说清生效条件：后端 worker 是守护进程启动时创建的，reload 不重建它
            println!("提示：需要重启守护进程才能生效（cx restart；保留当前剪贴板模式）。");
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

/// Add CLI configuration without silently overwriting credentials or doing a test.
fn add_backend(paths: &Paths, kind: BackendAddCommand) -> Result<bool, DaemonError> {
    let name = match &kind {
        BackendAddCommand::Webdav { name, .. }
        | BackendAddCommand::Server { name, .. }
        | BackendAddCommand::S3 { name, .. } => name.trim().to_string(),
    };
    check_backend_name(&name)?;
    // 在 match 借用/移动 kind 之前先取出这个可选开关，后面才知道要不要读会话令牌
    let session_token_stdin = matches!(
        &kind,
        BackendAddCommand::S3 {
            session_token_stdin: true,
            ..
        }
    );
    let mut config = Config::load(paths)?;
    if let Err(err) = ensure_unique_name(&config, &name) {
        eprintln!("{err}");
        return Ok(false);
    }
    let mut section;
    match &kind {
        BackendAddCommand::Webdav {
            url,
            username,
            password_stdin,
            ca_file,
            insecure,
            ..
        } => {
            section = empty_section(name.clone(), cx_core::BackendKind::Webdav);
            section.url = Some(url.trim().to_string());
            section.username = username.as_deref().and_then(nonempty);
            section.ca_file = ca_file.clone();
            section.insecure = *insecure;
            if *password_stdin && section.username.is_some() {
                section.password_file = Some(format!("{name}.pass"));
            }
        }
        BackendAddCommand::Server {
            url,
            space,
            token_stdin,
            enroll_secret,
            ca_file,
            insecure,
            ..
        } => {
            section = empty_section(name.clone(), cx_core::BackendKind::Server);
            section.url = Some(url.trim().to_string());
            section.space = Some(space.trim().to_string());
            section.ca_file = ca_file.clone();
            section.insecure = *insecure;
            if *token_stdin && enroll_secret.is_some() {
                return Err(DaemonError::Protocol(
                    "--token-stdin 与 --enroll-secret 只能选一个".to_string(),
                ));
            }
            if !*token_stdin && enroll_secret.is_none() {
                return Err(DaemonError::Protocol(
                    "请用 --token-stdin 提供设备令牌，或用 --enroll-secret 自助注册".to_string(),
                ));
            }
            if enroll_secret.as_deref() == Some("") {
                return Err(DaemonError::Protocol("注册密钥不能为空".to_string()));
            }
            section.token_file = Some(format!("{name}.token"));
        }
        BackendAddCommand::S3 {
            bucket,
            endpoint,
            region,
            prefix,
            access_key_id,
            secret_stdin,
            session_token_stdin,
            path_style,
            virtual_host,
            ca_file,
            insecure,
            ..
        } => {
            section = empty_section(name.clone(), cx_core::BackendKind::S3);
            section.bucket = Some(bucket.trim().to_string());
            section.endpoint = endpoint.clone();
            section.region = region.clone();
            section.prefix = prefix.clone();
            section.access_key_id = Some(access_key_id.trim().to_string());
            section.ca_file = ca_file.clone();
            section.insecure = *insecure;
            if *path_style && *virtual_host {
                return Err(DaemonError::Protocol(
                    "--path-style 与 --virtual-host 只能选一个".to_string(),
                ));
            }
            section.path_style = if *path_style {
                Some(true)
            } else if *virtual_host {
                Some(false)
            } else {
                None
            };
            if *secret_stdin {
                section.secret_access_key_file = Some(format!("{name}.key"));
            }
            if *session_token_stdin {
                section.session_token_file = Some(format!("{name}.sessiontoken"));
            }
        }
    }
    if let Err(err) = validate_addition(&config, &section) {
        eprintln!("配置有问题，未保存：{err}");
        return Ok(false);
    }
    let mut secrets = Vec::new();
    match kind {
        BackendAddCommand::Webdav {
            password_stdin: true,
            ..
        } => {
            if let Some(file) = &section.password_file {
                secrets.push((file.clone(), read_secret_stdin("口令")?));
            } else {
                eprintln!("注意：匿名 WebDAV 不读取或保存口令");
            }
        }
        BackendAddCommand::Server { enroll_secret, .. } => {
            let token = if let Some(secret) = enroll_secret {
                let identity = Identity::load_or_create(paths, config.device.name.as_deref())?;
                cx_backend::enroll(
                    section.url.as_deref().unwrap_or_default(),
                    section.space.as_deref().unwrap_or_default(),
                    &secret,
                    &identity.name,
                    section
                        .ca_file
                        .as_deref()
                        .map(|file| paths.secret_file(file)),
                    section.insecure,
                    std::time::Duration::from_secs(30),
                )
                .map_err(|err| {
                    DaemonError::Protocol(format!(
                        "自助注册失败：{err}；{}",
                        backend_error_tip(&err)
                    ))
                })?
            } else {
                read_secret_stdin("设备令牌")?
            };
            secrets.push((section.token_file.clone().unwrap(), token));
        }
        BackendAddCommand::S3 {
            secret_stdin: true, ..
        } => {
            secrets.push((
                section.secret_access_key_file.clone().unwrap(),
                read_secret_stdin("私有密钥")?,
            ));
        }
        _ => {}
    }
    // 会话令牌是可选来源，所以单独判一次，不塞进上面的必需分支
    if session_token_stdin {
        secrets.push((
            section.session_token_file.clone().unwrap(),
            read_secret_stdin("会话令牌（STS）")?,
        ));
    }
    commit_backend(
        paths,
        &mut config,
        section,
        secrets,
        |_, _, _| Ok(()),
        |config, paths| config.save(paths).map_err(Into::into),
    )?;
    println!("已添加后端 {name}");
    println!("重启后生效：cx restart");
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
        session_token_file: None,
        session_token_env: None,
        path_style: None,
        poll_interval_ms: None,
    }
}

/// Remove exactly one stdin line terminator, never password whitespace.
fn strip_line_terminator(mut value: String) -> String {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    value
}

fn read_secret_stdin(label: &str) -> Result<String, DaemonError> {
    eprint!("请输入{label}（回车结束）：");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(DaemonError::io)?;
    let value = strip_line_terminator(line);
    if value.is_empty() {
        return Err(DaemonError::Protocol(format!("{label}不能为空")));
    }
    Ok(value)
}

/// Exclusive creation: even orphaned credentials must never be overwritten.
fn write_secret_file(paths: &Paths, file_name: &str, value: &str) -> Result<(), DaemonError> {
    use std::io::Write;
    let path = paths.secret_file(file_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(DaemonError::io)?;
        cx_core::fsutil::tighten_permissions(parent, true)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|err| {
        DaemonError::Protocol(format!(
            "创建凭据文件失败（不会覆盖已有文件）{}：{err}",
            path.display()
        ))
    })?;
    let result = (|| -> Result<(), DaemonError> {
        file.write_all(value.as_bytes()).map_err(DaemonError::io)?;
        file.sync_all().map_err(DaemonError::io)?;
        cx_core::fsutil::tighten_permissions(&path, false)?;
        Ok(())
    })();
    drop(file);
    if let Err(err) = result {
        if let Err(cleanup) = std::fs::remove_file(&path) {
            return Err(DaemonError::Protocol(format!(
                "{err}；清理凭据失败：{cleanup}"
            )));
        }
        return Err(err);
    }
    Ok(())
}

fn backend_error_tip(err: &cx_backend::BackendError) -> &'static str {
    let detail = err.to_string();
    if detail.contains("SecondLevelDomainForbidden") {
        "OSS 要求 virtual-hosted 寻址；使用 --virtual-host（或 path_style=false），不要扩大 RAM 权限"
    } else if detail.contains("SignatureDoesNotMatch") {
        "核对签名区域、端点/寻址及密钥原始字节；不要把所有签名错误都当成权限不足"
    } else if detail.contains("InvalidAccessKeyId") {
        "AccessKeyId 不存在或已停用；核对账号与凭据来源"
    } else if detail.contains("ExpiredToken") || detail.contains("InvalidToken") {
        "临时凭据已过期或不完整；更新 STS 凭据（含 session token），不要只替换 Secret"
    } else if detail.contains("RequestTimeTooSkewed") || detail.contains("RequestExpired") {
        "检查系统时间与时间同步后重试"
    } else if detail.contains("AuthorizationHeaderMalformed")
        || detail.contains("PermanentRedirect")
        || detail.contains("IncorrectEndpoint")
    {
        "核对桶所属区域和服务端建议的 endpoint；不要将桶域名重复作为服务端点，也不要关闭 TLS"
    } else if detail.contains("AccessDenied") {
        "核对该桶及对象前缀的 List/Get 权限与显式 Deny；403 不证明桶属于别人，也不要求 ListBuckets 或建桶权限"
    } else if err.is_auth() {
        "区分认证与授权：核对账号/凭据及目标桶或目录权限；不建议反复删除重加或授予管理权限"
    } else if err.kind() == "config" || err.kind() == "credentials" {
        "检查配置字段和凭据文件/环境变量；不要删除可用凭据或关闭 TLS 来绕过错误"
    } else {
        "检查 DNS、网络、TLS 信任与端点/区域；只读测试不验证写入权限，请勿关闭 TLS 校验"
    }
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

    // 端点形态提示：内网端点之类"配置合法但多数情况连不上"的组合提前说清楚
    if let Some(endpoint) = section.endpoint.as_deref() {
        for note in cx_backend::endpoint_notes(endpoint) {
            println!("注意：{note}");
        }
    }

    match sync.probe() {
        Ok(summary) => {
            println!("后端 {name} 可用：{summary}");
            Ok(true)
        }
        Err(err) => {
            eprintln!("后端 {name} 不可用：{err}");
            eprintln!("提示：{}", backend_error_tip(&err));
            if let Some(endpoint) = section.endpoint.as_deref() {
                for note in cx_backend::endpoint_notes(endpoint) {
                    eprintln!("注意：{note}");
                }
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
            token_file: Some("home.token".to_string()),
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
            session_token_file: None,
            session_token_env: None,
            path_style: None,
            poll_interval_ms: None,
        });
        config.save(&paths).unwrap();
        // 放两个该后端的凭据文件（两种历史命名都放，验证按前缀清理）
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        let token = paths.secret_file("home.token");
        let pass = paths.secret_file("home.pass");
        std::fs::write(&token, "t").unwrap();
        std::fs::write(&pass, "p").unwrap();

        super::remove_backend(&paths, "home").unwrap();
        let after = Config::load(&paths).unwrap();
        assert!(
            !after.backends.iter().any(|backend| backend.name == "home"),
            "移除后配置里不该还有它"
        );
        assert!(!token.exists(), "凭据文件应当被清掉：{}", token.display());
        assert!(pass.exists(), "未被引用的旧文件不能仅凭名字前缀删除");

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
            token_file: Some("home.token".to_string()),
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
            session_token_file: None,
            session_token_env: None,
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
    fn 命令行添加也会先拒绝会写到_secrets_之外的名字() {
        let paths = temp_paths("cli-bad-name");
        // secret_stdin: true —— 名字校验如果没被提到读 stdin 之前，这个测试会挂住，
        // 所以它同时验证了顺序。
        let err = super::add_backend(
            &paths,
            BackendAddCommand::S3 {
                name: "../escape".to_string(),
                bucket: "b".to_string(),
                endpoint: Some("http://127.0.0.1:1".to_string()),
                region: Some("cn-hangzhou".to_string()),
                prefix: None,
                access_key_id: "AK".to_string(),
                secret_stdin: true,
                session_token_stdin: false,
                path_style: false,
                virtual_host: false,
                ca_file: None,
                insecure: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("后端名不合法"), "{err}");

        let lingering = std::fs::read_dir(paths.secrets_dir())
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(lingering, 0, "被拒绝的名字不该留下任何凭据文件");
        let _ = std::fs::remove_dir_all(paths.data_dir.parent().unwrap_or(&paths.data_dir));
    }

    #[test]
    fn 表单会拒绝会写到_secrets_之外的后端名() {
        use cx_tui::{BackendDraft, WizardKind};
        let paths = temp_paths("draft-bad-name");
        for name in ["", "a/b", "a\\b", "../escape"] {
            let draft = BackendDraft {
                kind: WizardKind::S3,
                values: vec![
                    name.to_string(),
                    "bucket".to_string(),
                    "https://oss-cn-hangzhou.aliyuncs.com".to_string(),
                    "cn-hangzhou".to_string(),
                    "AKID".to_string(),
                    "SECRET".to_string(),
                    "clipboardx".to_string(),
                ],
            };
            let err = super::add_backend_from_draft(&paths, &draft).unwrap_err();
            assert!(
                err.to_string().contains("后端名不合法"),
                "名字 {name:?} 应当被拒绝：{err}"
            );
        }
        // 关键断言：一个文件都不该被写出去
        let lingering: Vec<String> = std::fs::read_dir(paths.secrets_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            lingering.is_empty(),
            "被拒绝的名字不该留下任何凭据文件：{lingering:?}"
        );
        let _ = std::fs::remove_dir_all(paths.data_dir.parent().unwrap_or(&paths.data_dir));
    }

    #[test]
    fn 表单添加服务端先校验字段再自助注册() {
        use cx_tui::{BackendDraft, WizardKind};
        let paths = temp_paths("draft-server");
        let draft = BackendDraft {
            kind: WizardKind::Server,
            values: vec![
                "home".to_string(),
                // Invalid URL must fail before enrollment, identity creation, or any IO.
                "invalid-url".to_string(),
                "space-1".to_string(),
                "SECRET-abc".to_string(),
            ],
        };
        let err = super::add_backend_from_draft(&paths, &draft).unwrap_err();
        assert!(err.to_string().contains("URL"), "应先校验 URL：{err}");
        assert!(!paths.identity_file().exists());
        assert!(
            Config::load(&paths).unwrap().backends.is_empty(),
            "注册失败不该留下半截后端配置"
        );
        let _ = std::fs::remove_dir_all(paths.data_dir.parent().unwrap_or(&paths.data_dir));
    }

    #[test]
    fn duplicate_name_rejects_before_stdin_and_preserves_credentials() {
        let paths = temp_paths("duplicate-secret");
        let mut config = Config::default();
        let mut section = empty_section("dav".into(), cx_core::BackendKind::Webdav);
        section.url = Some("https://example.invalid/dav".into());
        section.username = Some("fake".into());
        section.password_file = Some("dav.pass".into());
        config.backends.push(section);
        config.save(&paths).unwrap();
        write_secret_file(&paths, "dav.pass", " original password ").unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        assert!(
            !add_backend(
                &paths,
                BackendAddCommand::Webdav {
                    name: " dav ".into(),
                    url: "https://example.invalid/other".into(),
                    username: Some("other".into()),
                    password_stdin: true,
                    ca_file: None,
                    insecure: false,
                }
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read(paths.secret_file("dav.pass")).unwrap(),
            b" original password "
        );
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        // Server duplicate must also be rejected before enrollment or identity creation.
        assert!(
            !add_backend(
                &paths,
                BackendAddCommand::Server {
                    name: "dav".into(),
                    url: "https://example.invalid".into(),
                    space: "fake".into(),
                    token_stdin: false,
                    enroll_secret: Some("fake".into()),
                    ca_file: None,
                    insecure: false,
                }
            )
            .unwrap()
        );
        assert!(!paths.identity_file().exists());
        let draft = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::Webdav,
            values: vec![
                "dav".into(),
                "https://example.invalid/new".into(),
                "other".into(),
                "replacement".into(),
            ],
        };
        assert!(
            add_draft_with_probe(&paths, &draft, |_, _, _| panic!(
                "duplicate draft must not probe"
            ))
            .is_err()
        );
        assert_eq!(
            std::fs::read(paths.secret_file("dav.pass")).unwrap(),
            b" original password "
        );
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        assert!(!paths.secret_file("dav.password").exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn remove_exact_custom_shared_and_dotted_references() {
        let paths = temp_paths("remove-exact");
        let mut config = Config::default();
        for (name, credential) in [
            ("a", "custom-token"),
            ("a.b", "a.b.token"),
            ("sharing", "shared.pass"),
        ] {
            let mut section = empty_section(name.into(), cx_core::BackendKind::Server);
            section.url = Some("https://example.invalid".into());
            section.space = Some("fake-space".into());
            section.token_file = Some(credential.into());
            config.backends.push(section);
        }
        config.backends[0].password_file = None;
        config.backends[0].secret_access_key_file = Some("shared.pass".into());
        config.backends[0].ca_file = Some("a.ca".into());
        // A credential file can also be a CA reference of a remaining backend.
        config.backends[0].password_file = Some("protected-ca".into());
        config.backends[2].ca_file = Some("./protected-ca".into());
        // Server forbids token + password, so use WebDAV for this synthetic multi-reference fixture.
        config.backends[0].kind = cx_core::BackendKind::Webdav;
        config.backends[0].username = Some("fake".into());
        config.save(&paths).unwrap();
        for file in [
            "custom-token",
            "a.b.token",
            "shared.pass",
            "a.ca",
            "protected-ca",
            "a.unreferenced",
        ] {
            write_secret_file(&paths, file, file).unwrap();
        }
        remove_backend(&paths, "a").unwrap();
        assert!(!paths.secret_file("custom-token").exists());
        for file in [
            "a.b.token",
            "shared.pass",
            "a.ca",
            "protected-ca",
            "a.unreferenced",
        ] {
            assert_eq!(
                std::fs::read_to_string(paths.secret_file(file)).unwrap(),
                file
            );
        }
        assert_eq!(Config::load(&paths).unwrap().backends.len(), 2);
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn remove_reports_credential_cleanup_failure() {
        let paths = temp_paths("remove-cleanup-fail");
        let mut config = Config::default();
        config.backends.push(fake_secret_section("fake"));
        config.save(&paths).unwrap();
        std::fs::create_dir(paths.secret_file("fake.pass")).unwrap();
        let err = remove_backend(&paths, "fake").unwrap_err();
        assert!(err.to_string().contains("清理失败"));
        assert!(Config::load(&paths).unwrap().backends.is_empty());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    fn fake_secret_section(name: &str) -> cx_core::BackendSection {
        let mut section = empty_section(name.into(), cx_core::BackendKind::Webdav);
        section.url = Some("https://example.invalid/dav".into());
        section.username = Some("fake".into());
        section.password_file = Some(format!("{name}.pass"));
        section
    }

    #[test]
    fn invalid_config_rejects_before_any_transaction_write() {
        let paths = temp_paths("invalid-config-rollback");
        let mut config = Config::default();
        let mut invalid = fake_secret_section("new");
        invalid.url = Some(" ".into());
        assert!(
            commit_backend(
                &paths,
                &mut config,
                invalid,
                vec![("new.pass".into(), "fake".into())],
                |_, _, _| panic!("invalid config must not probe"),
                |_, _| panic!("invalid config must not save")
            )
            .is_err()
        );
        assert!(!paths.secret_file("new.pass").exists());
        assert!(!paths.config_file().exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn failed_probe_rolls_back_secret_and_never_saves_config() {
        let paths = temp_paths("probe-rollback");
        let mut config = Config::default();
        let err = commit_backend(
            &paths,
            &mut config,
            fake_secret_section("new"),
            vec![("new.pass".into(), " secret ".into())],
            |paths, _, _| {
                assert!(
                    !paths.config_file().exists(),
                    "probe must happen before config commit"
                );
                assert_eq!(
                    std::fs::read_to_string(paths.secret_file("new.pass")).unwrap(),
                    " secret "
                );
                Err(DaemonError::Protocol("mock probe failure".into()))
            },
            |_, _| panic!("failed probe must not save"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("mock probe failure"));
        assert!(config.backends.is_empty());
        assert!(!paths.config_file().exists());
        assert!(!paths.secret_file("new.pass").exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn failed_save_rolls_back_new_credentials_and_restores_config_bytes() {
        let paths = temp_paths("save-rollback");
        let mut config = Config::default();
        config.save(&paths).unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        let err = commit_backend(
            &paths,
            &mut config,
            fake_secret_section("new"),
            vec![("new.pass".into(), "fake secret".into())],
            |_, _, _| Ok(()),
            |candidate, paths| {
                candidate.save(paths)?;
                Err(DaemonError::Protocol(
                    "mock post-rename save failure".into(),
                ))
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("mock post-rename"));
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        assert!(config.backends.is_empty());
        assert!(!paths.secret_file("new.pass").exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn failed_first_save_removes_new_config_and_secret() {
        let paths = temp_paths("first-save-rollback");
        let mut config = Config::default();
        let err = commit_backend(
            &paths,
            &mut config,
            fake_secret_section("new"),
            vec![("new.pass".into(), "fake".into())],
            |_, _, _| Ok(()),
            |candidate, paths| {
                candidate.save(paths)?;
                Err(DaemonError::Protocol("mock first save failure".into()))
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("first save"));
        assert!(!paths.config_file().exists());
        assert!(!paths.secret_file("new.pass").exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn config_save_io_failure_rolls_back_secret_without_touching_old_config() {
        let paths = temp_paths("save-io-failure");
        let mut config = Config::default();
        config.save(&paths).unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        // Force an actual rename/write failure at a directory target, without relying on temp names.
        assert!(
            commit_backend(
                &paths,
                &mut config,
                fake_secret_section("new"),
                vec![("new.pass".into(), "fake".into())],
                |_, _, _| Ok(()),
                |_, paths| cx_core::fsutil::write_atomic(
                    &paths.config_dir,
                    b"cannot-replace-directory"
                )
                .map_err(Into::into)
            )
            .is_err()
        );
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        assert!(!paths.secret_file("new.pass").exists());
        assert!(config.backends.is_empty());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn failed_draft_test_returns_error_with_no_config_or_credentials() {
        let paths = temp_paths("draft-probe-fail");
        let draft = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::Webdav,
            values: vec![
                "fake".into(),
                "https://example.invalid/dav".into(),
                "user".into(),
                " secret ".into(),
            ],
        };
        let err = add_draft_with_probe(&paths, &draft, |paths, _, _| {
            assert!(!paths.config_file().exists());
            Err(DaemonError::Protocol("mock denied".into()))
        })
        .unwrap_err();
        assert!(err.to_string().contains("mock denied"));
        assert!(!paths.config_file().exists());
        assert_eq!(std::fs::read_dir(paths.secrets_dir()).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn create_new_collision_keeps_orphan_secret_and_rolls_back_earlier_writes() {
        let paths = temp_paths("secret-collision");
        write_secret_file(&paths, "existing", "must survive").unwrap();
        let mut config = Config::default();
        assert!(
            commit_backend(
                &paths,
                &mut config,
                fake_secret_section("new"),
                vec![
                    ("new.pass".into(), "fake".into()),
                    ("existing".into(), "replacement".into())
                ],
                |_, _, _| panic!("collision must precede probe"),
                |_, _| panic!("must not save")
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(paths.secret_file("existing")).unwrap(),
            "must survive"
        );
        assert!(!paths.secret_file("new.pass").exists());
        assert!(!paths.config_file().exists());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn s3_草稿的寻址与会话令牌都落到配置且令牌原样保留() {
        let paths = temp_paths("oss-session-token-draft");
        let draft = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::S3,
            values: vec![
                "fake-sts".into(),
                "fake-bucket".into(),
                // 模板行（S3 表单的"服务商模板"，留空 = 自定义）
                "".into(),
                "https://oss-cn-beijing.aliyuncs.com".into(),
                "cn-beijing".into(),
                "fake-ak".into(),
                "fake-secret".into(),
                "fake-prefix".into(),
                "path".into(),
                " fake-session-token ".into(),
            ],
        };
        add_draft_with_probe(&paths, &draft, |paths, _, section| {
            assert_eq!(section.path_style, Some(true), "显式 path 必须落进配置");
            assert_eq!(
                section.session_token_file.as_deref(),
                Some("fake-sts.sessiontoken")
            );
            assert_eq!(
                std::fs::read_to_string(paths.secret_file("fake-sts.sessiontoken")).unwrap(),
                " fake-session-token ",
                "会话令牌必须原样保留，不做 trim"
            );
            Ok(())
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn oss_draft_keeps_auto_addressing_and_tests_before_commit() {
        let paths = temp_paths("oss-auto-draft");
        let draft = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::S3,
            values: vec![
                "fake-oss".into(),
                "fake-bucket".into(),
                // 模板行（S3 表单的"服务商模板"，留空 = 自定义）
                "".into(),
                "https://oss-cn-beijing.aliyuncs.com".into(),
                "cn-beijing".into(),
                "fake-ak".into(),
                " fake-secret ".into(),
                "fake-prefix".into(),
            ],
        };
        add_draft_with_probe(&paths, &draft, |paths, _, section| {
            assert_eq!(
                section.path_style, None,
                "auto is resolved centrally by build_backend"
            );
            assert!(!paths.config_file().exists());
            assert_eq!(
                std::fs::read_to_string(paths.secret_file("fake-oss.secret")).unwrap(),
                " fake-secret "
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(Config::load(&paths).unwrap().backends.len(), 1);
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn anonymous_draft_and_whitespace_password_are_preserved() {
        let paths = temp_paths("draft-secrets");
        let anonymous = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::Webdav,
            values: vec![
                "anon".into(),
                "https://example.invalid/dav".into(),
                "".into(),
                "".into(),
            ],
        };
        add_draft_with_probe(&paths, &anonymous, |_, _, section| {
            assert!(section.username.is_none());
            assert!(section.password_file.is_none());
            Ok(())
        })
        .unwrap();
        let draft = cx_tui::BackendDraft {
            kind: cx_tui::WizardKind::Webdav,
            values: vec![
                "auth".into(),
                "https://example.invalid/dav".into(),
                "user".into(),
                " \tpass  ".into(),
            ],
        };
        add_draft_with_probe(&paths, &draft, |paths, _, section| {
            assert_eq!(
                std::fs::read_to_string(
                    paths.secret_file(section.password_file.as_deref().unwrap())
                )
                .unwrap(),
                " \tpass  "
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(strip_line_terminator(" secret \r\n".into()), " secret ");
        assert_eq!(strip_line_terminator(" \t \n".into()), " \t ");
        assert_eq!(strip_line_terminator("value\r".into()), "value\r");
        assert_eq!(strip_line_terminator("value\n\n".into()), "value\n");
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn invalid_core_fields_fail_before_secret_input_or_enrollment() {
        let paths = temp_paths("invalid-before-enroll");
        assert!(
            !add_backend(
                &paths,
                BackendAddCommand::Server {
                    name: "new".into(),
                    url: "not-a-url".into(),
                    space: "space".into(),
                    token_stdin: false,
                    enroll_secret: Some("fake".into()),
                    ca_file: None,
                    insecure: false,
                }
            )
            .unwrap()
        );
        assert!(!paths.identity_file().exists());
        assert!(!paths.config_file().exists());
        let mut section = fake_secret_section("new");
        section.url = Some("https://user:secret@example.invalid".into());
        assert!(validate_addition(&Config::default(), &section).is_err());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn absent_space_is_distinct_from_corrupt_or_orphan_space() {
        let paths = temp_paths("space-state");
        assert!(load_optional_space(&paths).unwrap().is_none());
        std::fs::write(paths.space_key_file(), "fake").unwrap();
        assert!(load_optional_space(&paths).is_err());
        std::fs::write(paths.space_file(), "not-json").unwrap();
        assert!(load_optional_space(&paths).is_err());
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn restart_preserves_mode_waits_for_ready_and_propagates_failures() {
        use std::cell::RefCell;
        for mode in [false, true] {
            let calls = RefCell::new(Vec::new());
            restart_with_mode(
                Some(mode),
                || {
                    calls.borrow_mut().push("stop");
                    Ok(())
                },
                |actual| {
                    assert_eq!(actual, mode);
                    calls.borrow_mut().push("spawn");
                    Ok(())
                },
                || {
                    calls.borrow_mut().push("ready");
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(*calls.borrow(), ["stop", "spawn", "ready"]);
        }
        assert!(
            restart_with_mode(
                Some(true),
                || Ok(()),
                |_| Err(DaemonError::Protocol("mock spawn failed".into())),
                || panic!("spawn failure must not report ready")
            )
            .is_err()
        );
        assert!(
            restart_with_mode(
                Some(false),
                || Ok(()),
                |_| Ok(()),
                || Err(DaemonError::Protocol("mock readiness failed".into()))
            )
            .is_err()
        );
        restart_with_mode(
            None,
            || panic!("not running"),
            |_| panic!("must not auto-start"),
            || panic!("not started"),
        )
        .unwrap();
    }

    #[test]
    fn code_aware_auth_tips_distinguish_permissions_addressing_and_signature() {
        for (code, expected) in [
            ("AccessDenied", "前缀"),
            ("SecondLevelDomainForbidden", "virtual-host"),
            ("SignatureDoesNotMatch", "区域"),
            ("ExpiredToken", "STS"),
            ("InvalidAccessKeyId", "停用"),
        ] {
            let err = cx_backend::BackendError::Unauthorized {
                status: 403,
                detail: code.into(),
            };
            assert!(backend_error_tip(&err).contains(expected));
        }
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
    fn 编辑后端时留空的密钥沿用现值且先探测后落盘() {
        let paths = temp_paths("edit-keep-secret");
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        // 先落一个后端（带密钥文件）
        let mut config = Config::load_or_create(&paths).unwrap();
        let mut section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        section.bucket = Some("old-bucket".to_string());
        section.endpoint = Some("https://oss-cn-beijing.aliyuncs.com".to_string());
        section.region = Some("cn-beijing".to_string());
        section.access_key_id = Some("ak".to_string());
        section.prefix = Some("old".to_string());
        section.path_style = Some(false);
        section.secret_access_key_file = Some("oss.secret".to_string());
        config.backends.push(section);
        config.save(&paths).unwrap();
        write_secret_file(&paths, "oss.secret", "keep-me").unwrap();

        let draft = cx_tui::TuiBackendDraft {
            kind: cx_tui::WizardKind::S3,
            values: vec![
                "oss".into(),
                "new-bucket".into(),
                String::new(),
                "https://oss-cn-beijing.aliyuncs.com".into(),
                "cn-beijing".into(),
                "ak".into(),
                String::new(), // 密钥留空 = 沿用
                "new-prefix".into(),
                "virtual-host".into(),
                String::new(),
            ],
        };
        let probed = std::cell::Cell::new(false);
        update_draft_with_probe(&paths, "oss", &draft, |_, _, section| {
            probed.set(true);
            assert_eq!(section.prefix.as_deref(), Some("new-prefix"));
            Ok(())
        })
        .unwrap();
        assert!(probed.get(), "编辑也要先做连通性自测");

        let saved = Config::load(&paths).unwrap();
        let section = saved.backends.first().unwrap();
        assert_eq!(section.bucket.as_deref(), Some("new-bucket"));
        assert_eq!(section.prefix.as_deref(), Some("new-prefix"));
        assert_eq!(
            section.secret_access_key_file.as_deref(),
            Some("oss.secret"),
            "密钥字段留空必须沿用旧文件"
        );
        assert_eq!(
            std::fs::read_to_string(paths.secret_file("oss.secret")).unwrap(),
            "keep-me"
        );
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }

    #[test]
    fn 后端改名后旧凭据文件会被清掉() {
        let paths = temp_paths("edit-rename");
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        let mut config = Config::load_or_create(&paths).unwrap();
        let mut section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        section.bucket = Some("b".to_string());
        section.access_key_id = Some("ak".to_string());
        section.secret_access_key_file = Some("oss.secret".to_string());
        config.backends.push(section);
        config.save(&paths).unwrap();
        write_secret_file(&paths, "oss.secret", "old-key").unwrap();

        let draft = cx_tui::TuiBackendDraft {
            kind: cx_tui::WizardKind::S3,
            values: vec![
                "oss2".into(),
                "b".into(),
                String::new(),
                String::new(),
                String::new(),
                "ak".into(),
                "new-key".into(),
                String::new(),
                String::new(),
                String::new(),
            ],
        };
        update_draft_with_probe(&paths, "oss", &draft, |_, _, _| Ok(())).unwrap();

        assert!(
            !paths.secret_file("oss.secret").exists(),
            "改名后旧密钥文件必须删掉，不能永久留在磁盘上"
        );
        assert!(
            paths.secret_file("oss2.secret").exists(),
            "新名字的密钥要落盘"
        );
        let saved = Config::load(&paths).unwrap();
        assert_eq!(saved.backends[0].name, "oss2");
        assert_eq!(
            saved.backends[0].secret_access_key_file.as_deref(),
            Some("oss2.secret")
        );
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
    }
    #[test]
    fn 编辑探测失败时保持原配置() {
        let paths = temp_paths("edit-probe-fail");
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        let mut config = Config::load_or_create(&paths).unwrap();
        let mut section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        section.bucket = Some("old".to_string());
        section.access_key_id = Some("ak".to_string());
        section.secret_access_key_file = Some("oss.secret".to_string());
        config.backends.push(section);
        config.save(&paths).unwrap();
        write_secret_file(&paths, "oss.secret", "keep-me").unwrap();
        let before = std::fs::read_to_string(paths.config_file()).unwrap();

        let draft = cx_tui::TuiBackendDraft {
            kind: cx_tui::WizardKind::S3,
            values: vec![
                "oss".into(),
                "new-bucket".into(),
                String::new(),
                String::new(),
                "cn-beijing".into(),
                "ak".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
        };
        let err = update_draft_with_probe(&paths, "oss", &draft, |_, _, _| {
            Err(DaemonError::Protocol("连不上".to_string()))
        })
        .unwrap_err();
        assert!(err.to_string().contains("连不上"), "{err}");
        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            before,
            "探测失败不该动配置"
        );
        let _ = std::fs::remove_dir_all(paths.config_dir.parent().unwrap());
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
                session_token_stdin: false,
                path_style: true,
                virtual_host: false,
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

        // S3：预置测试桶，验证 probe 不会尝试创建它。
        let s3 = TestS3Server::start("minioadmin", "minioadmin", "clipboardx");
        s3.seed("fixture", b"fake");
        let mut s3_section = empty_section("oss".to_string(), cx_core::BackendKind::S3);
        s3_section.bucket = Some("clipboardx".to_string());
        s3_section.endpoint = Some(s3.endpoint());
        s3_section.access_key_id = Some("minioadmin".to_string());
        s3_section.secret_access_key_file = Some("fake-s3.key".to_string());
        write_secret_file(&paths, "fake-s3.key", "minioadmin").unwrap();
        s3_section.path_style = Some(true);
        config.backends.push(s3_section);
        config.save(&paths).unwrap();

        assert!(test_backend(&paths, "dav").unwrap(), "WebDAV 后端应当可用");
        assert!(test_backend(&paths, "oss").unwrap(), "S3 后端应当可用");
        assert_eq!(s3.buckets_created(), 0, "probe 不应建桶");
        assert!(
            s3.signature_failures().is_empty(),
            "{:?}",
            s3.signature_failures()
        );

        // 不存在的后端名：给出明确失败而不是 panic
        assert!(!test_backend(&paths, "nope").unwrap());
        let _ = std::fs::remove_dir_all(&paths.config_dir);
    }
}
