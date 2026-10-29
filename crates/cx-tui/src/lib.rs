//! 终端界面（ratatui + crossterm）。
//!
//! 设计约束（与 CLI 同源）：**只通过控制套接字与守护进程通信**——不直接读剪贴板、
//! 不直接访问网络、不读守护进程的私有状态文件。因此 TUI 可以随时换成别的前端，
//! 而能力集合完全一致。
//!
//! 线程模型：UI 线程只做"画一帧 + 读键盘"，所有 IO（连接、请求、订阅事件）
//! 都在一个后台线程里做，结果通过 channel 送回。这样网络卡住时界面不会僵死，
//! 退出也不会被一个挂着读 socket 的线程拖住。
//!
//! 可测性：渲染逻辑与终端解耦（`ui::draw` 接受任意 `Backend`），因此可以用
//! ratatui 的 `TestBackend` 在无终端环境下断言"画面上有什么"；
//! `cx tui --snapshot` 更是直接把一帧打到 stdout，便于端到端脚本验证。

mod app;
mod client;
mod error;
mod ui;
mod wizard;

pub use app::{App, KeyOutcome};
pub use client::{DaemonClient, UiEvent};
pub use error::TuiError;
pub use wizard::BackendDraft as TuiBackendDraft;
pub use wizard::{BackendDraft, Wizard, WizardKind};

use cx_core::Paths;
use cx_proto::control::HistoryPreview;
use std::io::Write;
use std::time::Duration;

/// TUI 事件流默认刷新间隔（毫秒）——仅用于兜底重绘，实时更新依赖守护进程推送。
pub const DEFAULT_REDRAW_MS: u64 = 1000;

/// 历史列表一次取多少条。
pub const HISTORY_LIMIT: usize = 500;

/// 界面退出的原因。
///
/// `AddBackend` 用于"用户在界面里填完表单"这种情形：界面只负责收集，落盘与自测由
/// 命令层完成（`cx tui` 与 `cx backend add` 是同一个二进制）。
/// 注意：接入了 [`BackendAdmin`] 之后这些动作都不再退出界面——它们留在
/// [`TuiExit`] 里只是为了"没有接入回调的调用方（比如独立复用 cx-tui 的测试）"仍能工作。
#[derive(Debug)]
pub enum TuiExit {
    /// 用户主动退出。
    Quit,
    /// 表单填完了，交给调用方去添加后端。
    AddBackend(Box<BackendDraft>),
    /// 用户在面板里确认移除某个后端，交给调用方去改配置。
    RemoveBackend(String),
    /// 用户在面板里停用 / 启用某个后端。
    ToggleBackend(String),
    /// 用户在面板里要建加密空间（并发布到该后端）。
    CreateSpace(String),
    /// 用户在面板里要导出空间密钥。
    ExportKey,
    /// 用户在密钥页上要把它写到文件里。
    ExportKeyToFile,
    /// 用户要编辑某个后端。
    EditBackend(String),
    /// 用户提交了对已有后端的修改。
    SaveBackend {
        /// 原后端名。
        original: String,
        /// 表单结果。
        draft: Box<BackendDraft>,
    },
    /// 用户要把空间元数据重新发布到某个后端。
    PublishSpace(String),
    /// 用户要导出整包。
    ExportBundle,
    /// 用户要导入整包。
    ImportBundle,
    /// 用户要打开配置页。
    OpenConfig,
    /// 用户要修改一个配置项。
    SetConfig {
        /// 键名。
        key: String,
        /// 新值。
        value: String,
    },
    /// 用户要重启守护进程。
    RestartDaemon,
    /// 用户在面板里要用密钥加入空间。
    JoinSpace {
        /// 后端名。
        backend: String,
        /// 空间密钥。
        key: String,
    },
}

/// 界面里能直接执行的后端管理动作。
///
/// 动机：停用 / 移除 / 新增原来都要**先退出界面**把动作交回命令层，改一次配置就得
/// 离开界面再进来一次（面板里的选中位置、诊断内容全丢）。把动作做成回调后，界面
/// 保持不动，只把结果回填成一行提示，面板原地刷新。
///
/// 约定：返回 `Ok(提示)` / `Err(原因)` 都是给用户看的一句话；实现方负责重启守护进程
/// 之类的副作用，界面只负责显示。
pub trait BackendAdmin {
    /// 停用 / 启用，返回结果提示。
    fn toggle(&mut self, name: &str) -> Result<String, String>;
    /// 移除后端，返回结果提示。
    fn remove(&mut self, name: &str) -> Result<String, String>;
    /// 保存新后端（含连通性自测），返回结果提示。
    fn add(&mut self, draft: &BackendDraft) -> Result<String, String>;
    /// 在本机建一个加密空间，并把空间元数据发布到指定后端。
    ///
    /// 口令由实现方随机生成并在结果里回显一次（用户要拿它给第二台设备 `space join`），
    /// 因为界面里让用户当场想一个强口令不现实、留空又会退化成明文。
    fn create_space(&mut self, backend: &str, force: bool) -> Result<String, String>;
    /// 用空间密钥加入已有空间（`key` 是 64 位十六进制）。返回结果提示。
    fn join_space(&mut self, backend: &str, key: &str) -> Result<String, String>;
    /// 导出空间密钥（等价于 `cx space export-key`）。返回 64 位十六进制。
    fn export_key(&mut self) -> Result<String, String>;
    /// 把空间密钥写到文件里，返回写到的路径（供界面回显）。
    fn write_key_file(&mut self) -> Result<String, String>;
    /// 取某个后端的现值，做成表单草稿（用于"编辑"）。
    fn edit_draft(&mut self, name: &str) -> Result<BackendDraft, String>;
    /// 保存对已有后端的修改（含连通性自测）。
    fn update(&mut self, original: &str, draft: &BackendDraft) -> Result<String, String>;
    /// 把本机空间元数据重新发布到某后端（等价 `cx space publish`）。
    fn publish_space(&mut self, backend: &str) -> Result<String, String>;
    /// 整套配置导出到文件（等价 `cx config export`），返回路径。
    fn export_bundle(&mut self) -> Result<String, String>;
    /// 从文件导入整套配置（等价 `cx config import --force`），返回路径。
    fn import_bundle(&mut self) -> Result<String, String>;
    /// 列出可设置的配置项（键名 + 当前值），等价 `cx config get`。
    fn config_items(&mut self) -> Result<Vec<(String, String)>, String>;
    /// 修改一个配置项（等价 `cx config set`）。
    fn set_config(&mut self, key: &str, value: &str) -> Result<String, String>;
    /// 重启守护进程（等价 `cx restart`）。
    fn restart_daemon(&mut self) -> Result<String, String>;
}

/// 进入全屏 TUI。
pub fn run(paths: &Paths) -> Result<TuiExit, TuiError> {
    run_with_admin(paths, &mut App::new(), None)
}

/// 使用保留的界面状态再次进入，管理操作失败时无需重填草稿。
pub fn run_with_app(paths: &Paths, app: &mut App) -> Result<TuiExit, TuiError> {
    run_with_admin(paths, app, None)
}

/// 使用保留的界面状态进入，并把后端管理动作交给调用方在界面内执行。
///
/// `admin` 为 `None` 时，面板里的停用 / 移除 / 添加会以 [`TuiExit`] 的形式交回调用方
/// （老行为）。
pub fn run_with_admin(
    paths: &Paths,
    app: &mut App,
    admin: Option<&mut dyn BackendAdmin>,
) -> Result<TuiExit, TuiError> {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    };
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .map_err(|err| TuiError::Io(format!("无法初始化终端：{err}")))?;
    enable_raw_mode().map_err(|err| TuiError::Io(format!("无法进入 raw 模式：{err}")))?;
    let mut stdout = std::io::stdout();
    let _ = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        // 括号粘贴：整段文本一次送到，不再逐字符模拟按键
        EnableBracketedPaste
    );

    let result = run_loop(&mut terminal, paths, app, admin);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = terminal.show_cursor();
    result
}

fn run_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    paths: &Paths,
    app: &mut App,
    mut admin: Option<&mut dyn BackendAdmin>,
) -> Result<TuiExit, TuiError> {
    use crossterm::event::{self, Event as TermEvent, KeyEventKind};

    let client = DaemonClient::start(paths.clone());
    app.note("正在连接守护进程…");

    let tick = Duration::from_millis(DEFAULT_REDRAW_MS);
    let mut exit = TuiExit::Quit;

    loop {
        // 1) 处理后台线程送来的消息
        while let Ok(event) = client.try_recv() {
            app.apply(event);
        }

        // 2) 先画一帧：布局/滚动偏移是预取窗口的唯一权威来源。
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|err| TuiError::Io(format!("绘制失败：{err}")))?;

        // 3) 按渲染后的实际可见窗口补拉摘要。
        for (position, name) in app.take_preview_requests() {
            client.request_preview(position, name);
        }

        // 4) 等键盘（最多等一个刷新间隔）
        if event::poll(tick).map_err(|err| TuiError::Io(format!("读取键盘事件失败：{err}")))?
        {
            let event =
                event::read().map_err(|err| TuiError::Io(format!("读取键盘事件失败：{err}")))?;
            // 滚轮翻列表：EnableMouseCapture 早就开了，但一直没处理鼠标事件，
            // 于是滚轮在界面里完全没反应。
            // 粘贴整段文本：交给界面按字段处理（含去掉末尾换行的处理）
            if let TermEvent::Paste(text) = &event {
                app.on_paste(text);
                continue;
            }
            let outcome = match event {
                TermEvent::Mouse(mouse) => app.on_mouse(mouse),
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                _ => continue,
            };
            {
                match outcome {
                    KeyOutcome::None => {}
                    KeyOutcome::Quit => break,
                    KeyOutcome::Copy(position) => {
                        if let Some(item) = app.selected_item() {
                            client.request_copy_checked(position, item.name.clone());
                        }
                    }
                    KeyOutcome::Pause => client.request("pause"),
                    KeyOutcome::Resume => client.request("resume"),
                    KeyOutcome::Refresh => {
                        client.request_status();
                        client.request_history(HISTORY_LIMIT);
                        // 历史整体重来，摘要缓存也重来
                        app.forget_previews();
                    }
                    KeyOutcome::Doctor => client.request_doctor(),
                    // 表单填完：有回调就在界面里直接保存，否则退出界面交回命令层
                    KeyOutcome::AddBackend(draft) => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::AddBackend(draft);
                            break;
                        }
                        Some(handler) => run_admin_in_place(
                            terminal,
                            app,
                            &client,
                            handler,
                            KeyOutcome::AddBackend(draft),
                        ),
                    },
                    // 面板里确认移除后端
                    KeyOutcome::RemoveBackend(name) => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::RemoveBackend(name);
                            break;
                        }
                        Some(handler) => run_admin_in_place(
                            terminal,
                            app,
                            &client,
                            handler,
                            KeyOutcome::RemoveBackend(name),
                        ),
                    },
                    KeyOutcome::ToggleBackend(name) => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::ToggleBackend(name);
                            break;
                        }
                        Some(handler) => run_admin_in_place(
                            terminal,
                            app,
                            &client,
                            handler,
                            KeyOutcome::ToggleBackend(name),
                        ),
                    },
                    KeyOutcome::ExportKey => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::ExportKey;
                            break;
                        }
                        Some(handler) => match handler.export_key() {
                            Ok(key) => app.exported_key = Some(key),
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::EditBackend(name) => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::EditBackend(name);
                            break;
                        }
                        Some(handler) => match handler.edit_draft(&name) {
                            Ok(draft) => {
                                let mut form = Wizard::from_draft(&draft, String::new());
                                // 编辑：密文字段留空表示沿用现值，标题也要换成"编辑"
                                form.set_edit_mode(true);
                                app.wizard = Some(form);
                                app.wizard_edit_target = Some(name);
                            }
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::SaveBackend { original, draft } => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::SaveBackend { original, draft };
                            break;
                        }
                        Some(handler) => {
                            match handler.update(&original, &draft) {
                                Ok(message) => app.note(message),
                                Err(message) => app.warn(message),
                            }
                            app.show_panel = true;
                        }
                    },
                    KeyOutcome::PublishSpace(backend) => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::PublishSpace(backend);
                            break;
                        }
                        Some(handler) => match handler.publish_space(&backend) {
                            Ok(message) => app.note(message),
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::OpenConfig => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::OpenConfig;
                            break;
                        }
                        Some(handler) => match handler.config_items() {
                            Ok(items) => {
                                app.config_page = Some(crate::app::ConfigPage {
                                    items,
                                    cursor: 0,
                                    editing: None,
                                })
                            }
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::SetConfig { key, value } => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::SetConfig { key, value };
                            break;
                        }
                        Some(handler) => {
                            match handler.set_config(&key, &value) {
                                Ok(message) => app.note(message),
                                Err(message) => app.warn(message),
                            }
                            // 重新读一遍：值可能被规范化（如布尔 true/false）
                            if let Ok(items) = handler.config_items()
                                && let Some(page) = app.config_page.as_mut()
                            {
                                page.items = items;
                                page.editing = None;
                            }
                        }
                    },
                    KeyOutcome::RestartDaemon => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::RestartDaemon;
                            break;
                        }
                        Some(handler) => match handler.restart_daemon() {
                            Ok(message) => app.note(message),
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::ExportBundle => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::ExportBundle;
                            break;
                        }
                        Some(handler) => match handler.export_bundle() {
                            Ok(message) => app.note(message),
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::ImportBundle => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::ImportBundle;
                            break;
                        }
                        Some(handler) => match handler.import_bundle() {
                            Ok(message) => app.note(message),
                            Err(message) => app.warn(message),
                        },
                    },
                    KeyOutcome::ExportKeyToFile => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::ExportKeyToFile;
                            break;
                        }
                        Some(handler) => match handler.write_key_file() {
                            Ok(path) => app.exported_key_note = Some(format!("已写入 {path}")),
                            Err(message) => {
                                app.exported_key_note = Some(format!("写入失败：{message}"))
                            }
                        },
                    },
                    KeyOutcome::CreateSpace { backend, force } => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::CreateSpace(backend);
                            break;
                        }
                        Some(handler) => run_admin_in_place(
                            terminal,
                            app,
                            &client,
                            handler,
                            KeyOutcome::CreateSpace { backend, force },
                        ),
                    },
                    KeyOutcome::JoinSpace { backend, key } => match admin.as_deref_mut() {
                        None => {
                            exit = TuiExit::JoinSpace { backend, key };
                            break;
                        }
                        Some(handler) => run_admin_in_place(
                            terminal,
                            app,
                            &client,
                            handler,
                            KeyOutcome::JoinSpace { backend, key },
                        ),
                    },
                    KeyOutcome::Delete(position) => {
                        if let Some(name) = app.delete_expected_name.take() {
                            client.request_delete_checked(position, name);
                        } else {
                            app.warn("删除缺少对象标识，请重新选择条目");
                        }
                        // 删完立刻刷新：列表里那一条应当马上消失
                        client.request_history(HISTORY_LIMIT);
                        app.forget_previews();
                    }
                    KeyOutcome::Push => client.request_push(),
                    KeyOutcome::Reload => client.request("reload"),
                }
            }
        }

        if !app.running {
            break;
        }
    }

    client.shutdown();
    Ok(exit)
}

/// 管理动作要等网络与磁盘，先把"正在做…"画出来，别让界面看着像卡住。
fn begin_inline_action(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    message: impl Into<String>,
) {
    app.note(message);
    // 画不出来也只能继续：动作本身比这一帧重要
    let _ = terminal.draw(|frame| ui::draw(frame, app));
}

/// 在界面里跑完一个管理动作：先画"正在做…"，再执行，再把结果画出来。
fn run_admin_in_place(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &DaemonClient,
    handler: &mut dyn BackendAdmin,
    outcome: KeyOutcome,
) {
    if let Some(message) = pending_message(&outcome) {
        begin_inline_action(terminal, app, message);
    }
    perform_admin_action(app, handler, outcome);
    refresh_after_action(app, client);
}

/// "正在做…"这句话（与动作一一对应，写在这里就不必在每个分支里各写一遍）。
fn pending_message(outcome: &KeyOutcome) -> Option<String> {
    match outcome {
        KeyOutcome::AddBackend(_) => Some("正在保存并自测新后端…".to_string()),
        KeyOutcome::RemoveBackend(name) => Some(format!("正在移除后端 {name}…")),
        KeyOutcome::ToggleBackend(name) => Some(format!("正在停用/启用后端 {name}…")),
        KeyOutcome::CreateSpace { backend, .. } => {
            Some(format!("正在建加密空间并上传到 {backend}…"))
        }
        KeyOutcome::JoinSpace { backend, .. } => {
            Some(format!("正在用密钥加入 {backend} 上的空间…"))
        }
        _ => None,
    }
}

/// 执行一个管理动作并把结果回填到界面状态里（不涉及绘制，因此可以单测）。
///
/// 返回 `true` 表示这是一个管理动作。三个动作的共同约定：
/// - 成功：留下面板 + 一行提示，让用户看到结果；
/// - 失败：同样留下面板/表单，只是提示是错误色 —— 不退出、不丢用户已经填好的东西。
fn perform_admin_action(
    app: &mut App,
    handler: &mut dyn BackendAdmin,
    outcome: KeyOutcome,
) -> bool {
    match outcome {
        KeyOutcome::AddBackend(draft) => {
            match handler.add(&draft) {
                Ok(message) => {
                    app.note(message);
                    // 回到面板并把新后端选中：用户刚做完的动作要看得见
                    app.show_panel = true;
                    app.select_backend(draft.name());
                }
                // 失败把草稿留在表单里：改一个字段就能重试，不必重填
                Err(message) => {
                    app.wizard = Some(Wizard::from_draft(&draft, message));
                }
            }
            true
        }
        KeyOutcome::RemoveBackend(name) => {
            match handler.remove(&name) {
                Ok(message) => app.note(message),
                Err(message) => app.warn(message),
            }
            app.show_panel = true;
            true
        }
        KeyOutcome::ToggleBackend(name) => {
            match handler.toggle(&name) {
                Ok(message) => app.note(message),
                Err(message) => app.warn(message),
            }
            app.show_panel = true;
            true
        }
        KeyOutcome::CreateSpace { backend, force } => {
            match handler.create_space(&backend, force) {
                Ok(message) => app.note(message),
                Err(message) => app.warn(message),
            }
            app.show_panel = true;
            true
        }
        KeyOutcome::JoinSpace { backend, key } => {
            match handler.join_space(&backend, &key) {
                Ok(message) => app.note(message),
                Err(message) => app.warn(message),
            }
            app.show_panel = true;
            true
        }
        _ => false,
    }
}

/// 管理动作多半重启了守护进程：状态、诊断与历史全部重新拉一次。
fn refresh_after_action(app: &mut App, client: &DaemonClient) {
    client.request_status();
    client.request_doctor();
    client.request_history(HISTORY_LIMIT);
    app.forget_previews();
}

/// 渲染一帧到 stdout（不需要终端，便于脚本与 CI 校验）。
///
/// 这是"TUI 到底读到了什么"的可验证出口：真实连接守护进程，取状态与历史，
/// 画一帧，然后把画面里的文字按行打印出来。
pub fn snapshot(paths: &Paths, width: u16, height: u16) -> Result<String, TuiError> {
    let client = DaemonClient::start(paths.clone());
    let mut app = App::new();
    client.request_status();
    client.request_history(HISTORY_LIMIT);

    // 等状态**与历史**都到齐。只等状态是不够的：历史晚到会让下面根本没机会请求预览，
    // 快照里就只剩"（正在读取…）"——CI 上就因为这个失败过一次。
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        while let Ok(event) = client.try_recv() {
            app.apply(event);
        }
        if (app.status.is_some() && !app.items.is_empty()) || std::time::Instant::now() >= deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let backend = ratatui::backend::TestBackend::new(width.max(20), height.max(6));
    let mut terminal = ratatui::Terminal::new(backend)
        .map_err(|err| TuiError::Io(format!("无法创建测试终端：{err}")))?;
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .map_err(|err| TuiError::Io(format!("绘制失败：{err}")))?;

    // 再等选中项的内容摘要：快照是"读到了什么"的证据，因此这里要等到位（或超时）
    for (position, name) in app.take_preview_requests() {
        client.request_preview(position, name);
    }
    let wanted = app.selected_item().map(|item| item.name.clone());
    if let Some(name) = wanted {
        loop {
            while let Ok(event) = client.try_recv() {
                app.apply(event);
            }
            if app.preview_state(&name).is_some() || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    client.shutdown();

    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .map_err(|err| TuiError::Io(format!("绘制失败：{err}")))?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// 把快照打到标准输出（给 `cx tui --snapshot` 用）。
pub fn print_snapshot(paths: &Paths) -> Result<(), TuiError> {
    let text = snapshot(paths, 100, 30)?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .map_err(|err| TuiError::Io(format!("写入标准输出失败：{err}")))?;
    Ok(())
}

/// 供测试与调用方判断"守护进程给的是哪种内容"。
pub fn preview_kind(preview: &HistoryPreview) -> &'static str {
    if preview.text.is_some() {
        "文本"
    } else if preview.image.is_some() {
        "图片"
    } else {
        "未知"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的"命令行层"：只记录调用，并按开关决定成功 / 失败。
    #[derive(Default)]
    struct FakeAdmin {
        toggled: Vec<String>,
        removed: Vec<String>,
        added: Vec<String>,
        spaces: Vec<String>,
        joins: Vec<(String, usize)>,
        edited: Vec<String>,
        updated: Vec<(String, String)>,
        published: Vec<String>,
        config_sets: Vec<(String, String)>,
        fail_add: Option<&'static str>,
        fail_toggle: Option<&'static str>,
    }

    impl BackendAdmin for FakeAdmin {
        fn toggle(&mut self, name: &str) -> Result<String, String> {
            match self.fail_toggle {
                Some(message) => Err(message.to_string()),
                None => {
                    self.toggled.push(name.to_string());
                    Ok(format!("后端 {name} 已停用"))
                }
            }
        }

        fn remove(&mut self, name: &str) -> Result<String, String> {
            self.removed.push(name.to_string());
            Ok(format!("后端 {name} 已移除"))
        }

        fn add(&mut self, draft: &BackendDraft) -> Result<String, String> {
            if let Some(message) = self.fail_add {
                return Err(message.to_string());
            }
            self.added.push(draft.name().to_string());
            Ok(format!("后端 {} 已保存", draft.name()))
        }

        fn create_space(&mut self, backend: &str, force: bool) -> Result<String, String> {
            self.spaces.push(format!("{backend}:{force}"));
            Ok(format!("已建空间 abc（后端 {backend}）；口令：p4ss"))
        }

        fn join_space(&mut self, backend: &str, key: &str) -> Result<String, String> {
            self.joins.push((backend.to_string(), key.len()));
            Ok(format!("已加入空间 abc（后端 {backend}）"))
        }

        fn export_key(&mut self) -> Result<String, String> {
            Ok("a".repeat(64))
        }

        fn write_key_file(&mut self) -> Result<String, String> {
            Ok("/tmp/cx-space-key.txt".to_string())
        }

        fn edit_draft(&mut self, name: &str) -> Result<BackendDraft, String> {
            self.edited.push(name.to_string());
            Ok(BackendDraft {
                kind: WizardKind::S3,
                values: vec![
                    "oss".into(),
                    "b".into(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "clip".into(),
                    String::new(),
                    String::new(),
                ],
            })
        }

        fn update(&mut self, original: &str, draft: &BackendDraft) -> Result<String, String> {
            self.updated
                .push((original.to_string(), draft.name().to_string()));
            Ok(format!("已保存 {original}"))
        }

        fn publish_space(&mut self, backend: &str) -> Result<String, String> {
            self.published.push(backend.to_string());
            Ok(format!("已把空间元数据发布到 {backend}"))
        }

        fn export_bundle(&mut self) -> Result<String, String> {
            Ok("/tmp/clipboardx-bundle.json".to_string())
        }

        fn import_bundle(&mut self) -> Result<String, String> {
            Ok("已导入".to_string())
        }

        fn config_items(&mut self) -> Result<Vec<(String, String)>, String> {
            Ok(vec![
                ("sync.poll_active_ms".to_string(), "1000".to_string()),
                ("lan.enabled".to_string(), "false".to_string()),
            ])
        }

        fn set_config(&mut self, key: &str, value: &str) -> Result<String, String> {
            self.config_sets.push((key.to_string(), value.to_string()));
            Ok(format!("{key} = {value}"))
        }

        fn restart_daemon(&mut self) -> Result<String, String> {
            Ok("守护进程已重启".to_string())
        }
    }

    fn draft(name: &str) -> BackendDraft {
        BackendDraft {
            kind: WizardKind::Server,
            values: vec![
                name.to_string(),
                "http://192.168.1.9:8688".to_string(),
                "space-1".to_string(),
                "token".to_string(),
            ],
        }
    }

    fn app_with_backends(names: &[&str]) -> App {
        let backends: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name, "kind": "server", "enabled": true, "failures": 0,
                    "next_attempt_ms": 0, "published": 0, "applied": 0
                })
            })
            .collect();
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": backends,
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        app
    }

    #[test]
    fn 添加成功回到面板并选中新行() {
        let mut app = app_with_backends(&["home"]);
        let mut admin = FakeAdmin::default();
        assert!(perform_admin_action(
            &mut app,
            &mut admin,
            KeyOutcome::AddBackend(Box::new(draft("office")))
        ));
        assert_eq!(admin.added, vec!["office".to_string()]);
        assert!(app.show_panel, "加完要留在面板里，而不是掉回历史列表");
        assert!(app.wizard.is_none(), "成功后表单要关掉");
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.clone()),
            Some("后端 office 已保存".to_string())
        );
        assert!(!app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn 添加失败把草稿留在表单里() {
        let mut app = app_with_backends(&[]);
        app.wizard = Some(Wizard::from_draft(&draft("office"), String::new()));
        let mut admin = FakeAdmin {
            fail_add: Some("连接超时：dial tcp 10.0.0.9:9000"),
            ..FakeAdmin::default()
        };
        perform_admin_action(
            &mut app,
            &mut admin,
            KeyOutcome::AddBackend(Box::new(draft("office"))),
        );
        let form = app.wizard.as_ref().expect("失败后表单必须留着，草稿不能丢");
        assert_eq!(form.value(0), "office", "已填的内容要原样保留");
        assert_eq!(
            form.message.as_deref(),
            Some("连接超时：dial tcp 10.0.0.9:9000"),
            "失败原因写在表单里，用户改一个字段就能重试"
        );
    }

    #[test]
    fn 停用成功回填提示_失败给错误提示() {
        let mut app = app_with_backends(&["home"]);
        let mut admin = FakeAdmin::default();
        perform_admin_action(
            &mut app,
            &mut admin,
            KeyOutcome::ToggleBackend("home".into()),
        );
        assert_eq!(admin.toggled, vec!["home".to_string()]);
        assert!(app.show_panel);
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.clone()),
            Some("后端 home 已停用".to_string())
        );

        let mut failing = FakeAdmin {
            fail_toggle: Some("配置文件只读"),
            ..FakeAdmin::default()
        };
        perform_admin_action(
            &mut app,
            &mut failing,
            KeyOutcome::ToggleBackend("home".into()),
        );
        let notice = app.notice.as_ref().unwrap();
        assert!(notice.is_error, "失败要用错误色提示");
        assert_eq!(notice.text, "配置文件只读");
        assert!(app.show_panel, "失败也要留在面板里，方便用户看状态再决定");
    }

    #[test]
    fn 非管理动作不碰界面状态() {
        let mut app = App::new();
        let mut admin = FakeAdmin::default();
        assert!(!perform_admin_action(
            &mut app,
            &mut admin,
            KeyOutcome::Quit
        ));
        assert!(admin.added.is_empty() && admin.toggled.is_empty() && admin.removed.is_empty());
    }
}
