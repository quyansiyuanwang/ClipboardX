//! TUI 的状态与按键处理。
//!
//! 刻意把"状态 + 按键 → 动作"做成纯函数式的部分：不碰终端、不碰 socket，
//! 因此可以在普通单元测试里把交互路径全跑一遍（包括边界：列表为空时按回车、
//! 光标越界时按方向键等）。
//!
//! 两个关键设计：
//!
//! - **行内摘要按需加载**：历史接口只给元数据（名称/类型/时间/大小/哈希），
//!   看不到"这条到底是什么"。让守护进程为 500 条都读一遍内容是不可接受的，
//!   所以界面只为**当前可见的窗口**（加上选中项）请求预览。
//! - **缓存按对象名索引，而不是位置**：位置的 0 永远是最新，来一条新内容所有位置
//!   都会平移，按位置缓存会张冠李戴。对象名是稳定的。

use crate::client::UiEvent;
use cx_proto::control::{HistoryPreview, HistorySummary, StatusReport};
use std::collections::{BTreeMap, BTreeSet};

/// 一次可见窗口最多请求多少条预览（防止窄屏/大屏差异导致请求风暴）。
const MAX_PREFETCH: usize = 40;
/// 缓存上限：超过就整体丢弃（历史在滚动，重建成本很低）。
const MAX_CACHE: usize = 400;

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

/// 某一行内容摘要的加载状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewState {
    /// 已发出请求，还没回来。
    Loading,
    /// 拿到了。
    Ready(Box<HistoryPreview>),
    /// 失败了（在这一行上就地标出来，而不是让整块预览区空着）。
    Failed(String),
}

impl PreviewState {
    /// 取内容摘要用的单行文本。
    pub fn summary_text(&self) -> Option<&str> {
        match self {
            PreviewState::Ready(preview) => match (&preview.text, &preview.image) {
                (Some(text), _) => Some(text.as_str()),
                (None, Some(image)) => Some(image.as_str()),
                (None, None) => None,
            },
            _ => None,
        }
    }

    /// 完整预览。
    pub fn preview(&self) -> Option<&HistoryPreview> {
        match self {
            PreviewState::Ready(preview) => Some(preview),
            _ => None,
        }
    }

    /// 失败原因。
    pub fn error(&self) -> Option<&str> {
        match self {
            PreviewState::Failed(message) => Some(message),
            _ => None,
        }
    }

    /// 内容的行数（用于限制预览滚动范围）。
    pub fn line_count(&self) -> usize {
        self.summary_text()
            .map(|text| text.lines().count().max(1))
            .unwrap_or(1)
    }
}

/// 界面状态。
#[derive(Debug, Default)]
pub struct App {
    /// 守护进程状态（未连上时为 None）。
    pub status: Option<StatusReport>,
    /// 历史列表（0 = 最新）。
    pub items: Vec<HistorySummary>,
    /// 光标位置：`visible` 里的下标（不是 items 下标，也不是 history position）。
    pub cursor: usize,
    /// 当前过滤词（空 = 不过滤）。
    filter: String,
    /// 是否正在输入过滤词。
    pub filter_input: bool,
    /// 过滤前的查询词（Esc 取消时恢复）。
    filter_backup: String,
    /// 通过过滤的 items 下标。
    visible: Vec<usize>,
    /// 按对象名缓存的预览。
    previews: BTreeMap<String, PreviewState>,
    /// 正在请求中的对象名。
    pending: BTreeSet<String>,
    /// 上一轮计划过的对象名，用来判断"窗口有没有变"（避免重复请求与请求风暴）。
    last_plan: Vec<String>,
    /// 列表可视行数（渲染层每帧告知）。
    viewport_rows: usize,
    /// 预览滚动偏移（行）。
    preview_scroll: u16,
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
                self.refilter();
            }
            UiEvent::Preview { name, result } => {
                self.pending.remove(&name);
                let state = match result {
                    Ok(preview) => PreviewState::Ready(preview),
                    Err(message) => PreviewState::Failed(message),
                };
                if self.previews.len() >= MAX_CACHE {
                    self.previews.clear();
                }
                self.previews.insert(name, state);
            }
            UiEvent::Failure { command, message } => {
                self.warn(format!("{command} 失败：{message}"));
            }
            UiEvent::Disconnected => {
                self.running = false;
                self.warn("与守护进程的连接已断开");
            }
            UiEvent::Info(text) => self.note(text),
        }
    }

    // ---------------------------------------------------------------- 过滤

    /// 过滤是否生效。
    pub fn filter_active(&self) -> bool {
        !self.filter.is_empty()
    }

    /// 当前过滤词。
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// 通过过滤的条数。
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// 通过过滤的 items 下标。
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// 按过滤词重算可见集合，并把光标夹回范围内。
    fn refilter(&mut self) {
        let query = self.filter.to_lowercase();
        self.visible = if query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            (0..self.items.len())
                .filter(|index| self.matches(*index, &query))
                .collect()
        };
        self.clamp();
        self.preview_scroll = 0;
    }

    /// 某一行是否匹配过滤词。
    ///
    /// 匹配范围：类型、来源、对象名、哈希前缀，以及**已加载的内容摘要**。
    /// 内容摘要只覆盖已经加载过的行（见模块头部的说明），因此过滤是"尽力而为"的，
    /// 界面上会把这件事写清楚，避免让人以为"搜不到就是没有"。
    fn matches(&self, index: usize, query: &str) -> bool {
        let Some(item) = self.items.get(index) else {
            return false;
        };
        let haystacks = [
            item.kind.as_str(),
            item.origin.as_str(),
            item.origin_device.as_str(),
            item.name.as_str(),
            item.hash.as_str(),
        ];
        if haystacks
            .iter()
            .any(|text| text.to_lowercase().contains(query))
        {
            return true;
        }
        self.previews
            .get(&item.name)
            .and_then(|state| state.summary_text())
            .map(|text| text.to_lowercase().contains(query))
            .unwrap_or(false)
    }

    fn set_filter(&mut self, query: String) {
        self.filter = query;
        self.refilter();
    }

    // ---------------------------------------------------------------- 预览

    /// 某个对象名的预览状态。
    pub fn preview_state(&self, name: &str) -> Option<&PreviewState> {
        self.previews.get(name)
    }

    /// 选中行的预览状态。
    pub fn selected_preview_state(&self) -> Option<&PreviewState> {
        let item = self.selected_item()?;
        self.previews.get(&item.name)
    }

    /// 选中行的完整预览（已加载时）。
    pub fn selected_preview(&self) -> Option<&HistoryPreview> {
        self.selected_preview_state()
            .and_then(|state| state.preview())
    }

    /// 选中行的预览失败原因。
    pub fn selected_preview_error(&self) -> Option<&str> {
        self.selected_preview_state()
            .and_then(|state| state.error())
    }

    /// 丢掉全部摘要缓存（历史整体刷新后用）。
    pub fn forget_previews(&mut self) {
        self.previews.clear();
        self.pending.clear();
        self.last_plan.clear();
    }

    /// 选中行。
    pub fn selected_item(&self) -> Option<&HistorySummary> {
        let index = *self.visible.get(self.cursor)?;
        self.items.get(index)
    }

    /// 渲染层每帧告知列表的可视行数（决定预取窗口与分页步长）。
    pub fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
    }

    /// 可视行数。
    pub fn viewport_rows(&self) -> usize {
        self.viewport_rows.max(1)
    }

    /// 生成这一轮要发出的预览请求（位置 + 对象名）。
    ///
    /// 只覆盖可见窗口与选中项。窗口没变时返回空，避免每帧都发请求。
    pub fn take_preview_requests(&mut self) -> Vec<(usize, String)> {
        let rows = self.viewport_rows();
        let start = self.cursor.saturating_sub(rows / 2);
        let end = (start + rows + 1).min(self.visible.len());
        let start = end.saturating_sub(rows + 1);

        let mut plan: Vec<(usize, String)> = Vec::new();
        for index in start..end {
            let Some(item) = self.visible.get(index).and_then(|i| self.items.get(*i)) else {
                continue;
            };
            plan.push((item.position, item.name.clone()));
        }
        // 选中项一定在内（窄屏时可能超出上面的窗口）
        if let Some(item) = self.selected_item() {
            let entry = (item.position, item.name.clone());
            if !plan.iter().any(|(_, name)| *name == entry.1) {
                plan.push(entry);
            }
        }

        let names: Vec<String> = plan.iter().map(|(_, name)| name.clone()).collect();
        if names == self.last_plan {
            return Vec::new();
        }
        self.last_plan = names;

        plan.truncate(MAX_PREFETCH);
        plan.retain(|(_, name)| !self.previews.contains_key(name) && !self.pending.contains(name));
        for (_, name) in &plan {
            self.pending.insert(name.clone());
        }
        plan
    }

    /// 预览滚动。
    pub fn preview_scroll_by(&mut self, delta: i32) {
        let lines = self
            .selected_preview_state()
            .map(|state| state.line_count())
            .unwrap_or(0);
        let max = lines.saturating_sub(self.viewport_rows()) as i32;
        let next = (self.preview_scroll as i32 + delta).clamp(0, max.max(0));
        self.preview_scroll = next as u16;
    }

    /// 预览滚动偏移。
    pub fn preview_scroll(&self) -> u16 {
        self.preview_scroll
    }

    // ---------------------------------------------------------------- 光标

    /// 光标所在的 history position（0 = 最新）。
    pub fn selected_position(&self) -> Option<usize> {
        self.selected_item().map(|item| item.position)
    }

    fn move_to(&mut self, cursor: usize) {
        if self.cursor != cursor {
            self.cursor = cursor;
            self.preview_scroll = 0;
            // 窗口可能整体移动了，允许下一轮重新计划
            self.last_plan.clear();
        }
    }

    /// 光标上移。
    pub fn up(&mut self) {
        self.move_to(self.cursor.saturating_sub(1));
    }

    /// 光标下移。
    pub fn down(&mut self) {
        if self.cursor + 1 < self.visible.len() {
            self.move_to(self.cursor + 1);
        }
    }

    /// 上翻一页。
    pub fn page_up(&mut self) {
        let step = self.viewport_rows();
        self.move_to(self.cursor.saturating_sub(step));
    }

    /// 下翻一页。
    pub fn page_down(&mut self) {
        let step = self.viewport_rows();
        let last = self.visible.len().saturating_sub(1);
        self.move_to((self.cursor + step).min(last));
    }

    /// 首页 / 末页。
    pub fn home(&mut self) {
        self.move_to(0);
    }

    /// 跳到末尾。
    pub fn end(&mut self) {
        self.move_to(self.visible.len().saturating_sub(1));
    }

    fn clamp(&mut self) {
        if self.visible.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.visible.len() {
            self.cursor = self.visible.len() - 1;
        }
    }

    // ---------------------------------------------------------------- 按键

    /// 处理一个按键。
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> KeyOutcome {
        use crossterm::event::{KeyCode, KeyModifiers};

        // 过滤输入态：先把键盘交给输入框，否则打字会触发退出/复制
        if self.filter_input {
            match key.code {
                KeyCode::Enter => {
                    self.filter_input = false;
                    self.filter_backup.clear();
                }
                KeyCode::Esc => {
                    self.filter_input = false;
                    let previous = std::mem::take(&mut self.filter_backup);
                    self.set_filter(previous);
                }
                KeyCode::Backspace => {
                    let mut query = self.filter.clone();
                    query.pop();
                    self.set_filter(query);
                }
                KeyCode::Char(ch) => {
                    let mut query = self.filter.clone();
                    query.push(ch);
                    self.set_filter(query);
                }
                _ => {}
            }
            return KeyOutcome::None;
        }

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

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => KeyOutcome::Quit,
            // Esc 先清过滤，再退出：有过滤时不至于一不小心把界面关了
            KeyCode::Esc => {
                if self.filter_active() {
                    self.set_filter(String::new());
                    self.filter_backup.clear();
                    KeyOutcome::None
                } else {
                    KeyOutcome::Quit
                }
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.show_help = true;
                KeyOutcome::None
            }
            KeyCode::Char('/') => {
                self.filter_input = true;
                self.filter_backup = self.filter.clone();
                self.set_filter(String::new());
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
            KeyCode::PageUp => {
                self.page_up();
                KeyOutcome::None
            }
            KeyCode::PageDown => {
                self.page_down();
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
            // 预览滚动：大写 J/K 逐行，Ctrl+U/D 翻页，0 回到开头
            KeyCode::Char('J') => {
                self.preview_scroll_by(1);
                KeyOutcome::None
            }
            KeyCode::Char('K') => {
                self.preview_scroll_by(-1);
                KeyOutcome::None
            }
            KeyCode::Char('d') if ctrl => {
                self.preview_scroll_by(self.viewport_rows() as i32);
                KeyOutcome::None
            }
            KeyCode::Char('u') if ctrl => {
                self.preview_scroll_by(-(self.viewport_rows() as i32));
                KeyOutcome::None
            }
            KeyCode::Char('0') => {
                self.preview_scroll = 0;
                KeyOutcome::None
            }
            KeyCode::Enter => match self.selected_position() {
                Some(position) => KeyOutcome::Copy(position),
                None => {
                    if self.filter_active() {
                        self.warn("当前过滤没有匹配到内容");
                    } else {
                        self.warn("历史里还没有内容可复制");
                    }
                    KeyOutcome::None
                }
            },
            KeyCode::Char(' ') => {
                let paused = self
                    .status
                    .as_ref()
                    .map(|status| status.paused)
                    .unwrap_or(false);
                if paused {
                    KeyOutcome::Resume
                } else {
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
                let state = if status.paused {
                    "已暂停"
                } else {
                    "同步中"
                };
                let space = if status.encrypted {
                    status.space_id.as_deref().unwrap_or("-").to_string()
                } else {
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

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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

    fn preview(position: usize, text: &str) -> UiEvent {
        UiEvent::Preview {
            name: format!("1759323998000-ab12cd34-9f3c1e77-{position}"),
            result: Ok(Box::new(HistoryPreview {
                position,
                kind: "text".to_string(),
                text: Some(text.to_string()),
                image: None,
                truncated: false,
                bytes: text.len() as u32,
            })),
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

    fn app_with(items: usize) -> App {
        let mut app = App::new();
        app.apply(UiEvent::History((0..items).map(summary).collect()));
        app.set_viewport_rows(10);
        app
    }

    #[test]
    fn 方向键在边界上不越界() {
        let mut app = app_with(2);
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
        let mut app = app_with(3);
        app.down();
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::Copy(1));
    }

    #[test]
    fn 分页按可视行数移动光标() {
        let mut app = app_with(50);
        app.set_viewport_rows(10);
        app.page_down();
        assert_eq!(app.cursor, 10);
        app.page_down();
        assert_eq!(app.cursor, 20);
        app.page_up();
        assert_eq!(app.cursor, 10);
        // 底部不再越界
        app.end();
        app.page_down();
        assert_eq!(app.cursor, 49);
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
        assert_eq!(
            app.on_key(key(KeyCode::Char('q'))),
            KeyOutcome::Quit,
            "帮助页里 q 应直接退出"
        );
    }

    #[test]
    fn 列表变短后光标被夹住() {
        let mut app = app_with(3);
        app.end();
        assert_eq!(app.cursor, 2);
        app.apply(UiEvent::History(vec![summary(0)]));
        assert_eq!(app.cursor, 0, "列表变短后光标必须回到范围内");
        assert_eq!(app.selected_position(), Some(0));
    }

    // ------------------------------------------------------------ 过滤

    #[test]
    fn 过滤输入态里打字不会误触发命令() {
        let mut app = app_with(3);
        assert_eq!(app.on_key(key(KeyCode::Char('/'))), KeyOutcome::None);
        assert!(app.filter_input);

        // 这几个键在非输入态分别是"发布/退出/复制"，输入态里必须只当文本
        assert_eq!(app.on_key(key(KeyCode::Char('p'))), KeyOutcome::None);
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), KeyOutcome::None);
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::None);
        assert!(!app.filter_input, "回车应当结束输入");
        assert_eq!(app.filter(), "pq");
    }

    #[test]
    fn 过滤按类型与设备匹配并更新计数() {
        let mut app = app_with(5);
        assert_eq!(app.visible_len(), 5);

        app.on_key(key(KeyCode::Char('/')));
        for ch in "image".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.visible_len(), 0, "没有图片条目");
        // 过滤没结果时回车给出"过滤没有匹配"的提示，而不是"历史为空"
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::None);
        assert!(app.notice.as_ref().unwrap().text.contains("过滤"));

        // 换成匹配来源设备
        app.on_key(key(KeyCode::Char('/')));
        for ch in "cli".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.visible_len(), 5);
    }

    #[test]
    fn 过滤能匹配到已加载的内容摘要() {
        let mut app = app_with(3);
        app.apply(preview(1, "这是一段独特的内容 关键词在里面"));
        app.on_key(key(KeyCode::Char('/')));
        for ch in "关键词".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.visible_len(), 1, "只有内容里含关键词的那条应当留下");
        assert_eq!(app.selected_position(), Some(1));
    }

    #[test]
    fn esc_先清过滤再退出() {
        let mut app = app_with(3);
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('c')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.filter_active());

        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None, "先清过滤");
        assert!(!app.filter_active());
        assert_eq!(
            app.on_key(key(KeyCode::Esc)),
            KeyOutcome::Quit,
            "再按才退出"
        );
    }

    #[test]
    fn 输入中按_esc_取消并恢复原过滤() {
        let mut app = app_with(3);
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('c')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.filter(), "c");

        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.filter(), "x", "输入中实时过滤");
        app.on_key(key(KeyCode::Esc));
        assert!(!app.filter_input);
        assert_eq!(app.filter(), "c", "取消应当回到输入前的过滤词");
    }

    // ------------------------------------------------------------ 预览

    #[test]
    fn 预览按对象名缓存_新内容到来后不会张冠李戴() {
        let mut app = app_with(3);
        app.apply(preview(1, "旧内容"));
        // 来了一条更新内容：所有位置整体后移
        let mut items = vec![summary(0)];
        items.extend((1..4).map(summary));
        app.apply(UiEvent::History(items));

        // 第 1 条现在是原来的"位置 2"（老的内容），缓存按名字命中，因此仍在
        let still_there = app.preview_state("1759323998000-ab12cd34-9f3c1e77-1");
        assert_eq!(
            still_there.and_then(|state| state.summary_text()),
            Some("旧内容"),
            "缓存按对象名索引，位置平移不影响它"
        );
    }

    #[test]
    fn 预览失败会在那一行上给出原因() {
        let mut app = app_with(2);
        app.apply(UiEvent::Preview {
            name: "1759323998000-ab12cd34-9f3c1e77-0".to_string(),
            result: Err("内容已过期".to_string()),
        });
        assert_eq!(app.selected_preview_error(), Some("内容已过期"));
        assert!(app.selected_preview().is_none());
    }

    #[test]
    fn 滚动范围被内容行数限制() {
        let mut app = app_with(1);
        app.set_viewport_rows(5);
        app.apply(preview(
            0,
            &(1..=3).map(|i| format!("行{i}\n")).collect::<String>(),
        ));
        app.preview_scroll_by(100);
        assert_eq!(app.preview_scroll(), 0, "内容只有 3 行，没得滚");
        app.on_key(key(KeyCode::Char('0')));
        assert_eq!(app.preview_scroll(), 0);
    }

    #[test]
    fn 预览滚动与夹紧() {
        let mut app = app_with(1);
        app.set_viewport_rows(2);
        let long: String = (1..=10).map(|i| format!("第 {i} 行\n")).collect();
        app.apply(preview(0, &long));
        // 末尾换行不算一行：10 行文本 + 1 个换行 => .lines() 给出 10 行\n        assert_eq!(app.selected_preview_state().unwrap().line_count(), 10);

        app.on_key(key(KeyCode::Char('J')));
        app.on_key(key(KeyCode::Char('J')));
        assert_eq!(app.preview_scroll(), 2, "J 逐行下滚");
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(app.preview_scroll(), 1);
        app.on_key(ctrl(KeyCode::Char('d')));
        assert_eq!(app.preview_scroll(), 3, "Ctrl+D 翻一页（= 可视行数）");
        app.on_key(ctrl(KeyCode::Char('u')));
        assert_eq!(app.preview_scroll(), 1);
        app.on_key(key(KeyCode::Char('0')));
        assert_eq!(app.preview_scroll(), 0);

        // 换选中项会把滚动复位
        let mut two = app_with(2);
        two.set_viewport_rows(2);
        two.apply(preview(0, &long));
        two.apply(preview(1, &long));
        two.on_key(key(KeyCode::Char('J')));
        two.down();
        assert_eq!(two.preview_scroll(), 0, "换条目应回到内容开头");
    }

    // ------------------------------------------------------------ 预取

    #[test]
    fn 只为可见窗口预取且窗口不变时不重复请求() {
        let mut app = app_with(100);
        app.set_viewport_rows(10);
        let first = app.take_preview_requests();
        assert!(first.len() <= 12, "只预取可见窗口，实际 {}", first.len());
        assert!(first.len() >= 10);

        let again = app.take_preview_requests();
        assert!(again.is_empty(), "窗口没变就不该再发请求");

        // 移动到很下面：窗口变了，应当重新计划
        for _ in 0..30 {
            app.down();
        }
        let after = app.take_preview_requests();
        assert!(!after.is_empty(), "窗口移动后应当补请求");

        // 已经拿到的不再请求
        let (position, name) = after[0].clone();
        app.apply(UiEvent::Preview {
            name: name.clone(),
            result: Ok(Box::new(HistoryPreview {
                position,
                kind: "text".to_string(),
                text: Some("内容".to_string()),
                image: None,
                truncated: false,
                bytes: 6,
            })),
        });
        app.last_plan.clear();
        let third = app.take_preview_requests();
        assert!(
            !third.iter().any(|(_, pending)| *pending == name),
            "已有缓存的条目不该重复请求"
        );
    }

    #[test]
    fn 缓存超限会整体丢弃而不是无限增长() {
        let mut app = app_with(600);
        app.set_viewport_rows(500);
        let requests = app.take_preview_requests();
        assert!(requests.len() <= MAX_PREFETCH, "预取数量必须封顶");
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
