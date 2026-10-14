//! TUI 的状态与按键处理。
//!
//! 刻意把"状态 + 按键 → 动作"做成纯函数式的部分：不碰终端、不碰 socket，
//! 因此可以在普通单元测试里把交互路径全跑一遍（包括边界：列表为空时按回车、
//! 光标越界时按方向键等）。

use crate::client::UiEvent;
use cx_proto::control::{HistoryPreview, HistorySummary, StatusReport};

/// 按键处理的结果（由调用方翻译成对守护进程的请求）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// 什么都没发生。
    None,
    /// 退出。
    Quit,
    /// 把第 N 条（0 = 最新）写回剪贴板。
    Copy(usize),
    /// 暂停同步。
    Pause,
    /// 恢复同步。
    Resume,
    /// 刷新状态与历史。
    Refresh,
    /// 把本机剪贴板内容立即发布出去。
    Push,
    /// 重新读取配置。
    Reload,
}

/// 一条提示信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// 文本。
    pub text: String,
    /// 是否是错误。
    pub is_error: bool,
}

/// 界面状态。
#[derive(Debug, Default)]
pub struct App {
    /// 守护进程状态（未连上时为 None）。
    pub status: Option<StatusReport>,
    /// 历史列表（0 = 最新）。
    pub items: Vec<HistorySummary>,
    /// 当前光标位置（在列表里的下标，不是 history position）。
    pub cursor: usize,
    /// 选中项的完整内容。
    pub preview: Option<HistoryPreview>,
    /// 拉预览失败的原因。
    pub preview_error: Option<String>,
    /// 最近一条提示。
    pub notice: Option<Notice>,
    /// 是否显示帮助。
    pub show_help: bool,
    /// 后台线程是否还在跑。
    pub running: bool,
}

impl App {
    /// 新建（running = true，等后台线程报告连接结果）。
    pub fn new() -> Self {
        Self {
            running: true,
            ..Self::default()
        }
    }

    /// 记一条提示。
    pub fn note(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: false,
        });
    }

    /// 记一条错误提示。
    pub fn warn(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: true,
        });
    }

    /// 应用一条后台消息。
    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Status(report) => {
                self.status = Some(*report);
            }
            UiEvent::History(items) => {
                self.items = items;
                // 列表变短时把光标夹回范围内
                self.clamp();
                // 选中项变了，旧预览就作废
                self.preview = None;
                self.preview_error = None;
            }
            UiEvent::Preview(preview) => {
                self.preview = Some(preview);
                self.preview_error = None;
            }
            UiEvent::Failure { command, message } => {
                if command == "history.show" {
                    self.preview_error = Some(message.clone());
                }
                self.warn(format!("{command} 失败：{message}"));
            }
            UiEvent::Disconnected => {
                self.running = false;
                self.warn("与守护进程的连接已断开");
            }
            UiEvent::Info(text) => self.note(text),
        }
    }

    /// 光标所在的 history position（0 = 最新）。
    pub fn selected_position(&self) -> Option<usize> {
        self.items.get(self.cursor).map(|item| item.position)
    }

    /// 光标上移。
    pub fn up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.preview = None;
            self.preview_error = None;
        }
    }

    /// 光标下移。
    pub fn down(&mut self) {
        if self.cursor + 1 < self.items.len() {
            self.cursor += 1;
            self.preview = None;
            self.preview_error = None;
        }
    }

    /// 首页 / 末页。
    pub fn home(&mut self) {
        self.cursor = 0;
        self.preview = None;
    }

    /// 跳到末尾。
    pub fn end(&mut self) {
        self.cursor = self.items.len().saturating_sub(1);
        self.preview = None;
    }

    fn clamp(&mut self) {
        if self.items.is_empty() {
            self.cursor = 0;
        }
        else if self.cursor >= self.items.len() {
            self.cursor = self.items.len() - 1;
        }
    }

    /// 处理一个按键。
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> KeyOutcome {
        use crossterm::event::KeyCode;

        if self.show_help {
            // 帮助页里任意键返回（q 直接退出，避免"被困在帮助页"）
            return match key.code {
                KeyCode::Char('q') => KeyOutcome::Quit,
                _ => {
                    self.show_help = false;
                    KeyOutcome::None
                }
            };
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => KeyOutcome::Quit,
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = true;
                KeyOutcome::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.up();
                KeyOutcome::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.down();
                KeyOutcome::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.home();
                KeyOutcome::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.end();
                KeyOutcome::None
            }
            KeyCode::Enter => match self.selected_position() {
                Some(position) => KeyOutcome::Copy(position),
                None => {
                    self.warn("历史里还没有内容可复制");
                    KeyOutcome::None
                }
            },
            KeyCode::Char(' ') => {
                let paused = self.status.as_ref().map(|status| status.paused).unwrap_or(false);
                if paused {
                    KeyOutcome::Resume
                }
                else {
                    KeyOutcome::Pause
                }
            }
            KeyCode::Char('r') => KeyOutcome::Refresh,
            KeyCode::Char('p') => KeyOutcome::Push,
            KeyCode::Char('R') => KeyOutcome::Reload,
            _ => KeyOutcome::None,
        }
    }

    /// 状态栏一行摘要（也用于测试断言）。
    pub fn status_line(&self) -> String {
        match &self.status {
            None => "未连接守护进程".to_string(),
            Some(status) => {
                let state = if status.paused { "已暂停" } else { "同步中" };
                let space = if status.encrypted {
                    status.space_id.as_deref().unwrap_or("-").to_string()
                }
                else {
                    "未加密".to_string()
                };
                format!(
                    "{state}｜设备 {}（{}）｜空间 {}｜历史 {} 条｜后端 {} 个",
                    status.device_name,
                    short(&status.device_id),
                    space,
                    status.history_len,
                    status.backends.len()
                )
            }
        }
    }
}

/// 取前 8 位（设备 id 很长，界面上没必要全展示）。
pub fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn summary(position: usize) -> HistorySummary {
        HistorySummary {
            position,
            name: format!("1759323998000-ab12cd34-9f3c1e77-{position}"),
            kind: "text".to_string(),
            created_ms: 1_759_323_998_000 + position as u64,
            bytes: 10,
            hash: "a".repeat(64),
            origin: "cli".to_string(),
            origin_device: "ab12cd34".to_string(),
        }
    }

    /// 用 JSON 构造状态报告：字段增删时测试不会碎，也更贴近真实到达路径。
    fn status(paused: bool) -> StatusReport {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "pid": 42,
            "started_ms": 1_759_323_000_000u64,
            "paused": paused,
            "device_id": "ab12cd34ef567890",
            "device_name": "笔记本",
            "space_id": "space-1",
            "encrypted": true,
            "backends": [],
            "history_len": 3,
            "seen_len": 0,
            "skipped_oversize": 0,
            "skipped_ignored": 0,
            "last_published_hash": null,
            "clipboard": "系统剪贴板（测试）",
            "watch": "平台事件通知",
            "no_clipboard": false
        }))
        .expect("状态 JSON 应当能反序列化")
    }

    #[test]
    fn 方向键在边界上不越界() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![summary(0), summary(1)]));

        app.up();
        assert_eq!(app.cursor, 0, "已经在顶部，不该继续上移");
        app.down();
        app.down();
        assert_eq!(app.cursor, 1, "到底部后不该继续下移");
        assert_eq!(app.selected_position(), Some(1));

        app.home();
        assert_eq!(app.selected_position(), Some(0));
        app.end();
        assert_eq!(app.selected_position(), Some(1));
    }

    #[test]
    fn 空历史时回车给出提示而不是崩溃() {
        let mut app = App::new();
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::None);
        assert!(app.notice.as_ref().unwrap().is_error);
        assert!(app.notice.as_ref().unwrap().text.contains("还没有内容"));
    }

    #[test]
    fn 回车复制当前选中项() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![summary(0), summary(1), summary(2)]));
        app.down();
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::Copy(1));
    }

    #[test]
    fn 空格按当前状态暂停或恢复() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        assert_eq!(app.on_key(key(KeyCode::Char(' '))), KeyOutcome::Pause);
        app.apply(UiEvent::Status(Box::new(status(true))));
        assert_eq!(app.on_key(key(KeyCode::Char(' '))), KeyOutcome::Resume);
        // 还没连上时按空格：默认当作"暂停"（宁可停下也别在不知情时同步）
        let mut fresh = App::new();
        assert_eq!(fresh.on_key(key(KeyCode::Char(' '))), KeyOutcome::Pause);
    }

    #[test]
    fn 帮助页不会困住用户() {
        let mut app = App::new();
        assert_eq!(app.on_key(key(KeyCode::Char('?'))), KeyOutcome::None);
        assert!(app.show_help);
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), KeyOutcome::None);
        assert!(!app.show_help, "任意键应关闭帮助");

        assert_eq!(app.on_key(key(KeyCode::Char('?'))), KeyOutcome::None);
        assert!(app.show_help);
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), KeyOutcome::Quit, "帮助页里 q 应直接退出");
    }

    #[test]
    fn 列表变短后光标被夹住() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![summary(0), summary(1), summary(2)]));
        app.end();
        assert_eq!(app.cursor, 2);
        app.apply(UiEvent::History(vec![summary(0)]));
        assert_eq!(app.cursor, 0, "列表变短后光标必须回到范围内");
        assert_eq!(app.selected_position(), Some(0));
    }

    #[test]
    fn 选中项变化会作废旧预览() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![summary(0), summary(1)]));
        app.apply(UiEvent::Preview(HistoryPreview {
            position: 0,
            kind: "text".to_string(),
            text: Some("hello".to_string()),
            image: None,
            truncated: false,
            bytes: 5,
        }));
        assert!(app.preview.is_some());
        app.down();
        assert!(app.preview.is_none(), "换了选中项，旧预览应当清空");
    }

    #[test]
    fn 连接断开会结束界面() {
        let mut app = App::new();
        app.apply(UiEvent::Disconnected);
        assert!(!app.running);
        assert!(app.notice.as_ref().unwrap().is_error);
    }

    #[test]
    fn 状态行包含关键信息() {
        let mut app = App::new();
        assert!(app.status_line().contains("未连接"));
        app.apply(UiEvent::Status(Box::new(status(false))));
        let line = app.status_line();
        assert!(line.contains("同步中"), "{line}");
        assert!(line.contains("笔记本"), "{line}");
        assert!(line.contains("ab12cd34"), "{line}");
        assert!(line.contains("历史 3 条"), "{line}");

        app.apply(UiEvent::Status(Box::new(status(true))));
        assert!(app.status_line().contains("已暂停"));
    }
}
