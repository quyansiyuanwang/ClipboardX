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
}

/// 进入全屏 TUI。
pub fn run(paths: &Paths) -> Result<TuiExit, TuiError> {
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

    let result = run_loop(&mut terminal, paths);

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
) -> Result<TuiExit, TuiError> {
    use crossterm::event::{self, Event as TermEvent, KeyEventKind};

    let mut app = App::new();
    let client = DaemonClient::start(paths.clone());
    app.note("正在连接守护进程…");

    let tick = Duration::from_millis(DEFAULT_REDRAW_MS);
    let mut exit = TuiExit::Quit;

    loop {
        // 1) 处理后台线程送来的消息
        while let Ok(event) = client.try_recv() {
            app.apply(event);
        }

        // 2) 为**当前可见窗口**补拉内容摘要（见 app.rs 的说明：位置会漂移，
        //    所以请求带上对象名，结果按名字贴回对应的行）
        for (position, name) in app.take_preview_requests() {
            client.request_preview(position, name);
        }

        // 3) 画一帧（渲染同时回填可视行数，供预取与翻页使用）
        terminal
            .draw(|frame| ui::draw(frame, &mut app))
            .map_err(|err| TuiError::Io(format!("绘制失败：{err}")))?;

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
            if let TermEvent::Mouse(mouse) = event {
                use crossterm::event::MouseEventKind;
                match mouse.kind {
                    MouseEventKind::ScrollDown => app.on_scroll(3),
                    MouseEventKind::ScrollUp => app.on_scroll(-3),
                    // 左键点列表某一行就选中它（坐标换算由 App 负责：它手里有渲染时
                    // 回填的列表位置与滚动偏移）
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        app.click(mouse.row);
                    }
                    _ => {}
                }
                continue;
            }
            if let TermEvent::Key(key) = event {
                // Windows 上按键会同时报 Press 与 Release，只处理按下
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match app.on_key(key) {
                    KeyOutcome::None => {}
                    KeyOutcome::Quit => break,
                    KeyOutcome::Copy(position) => client.request_copy(position),
                    KeyOutcome::Pause => client.request("pause"),
                    KeyOutcome::Resume => client.request("resume"),
                    KeyOutcome::Refresh => {
                        client.request_status();
                        client.request_history(HISTORY_LIMIT);
                        // 历史整体重来，摘要缓存也重来
                        app.forget_previews();
                    }
                    KeyOutcome::Doctor => client.request_doctor(),
                    // 表单填完：退出界面，把草稿交回命令层
                    KeyOutcome::AddBackend(draft) => {
                        exit = TuiExit::AddBackend(draft);
                        break;
                    }
                    // 面板里确认移除后端：同样退出界面交给命令层处理
                    KeyOutcome::RemoveBackend(name) => {
                        exit = TuiExit::RemoveBackend(name);
                        break;
                    }
                    KeyOutcome::ToggleBackend(name) => {
                        exit = TuiExit::ToggleBackend(name);
                        break;
                    }
                    KeyOutcome::Delete(position) => {
                        client.request_delete(position);
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

    let backend = ratatui::backend::TestBackend::new(width.max(20), height.max(6));
    let mut terminal = ratatui::Terminal::new(backend)
        .map_err(|err| TuiError::Io(format!("无法创建测试终端：{err}")))?;
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
