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
use crate::wizard::{BackendDraft, Focus, Wizard};
use cx_proto::control::{DoctorReport, HistoryPreview, HistorySummary, StatusReport};
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
    /// 拉一次诊断（后端面板要用）。
    Doctor,
    /// 删除一条本机历史（位置 0 = 最新）。
    Delete(usize),
    /// 表单填完了：交给命令层去落盘与自测。
    AddBackend(Box<BackendDraft>),
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
    /// 列表内容区的顶部行与行数（渲染层每帧告知，鼠标点击要用它做坐标换算）。
    list_geometry: Option<(u16, u16)>,
    /// 列表当前滚动到第几行（渲染层每帧告知：ratatui 自己算的，只有渲染后才知道）。
    list_offset: usize,
    /// 预览滚动偏移（行）。
    preview_scroll: u16,
    /// 最近一条提示。
    pub notice: Option<Notice>,
    /// 是否显示帮助。
    pub show_help: bool,
    /// 是否显示"后端与连接"面板。
    pub show_panel: bool,
    /// 添加后端的表单（Some 表示正在填）。
    pub wizard: Option<Wizard>,
    /// 面板的滚动偏移（面板里有配置引导，小终端放不下）。
    panel_scroll: u16,
    /// 最近一次诊断报告（面板里的局域网对端等信息只有它知道）。
    pub doctor: Option<DoctorReport>,
    /// 等待确认的删除位置（删除是破坏性操作，必须先确认一次）。
    pub pending_delete: Option<usize>,
    /// 后台线程是否还在跑。
    pub running: bool,
    /// 是否已连上守护进程（断线时界面继续留在屏幕上重试，而不是退出）。
    pub connected: bool,
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
                self.connected = true;
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
            UiEvent::Doctor(report) => {
                self.doctor = Some(*report);
            }
            UiEvent::Disconnected => {
                // 不结束界面：让用户看得见发生了什么，连上了会自动恢复。
                // （早先这里直接退出，从快捷方式启动时表现为"黑窗一闪而过"。）
                self.connected = false;
                self.warn("未连上守护进程，正在自动重试…（q 退出）");
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

    /// 面板滚动偏移。
    pub fn panel_scroll(&self) -> u16 {
        self.panel_scroll
    }

    /// 可视行数。
    pub fn viewport_rows(&self) -> usize {
        self.viewport_rows.max(1)
    }

    /// 渲染层回填列表内容区的位置与滚动偏移。
    pub fn set_list_geometry(&mut self, top: u16, rows: u16, offset: usize) {
        self.list_geometry = Some((top, rows));
        self.list_offset = offset;
    }

    /// 点击某一行：命中列表就选中对应条目，返回它在 `visible` 里的下标。
    ///
    /// 坐标换算必须用**渲染时**记录的位置与偏移：列表是不是滚动、滚动到哪一行，
    /// 都是 ratatui 在渲染时为了"让选中项可见"自己算出来的，App 这边凭空推不出来。
    pub fn click(&mut self, row: u16) -> Option<usize> {
        let (top, rows) = self.list_geometry?;
        if rows == 0 || row < top || row >= top.saturating_add(rows) {
            return None;
        }
        let index = self.list_offset + (row - top) as usize;
        if index >= self.visible.len() {
            // 点在列表空白处：不动光标（比"随便选一个"更符合直觉）
            return None;
        }
        self.move_to(index);
        Some(index)
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

        // 表单态：键盘全部交给表单（否则打字会触发退出/复制等命令）
        if self.wizard.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let outcome = match key.code {
                KeyCode::Esc => {
                    self.wizard = None;
                    self.note("已取消，未做任何改动");
                    KeyOutcome::None
                }
                KeyCode::Tab | KeyCode::Down => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.focus_next();
                    }
                    KeyOutcome::None
                }
                KeyCode::BackTab | KeyCode::Up => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.focus_prev();
                    }
                    KeyOutcome::None
                }
                KeyCode::Left => {
                    if let Some(form) = self.wizard.as_mut() {
                        if form.focused_field().is_some() {
                            form.backspace();
                        } else {
                            form.kind_left();
                        }
                    }
                    KeyOutcome::None
                }
                KeyCode::Right => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.kind_right();
                    }
                    KeyOutcome::None
                }
                KeyCode::Backspace => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.backspace();
                    }
                    KeyOutcome::None
                }
                KeyCode::Char('y') if ctrl => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.fill_sample();
                    }
                    KeyOutcome::None
                }
                KeyCode::Char('l') if ctrl => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.clear_field();
                    }
                    KeyOutcome::None
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.push_char(ch);
                    }
                    KeyOutcome::None
                }
                // 回车：在 [保存] 上或任意字段上提交
                KeyCode::Enter => {
                    let submit = self
                        .wizard
                        .as_mut()
                        .map(|form| form.submit())
                        .unwrap_or(Err(Vec::new()));
                    match submit {
                        Ok(draft) => {
                            self.wizard = None;
                            KeyOutcome::AddBackend(Box::new(draft))
                        }
                        Err(_) => KeyOutcome::None,
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(form) = self.wizard.as_mut() {
                        match form.focus() {
                            Focus::Save => {
                                let submit = form.submit();
                                if let Ok(draft) = submit {
                                    self.wizard = None;
                                    return KeyOutcome::AddBackend(Box::new(draft));
                                }
                            }
                            Focus::Cancel => {
                                self.wizard = None;
                                self.note("已取消，未做任何改动");
                            }
                            _ => form.push_char(' '),
                        }
                    }
                    KeyOutcome::None
                }
                _ => KeyOutcome::None,
            };
            return outcome;
        }

        // 删除确认：只有 y/回车 才真的删，其他任何键都取消
        if let Some(position) = self.pending_delete {
            self.pending_delete = None;
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    KeyOutcome::Delete(position)
                }
                _ => {
                    self.note("已取消删除");
                    KeyOutcome::None
                }
            };
        }

        // 面板与帮助页都是"看一眼就关"的浮层：任意键关闭（q 退出）
        if self.show_panel {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            return match key.code {
                KeyCode::Char('q') => KeyOutcome::Quit,
                // 滚动键不关闭面板：引导内容在小终端上放不下，得能翻
                KeyCode::Char('J') => {
                    self.panel_scroll = self.panel_scroll.saturating_add(1);
                    KeyOutcome::None
                }
                KeyCode::Char('K') => {
                    self.panel_scroll = self.panel_scroll.saturating_sub(1);
                    KeyOutcome::None
                }
                KeyCode::PageDown => {
                    self.panel_scroll = self.panel_scroll.saturating_add(10);
                    KeyOutcome::None
                }
                KeyCode::PageUp => {
                    self.panel_scroll = self.panel_scroll.saturating_sub(10);
                    KeyOutcome::None
                }
                KeyCode::Char('d') if ctrl => {
                    self.panel_scroll = self.panel_scroll.saturating_add(10);
                    KeyOutcome::None
                }
                KeyCode::Char('u') if ctrl => {
                    self.panel_scroll = self.panel_scroll.saturating_sub(10);
                    KeyOutcome::None
                }
                KeyCode::Char('0') => {
                    self.panel_scroll = 0;
                    KeyOutcome::None
                }
                // b 是开关：再按一次就关掉（不能变成"先被任意键关掉、再按又打开"）
                KeyCode::Char('b') | KeyCode::Esc => {
                    self.show_panel = false;
                    KeyOutcome::None
                }
                _ => {
                    self.show_panel = false;
                    KeyOutcome::None
                }
            };
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
            KeyCode::Char('a') => {
                self.wizard = Some(Wizard::new());
                KeyOutcome::None
            }
            KeyCode::Char('b') => {
                self.show_panel = true;
                self.panel_scroll = 0;
                // 打开时拉一次诊断：后端面板要显示局域网对端等只有 doctor 才知道的信息
                KeyOutcome::Doctor
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
            // 删除要先确认：这是唯一会真的丢内容的操作
            KeyCode::Char('d') => match self.selected_position() {
                Some(position) => {
                    self.pending_delete = Some(position);
                    KeyOutcome::None
                }
                None => {
                    self.warn("没有可删除的条目");
                    KeyOutcome::None
                }
            },
            KeyCode::Char('r') => KeyOutcome::Refresh,
            KeyCode::Char('p') => KeyOutcome::Push,
            KeyCode::Char('R') => KeyOutcome::Reload,
            _ => KeyOutcome::None,
        }
    }

    /// 后端健康统计：`(正常, 有失败, 已禁用)`。
    pub fn backend_health(&self) -> (usize, usize, usize) {
        let Some(status) = &self.status else {
            return (0, 0, 0);
        };
        let mut healthy = 0;
        let mut failing = 0;
        let mut disabled = 0;
        for backend in &status.backends {
            if !backend.enabled {
                disabled += 1;
            } else if backend.failures > 0 || backend.last_error.is_some() {
                failing += 1;
            } else {
                healthy += 1;
            }
        }
        (healthy, failing, disabled)
    }

    /// 头部第二行要显示的健康提示（没什么可说时返回 None，别占地方）。
    ///
    /// 空后端是最容易让人困惑的状态——"我明明开着，为什么不同步"，
    /// 所以这里给出可执行的下一步，而不是只报个 0。
    pub fn health_line(&self) -> Option<(String, bool)> {
        let status = self.status.as_ref()?;
        let (healthy, failing, disabled) = self.backend_health();
        if status.backends.is_empty() {
            return Some((
                "还没有配置同步后端：内容只留在本机。按 b 看配置引导（三种方式、凭据怎么传）"
                    .to_string(),
                true,
            ));
        }
        if failing > 0 {
            return Some((
                format!(
                    "后端 {} 个：{} 个正在失败（按 b 看错误与退避时间）",
                    status.backends.len(),
                    failing
                ),
                true,
            ));
        }
        let latest = status
            .backends
            .iter()
            .filter_map(|backend| backend.last_ok_ms)
            .max();
        let tail = match latest {
            Some(ms) => format!("最近成功 {}", crate::ui::relative_time(ms)),
            None => "还没有成功过".to_string(),
        };
        let extra = if disabled > 0 {
            format!("，{disabled} 个已禁用")
        } else {
            String::new()
        };
        Some((
            format!("后端 {healthy} 个正常{extra}｜{tail}（按 b 看详情）"),
            false,
        ))
    }

    /// 鼠标滚轮：按行移动光标（滚轮是最自然的翻列表方式）。
    pub fn on_scroll(&mut self, delta: i32) {
        if self.show_help || self.show_panel {
            return;
        }
        match delta.cmp(&0) {
            std::cmp::Ordering::Less => {
                for _ in delta..0 {
                    self.up();
                }
            }
            std::cmp::Ordering::Greater => {
                for _ in 0..delta {
                    self.down();
                }
            }
            std::cmp::Ordering::Equal => {}
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
    fn 连接断开不会结束界面而是留在屏幕上重试() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        assert!(app.connected);
        app.apply(UiEvent::Disconnected);
        assert!(
            app.running,
            "断线不该结束界面：从快捷方式启动时守护进程可能还没起来，退出会让窗口一闪而过"
        );
        assert!(!app.connected);
        let notice = app.notice.as_ref().unwrap();
        assert!(notice.is_error);
        assert!(
            notice.text.contains("重试"),
            "要告诉用户在重试：{}",
            notice.text
        );
    }

    /// 带后端的诊断状态：用 JSON 构造，字段增删时测试不会碎。
    fn status_with_backends(paused: bool, backends: serde_json::Value) -> StatusReport {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "pid": 42,
            "started_ms": 1_759_323_000_000u64,
            "paused": paused,
            "device_id": "ab12cd34ef567890",
            "device_name": "笔记本",
            "space_id": "space-1",
            "encrypted": true,
            "backends": backends,
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

    fn backend(name: &str, enabled: bool, failures: u32, error: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "kind": "server",
            "enabled": enabled,
            "last_ok_ms": cx_core::now_ms() - 60_000,
            "last_error": error,
            "failures": failures,
            "next_attempt_ms": cx_core::now_ms() + 30_000,
            "published": 3,
            "applied": 2
        })
    }

    #[test]
    fn 空后端给出可执行的下一步而不是只报零() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        let (text, warn) = app.health_line().expect("空后端应当有提示");
        assert!(warn, "这是需要用户处理的提示");
        // 提示要指向界面内的引导（面板里有分步说明与凭据怎么传），
        // 而不是甩一条命令行——早先这里断言的是 "cx backend add"
        assert!(text.contains("按 b"), "要指向界面内的配置引导：{text}");
        assert!(text.contains("凭据"), "要提到凭据这件事：{text}");
        assert_eq!(app.backend_health(), (0, 0, 0));
    }

    #[test]
    fn 有后端在失败时提示条数与去看详情() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([
                backend("home", true, 0, None),
                backend("oss", true, 4, Some("连接超时")),
            ]),
        ))));
        assert_eq!(app.backend_health(), (1, 1, 0));
        let (text, warn) = app.health_line().unwrap();
        assert!(warn);
        assert!(text.contains("1 个正在失败"), "{text}");
        assert!(text.contains("按 b"), "{text}");
    }

    #[test]
    fn 全部正常时给出最近成功时间且不算警告() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([
                backend("home", true, 0, None),
                backend("dav", false, 0, None)
            ]),
        ))));
        assert_eq!(app.backend_health(), (1, 0, 1));
        let (text, warn) = app.health_line().unwrap();
        assert!(!warn, "正常状态不该是警告色：{text}");
        assert!(text.contains("1 个正常"), "{text}");
        assert!(text.contains("1 个已禁用"), "{text}");
        assert!(text.contains("最近成功"), "{text}");
    }

    #[test]
    fn 未连上时没有健康行() {
        let app = App::new();
        assert!(app.health_line().is_none(), "状态本身已经说了未连接");
    }

    #[test]
    fn b_键打开面板并拉诊断() {
        let mut app = app_with(2);
        assert_eq!(app.on_key(key(KeyCode::Char('b'))), KeyOutcome::Doctor);
        assert!(app.show_panel);
        // 面板里任意键关闭；q 直接退出（与帮助页一致，别把人困住）
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), KeyOutcome::None);
        assert!(!app.show_panel);
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), KeyOutcome::Quit);
    }

    #[test]
    fn 面板打开时按键不会误触列表操作() {
        let mut app = app_with(3);
        assert_eq!(app.on_key(key(KeyCode::Char('b'))), KeyOutcome::Doctor);
        assert!(app.show_panel);
        // 面板开着时 j 不该移动列表光标
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 0, "面板打开时 j 不该移动列表光标");
        assert!(!app.show_panel, "任意键应当关掉面板");

        // b 是开关：再按一次直接关闭（而不是"被任意键关掉后又被它打开"）
        app.on_key(key(KeyCode::Char('b')));
        assert!(app.show_panel);
        app.on_key(key(KeyCode::Char('b')));
        assert!(!app.show_panel);

        // 关掉之后 j 恢复正常
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 1, "关掉面板后 j 恢复正常");
    }

    #[test]
    fn 滚轮按行移动且浮层打开时不动() {
        let mut app = app_with(10);
        app.on_scroll(3);
        assert_eq!(app.cursor, 3);
        app.on_scroll(-2);
        assert_eq!(app.cursor, 1);
        // 到顶后再往上滚不越界
        app.on_scroll(-10);
        assert_eq!(app.cursor, 0);
        // 浮层打开时滚轮不该改列表
        app.on_key(key(KeyCode::Char('?')));
        app.on_scroll(5);
        assert_eq!(app.cursor, 0);
        app.on_key(key(KeyCode::Char('?')));
        app.on_key(key(KeyCode::Char('b')));
        app.on_scroll(5);
        assert_eq!(app.cursor, 0, "面板打开时滚轮也不该动列表");
    }

    #[test]
    fn 诊断报告到达后面板能读到局域网信息() {
        let mut app = App::new();
        app.apply(UiEvent::Doctor(Box::new(
            serde_json::from_value(serde_json::json!({
                "checks": [
                    {"name": "局域网", "ok": true, "detail": "已启用（端口 8699），ALEXLAPTOP（a5af2a1b）"},
                    {"name": "密钥权限", "ok": true, "detail": "1 个密钥文件都没有组/其他权限"}
                ]
            }))
            .expect("诊断 JSON 应当能反序列化"),
        )));
        let doctor = app.doctor.as_ref().expect("应当存下诊断报告");
        assert!(doctor.checks.iter().any(|check| check.name == "局域网"));
    }

    #[test]
    fn 删除必须确认_其他键一律取消() {
        let mut app = app_with(3);
        app.down(); // 选中位置 1
        assert_eq!(app.on_key(key(KeyCode::Char('d'))), KeyOutcome::None);
        assert_eq!(app.pending_delete, Some(1), "d 只是进入确认，不该立刻删");

        // 取消：任何非 y/回车 的键
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), KeyOutcome::None);
        assert!(app.pending_delete.is_none());
        assert!(!app.notice.as_ref().unwrap().is_error);

        // 确认
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), KeyOutcome::Delete(1));
        assert!(app.pending_delete.is_none(), "确认后应当清掉待确认状态");
        // 回车也算确认
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::Delete(1));
    }

    #[test]
    fn 确认浮层打开时按键不会触发别的命令() {
        let mut app = app_with(3);
        // 这些键在平时分别是"发布/暂停/退出"，确认态里只能算取消。
        // 每次都要重新打开确认——第一次按键已经把它关掉了（这正是上一版测试写错的地方）。
        for ch in ['p', ' ', 'q'] {
            assert_eq!(app.on_key(key(KeyCode::Char('d'))), KeyOutcome::None);
            assert!(app.pending_delete.is_some(), "d 应当打开确认");
            assert_eq!(
                app.on_key(key(KeyCode::Char(ch))),
                KeyOutcome::None,
                "{ch} 在确认态里只应当取消，不该触发对应命令"
            );
            assert!(app.pending_delete.is_none());
        }
    }

    #[test]
    fn 没有条目时按_d_给提示() {
        let mut app = App::new();
        assert_eq!(app.on_key(key(KeyCode::Char('d'))), KeyOutcome::None);
        assert!(app.pending_delete.is_none());
        assert!(app.notice.as_ref().unwrap().is_error);
        assert!(app.notice.as_ref().unwrap().text.contains("没有可删除"));
    }
    #[test]
    fn 点击列表行会选中对应条目() {
        let mut app = app_with(10);
        // 渲染层会告诉我们：内容区从第 3 行开始、高 5 行、当前没滚动
        app.set_list_geometry(3, 5, 0);
        assert_eq!(app.click(3), Some(0), "第一行 = 第一条");
        assert_eq!(app.cursor, 0);
        assert_eq!(app.click(6), Some(3), "第 4 行 = 第 4 条");
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn 点击要算上滚动偏移() {
        let mut app = app_with(50);
        app.set_list_geometry(3, 5, 20); // 列表已经滚到第 20 条
        assert_eq!(app.click(3), Some(20), "屏幕上第一行是第 21 条");
        assert_eq!(app.click(7), Some(24));
    }

    #[test]
    fn 点在列表之外的空白处不动光标() {
        let mut app = app_with(3);
        app.set_list_geometry(3, 5, 0);
        app.down();
        let selected = app.cursor;

        // 头部 / 列表边框外 / 页脚
        assert_eq!(app.click(0), None);
        assert_eq!(app.click(2), None);
        assert_eq!(app.click(20), None);
        assert_eq!(app.cursor, selected, "这些点击不该改变选中项");

        // 列表内部但超出条目数的空白行
        app.set_list_geometry(3, 5, 0);
        assert_eq!(app.click(6), None, "只有 3 条，第 4 行是空白");
        assert_eq!(app.cursor, selected);
    }

    #[test]
    fn 还没渲染过时点击不越界() {
        let mut app = app_with(3);
        assert_eq!(app.click(5), None, "没有几何信息时应当安全返回");
    }

    #[test]
    fn 过滤之后点击按可见顺序映射() {
        let mut app = app_with(6);
        // 让其中一条的内容里带上独特关键词，过滤后只剩它
        app.apply(preview(4, "独特关键词 在这里"));
        app.on_key(key(KeyCode::Char('/')));
        for ch in "独特关键词".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.visible_len(), 1, "过滤后只剩一条");

        app.set_list_geometry(3, 4, 0);
        // 可见列表只有一项，点第一行命中它——位置是 4（不是 0）
        assert_eq!(app.click(3), Some(0), "返回的是 visible 下标");
        assert_eq!(
            app.selected_position(),
            Some(4),
            "选中的是那条被过滤出来的内容"
        );
        // 第二行是空的
        assert_eq!(app.click(4), None);
    }
    #[test]
    fn 面板里滚动键不关闭面板() {
        let mut app = app_with(1);
        app.on_key(key(KeyCode::Char('b')));
        assert!(app.show_panel);
        assert_eq!(app.panel_scroll(), 0);

        app.on_key(key(KeyCode::Char('J')));
        app.on_key(key(KeyCode::Char('J')));
        assert!(app.show_panel, "J 是滚面板，不该关掉它");
        assert_eq!(app.panel_scroll(), 2);
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(app.panel_scroll(), 1);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.panel_scroll(), 11);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.panel_scroll(), 1);
        app.on_key(ctrl(KeyCode::Char('d')));
        assert_eq!(app.panel_scroll(), 11);
        app.on_key(ctrl(KeyCode::Char('u')));
        assert_eq!(app.panel_scroll(), 1);
        app.on_key(key(KeyCode::Char('0')));
        assert_eq!(app.panel_scroll(), 0);
        // 到顶后继续上滚不越界
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(app.panel_scroll(), 0);

        // 仍然是"b / Esc 关闭，其他键关闭"
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.show_panel);
    }

    #[test]
    fn 每次打开面板滚动都从顶部开始() {
        let mut app = app_with(1);
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Char('J')));
        app.on_key(key(KeyCode::Char('J')));
        assert_eq!(app.panel_scroll(), 2);
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.panel_scroll(), 0, "重新打开不该停在上一半截");
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
