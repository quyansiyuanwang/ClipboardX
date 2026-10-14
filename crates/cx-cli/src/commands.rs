//! 子命令定义与分发。

use crate::{doctor, output};
use clap::Subcommand;
use cx_core::{Config, Identity, LocalSpace, Paths};
use cx_daemon::{ControlClient, Daemon, DaemonError, DaemonOptions};
use cx_proto::control::cmd as commands;
use serde_json::{Map, json};
use std::process::ExitCode;


#[derive(Debug, Clone, Copy)]
pub struct TuiFallback;

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

    /// 打开终端界面
    Tui,
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

        Command::Tui => {
            let _ = TuiFallback;

            eprintln!("在此之前可以用：cx status / cx history / cx show / cx copy");
            Ok(ExitCode::from(2))
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
    let config = Config::load_or_create(paths)?;
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
