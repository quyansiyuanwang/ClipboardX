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
use ratatui::layout::Rect;
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
    /// 移除某个后端（面板里确认后）。
    RemoveBackend(String),
    /// 停用 / 启用某个后端（面板里按 t）。
    ToggleBackend(String),
    /// 在本机建加密空间并把元数据发布到该后端（面板里按 s）。
    /// `force` 为真表示"已有空间也重建"（命令行 `cx space init --force` 的等价物）。
    CreateSpace {
        /// 后端名。
        backend: String,
        /// 是否强制重建。
        force: bool,
    },
    /// 导出空间密钥（`cx space export-key` 的等价物）。
    ExportKey,
    /// 把空间密钥写到文件里（密钥页上按 x）。
    ExportKeyToFile,
    /// 打开某个后端的编辑表单（面板里按 E）。
    EditBackend(String),
    /// 提交对已有后端的修改。
    SaveBackend {
        /// 原后端名。
        original: String,
        /// 表单结果。
        draft: Box<crate::wizard::BackendDraft>,
    },
    /// 把空间元数据重新发布到某个后端（面板里按 P）。
    PublishSpace(String),
    /// 整套配置导出（面板里按 O）。
    ExportBundle,
    /// 整套配置导入（面板里按 I，已确认）。
    ImportBundle,
    /// 打开配置页（面板里按 C）。
    OpenConfig,
    /// 提交一个配置项修改。
    SetConfig {
        /// 键名。
        key: String,
        /// 新值。
        value: String,
    },
    /// 重启守护进程（面板里按 R，已确认）。
    RestartDaemon,
    /// 用空间密钥加入已有空间（面板里按 j 并输入密钥）。
    JoinSpace {
        /// 后端名。
        backend: String,
        /// 64 位十六进制空间密钥。
        key: String,
    },
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

/// 配置页的状态：列表 + 光标 + 行内编辑缓冲。
#[derive(Debug, Default, Clone)]
pub struct ConfigPage {
    /// `(键名, 当前值)`，顺序与 `config_cmd::keys()` 一致。
    pub items: Vec<(String, String)>,
    /// 光标所在行。
    pub cursor: usize,
    /// 正在编辑的输入缓冲（`None` 表示没在编辑）。
    pub editing: Option<String>,
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
    /// 二维命中区域只由实际渲染回填。
    pub(crate) list_rect: Rect,
    pub(crate) preview_rect: Rect,
    pub(crate) modal_rect: Rect,
    pub(crate) wizard_hits: Vec<(Rect, Focus)>,
    pub(crate) wizard_kinds: Vec<(Rect, usize)>,
    preview_rows: usize,
    preview_lines: usize,
    preview_width: u16,
    pub(crate) help_scroll: u16,
    pub(crate) help_max_scroll: u16,
    pub(crate) panel_max_scroll: u16,
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
    /// 面板里选中的后端下标（用于移除）。
    panel_backend: usize,
    /// 等待确认移除的后端名。
    pub pending_backend_remove: Option<String>,
    /// 正在"用密钥加入空间"的输入缓冲：`(后端名, 已输入的密钥文本)`。
    pub join_space: Option<(String, String)>,
    /// 已有空间时按 s：等用户在确认页上点头，值是要重建到的后端。
    pub pending_space_rebuild: Option<String>,
    /// 已有空间时按 i：等用户确认"换空间"，值是目标后端。
    pub pending_space_switch: Option<String>,
    /// 导出出来的空间密钥（`e`）：单独一页展示，不在提示行里塞 64 个字符。
    pub exported_key: Option<String>,
    /// 正在"编辑已有后端"时的原后端名（提交时要按原名替换）。
    pub wizard_edit_target: Option<String>,
    /// 导入整包前的确认（会覆盖本机配置、凭据与空间密钥）。
    pub pending_import: bool,
    /// 配置页：`(键, 值)` 列表 + 选中行 + 正在编辑的输入缓冲。
    pub config_page: Option<ConfigPage>,
    /// 重启守护进程前的确认。
    pub pending_restart: bool,
    /// 密钥页上"导出到文件"的结果（成功路径或错误），显示在同一页里。
    pub exported_key_note: Option<String>,
    /// 最近一次诊断报告（面板里的局域网对端等信息只有它知道）。
    pub doctor: Option<DoctorReport>,
    /// 面板内独立错误，避免失败后永远显示读取中。
    pub doctor_error: Option<String>,
    /// 等待确认的删除位置（删除是破坏性操作，必须先确认一次）。
    pub pending_delete: Option<usize>,
    /// 确认弹层绑定的稳定对象名，不随位置漂移。
    pub pending_delete_name: Option<String>,
    /// 最近确认的删除对象；事件循环取走后发 expected_name 请求。
    pub delete_expected_name: Option<String>,
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
                // 后端数量会变（尤其刚在面板里移除/添加完）：选中下标必须夹回范围内，
                // 否则面板里既没有 ▶、空格也没反应，看起来像键盘坏了
                self.clamp_panel_selection();
            }
            UiEvent::History(items) => {
                let selected = self.selected_item().map(|item| item.name.clone());
                let scroll = self.preview_scroll;
                self.items = items;
                self.refilter();
                if let Some(name) = selected {
                    if let Some(cursor) = self
                        .visible
                        .iter()
                        .position(|index| self.items[*index].name == name)
                    {
                        self.cursor = cursor;
                        self.preview_scroll = scroll;
                    } else {
                        self.preview_scroll = 0;
                    }
                }
                self.last_plan.clear();
            }
            UiEvent::Preview { name, result } => {
                self.pending.remove(&name);
                self.last_plan.clear();
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
                if command == "doctor" {
                    self.doctor_error = Some(message.clone());
                }
                self.warn(format!("{command} 失败：{message}"));
            }
            UiEvent::Doctor(report) => {
                self.doctor_error = None;
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
        let selected = self.selected_item().map(|item| item.name.clone());
        let query = self.filter.to_lowercase();
        self.visible = if query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            (0..self.items.len())
                .filter(|index| self.matches(*index, &query))
                .collect()
        };
        self.clamp();
        if let Some(name) = selected {
            if let Some(cursor) = self
                .visible
                .iter()
                .position(|index| self.items[*index].name == name)
            {
                self.cursor = cursor;
            } else {
                self.preview_scroll = 0;
            }
        }
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

    /// 面板里列出的后端名（顺序与 status 一致）。
    pub fn backend_names(&self) -> Vec<String> {
        self.status
            .as_ref()
            .map(|status| {
                status
                    .backends
                    .iter()
                    .map(|backend| backend.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 面板里选中的后端名。
    pub fn selected_backend(&self) -> Option<String> {
        self.backend_names().get(self.panel_backend).cloned()
    }

    /// 面板选中的后端下标。
    pub fn panel_backend(&self) -> usize {
        self.panel_backend
    }

    /// 按名字选中面板里的某个后端（添加成功后把新行高亮出来）。
    pub fn select_backend(&mut self, name: &str) {
        if let Some(index) = self
            .backend_names()
            .iter()
            .position(|candidate| candidate == name)
        {
            self.panel_backend = index;
        }
    }

    /// 打开面板时把选中项夹回范围内（后端数量会变）。
    pub fn clamp_panel_selection(&mut self) {
        let count = self.backend_names().len();
        if count == 0 {
            self.panel_backend = 0;
        } else if self.panel_backend >= count {
            self.panel_backend = count - 1;
        }
    }

    /// 本机是否已经在某个加密空间里（很多"危险动作"都要先问这个）。
    pub fn has_space(&self) -> bool {
        self.status
            .as_ref()
            .map(|status| status.space_id.is_some())
            .unwrap_or(false)
    }

    pub(crate) fn clamp_panel_scroll(&mut self) {
        self.panel_scroll = self.panel_scroll.min(self.panel_max_scroll);
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
        if self.show_help
            || self.show_panel
            || self.wizard.is_some()
            || self.pending_delete.is_some()
            || self.pending_backend_remove.is_some()
        {
            return None;
        }
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
        let start = if self.list_geometry.is_some()
            && self.cursor >= self.list_offset
            && self.cursor < self.list_offset.saturating_add(rows)
        {
            self.list_offset
        } else {
            self.cursor.saturating_sub(rows / 2)
        };
        let end = (start + rows + 1).min(self.visible.len());
        let start = end.saturating_sub(rows + 1);

        let mut plan: Vec<(usize, String)> = Vec::new();
        for index in start..end {
            let Some(item) = self.visible.get(index).and_then(|i| self.items.get(*i)) else {
                continue;
            };
            plan.push((item.position, item.name.clone()));
        }
        // 选中项排第一，避免大终端的 MAX_PREFETCH 截断把当前预览丢掉。
        if let Some(item) = self.selected_item() {
            let entry = (item.position, item.name.clone());
            plan.retain(|(_, name)| *name != entry.1);
            plan.insert(0, entry);
        }

        let names: Vec<String> = plan.iter().map(|(_, name)| name.clone()).collect();
        if names == self.last_plan {
            return Vec::new();
        }
        self.last_plan = names;

        plan.retain(|(_, name)| !self.previews.contains_key(name) && !self.pending.contains(name));
        plan.truncate(MAX_PREFETCH);
        for (_, name) in &plan {
            self.pending.insert(name.clone());
        }
        plan
    }

    /// 渲染后的包装行数包含元数据；预览高度独立于历史列表。
    pub(crate) fn set_preview_geometry(&mut self, rect: Rect, lines: usize) {
        self.preview_rect = rect;
        if self.selected_item().is_some() && self.selected_preview_state().is_none() {
            // 手动刷新会临时清掉缓存；读取中的占位行不能把阅读位置夹回 0。
            return;
        }
        if self.preview_width > 0 && rect.width > 0 && self.preview_width != rect.width {
            let at_end = self.preview_scroll > 0
                && self.preview_scroll as usize
                    >= self.preview_lines.saturating_sub(self.preview_rows);
            self.preview_scroll = if at_end {
                lines
                    .saturating_sub(rect.height as usize)
                    .min(u16::MAX as usize) as u16
            } else {
                ((self.preview_scroll as usize * self.preview_width as usize) / rect.width as usize)
                    .min(u16::MAX as usize) as u16
            };
        }
        self.preview_width = rect.width;
        self.preview_rect = rect;
        self.preview_rows = rect.height as usize;
        self.preview_lines = lines;
        self.preview_scroll_by(0);
    }

    /// 预览滚动。
    pub fn preview_scroll_by(&mut self, delta: i32) {
        let (lines, rows) = if self.preview_width > 0 {
            (self.preview_lines, self.preview_rows)
        } else {
            (
                self.selected_preview_state()
                    .map(|state| state.line_count())
                    .unwrap_or(0),
                self.viewport_rows(),
            )
        };
        let max = lines.saturating_sub(rows).min(u16::MAX as usize) as i32;
        let next = (self.preview_scroll as i32 + delta).clamp(0, max);
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

    /// 处理括号粘贴。秘密按原值保存；普通单行字段去掉换行。
    pub fn on_paste(&mut self, text: &str) {
        if let Some(form) = self.wizard.as_mut() {
            if let Some(index) = form.focused_field() {
                let secret = form.fields()[index].secret;
                let cleaned;
                let value = if secret {
                    text
                } else {
                    cleaned = text.replace(['\r', '\n'], "");
                    cleaned.trim()
                };
                for ch in value.chars() {
                    form.push_char(ch);
                }
            }
            return;
        }
        // 加入空间的密钥输入：粘贴时只留十六进制、转小写，并且**允许一次贴满 64 位**
        // （密钥都是从另一台机器 `cx space export-key` 复制来的，手敲不现实）。
        if let Some((backend, typed)) = self.join_space.take() {
            let mut buffer = typed;
            for ch in text.chars() {
                if ch == '\r' || ch == '\n' || ch.is_whitespace() {
                    continue;
                }
                if ch.is_ascii_hexdigit() && buffer.chars().count() < 128 {
                    buffer.push(ch.to_ascii_lowercase());
                }
            }
            self.join_space = Some((backend, buffer));
            return;
        }
        if self.filter_input {
            let mut query = self.filter.clone();
            query.push_str(text.replace(['\r', '\n'], "").trim());
            self.set_filter(query);
        }
    }

    /// 关掉表单：**同时清掉编辑目标**。漏掉这一步会导致"取消编辑后再新增"被当成
    /// 保存修改，把之前那个后端覆盖掉（实测过的坑，别再拆开写）。
    fn close_wizard(&mut self) {
        self.wizard = None;
        self.wizard_edit_target = None;
    }

    /// 表单提交后该"新增"还是"保存修改"：由 `wizard_edit_target` 决定。
    pub(crate) fn backend_outcome(&mut self, draft: crate::wizard::BackendDraft) -> KeyOutcome {
        match self.wizard_edit_target.take() {
            Some(original) => KeyOutcome::SaveBackend {
                original,
                draft: Box::new(draft),
            },
            None => KeyOutcome::AddBackend(Box::new(draft)),
        }
    }

    fn activate_wizard(&mut self) -> KeyOutcome {
        if self
            .wizard
            .as_ref()
            .is_some_and(|form| form.focus() == Focus::Cancel)
        {
            self.close_wizard();
            self.note("已取消，未做任何改动");
            return KeyOutcome::None;
        }
        match self.wizard.as_mut().map(|form| form.submit()) {
            Some(Ok(draft)) => {
                self.wizard = None;
                self.backend_outcome(draft)
            }
            _ => KeyOutcome::None,
        }
    }
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

        // 重建空间的确认（已有空间时按 s）：只有 y/回车 才真的重建
        if let Some(backend) = self.pending_space_rebuild.take() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    KeyOutcome::CreateSpace {
                        backend,
                        force: true,
                    }
                }
                _ => {
                    self.note("已取消重建");
                    KeyOutcome::None
                }
            };
        }
        // 换空间的确认（已有空间时按 i）：确认后进密钥输入页
        if let Some(backend) = self.pending_space_switch.take() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.join_space = Some((backend, String::new()));
                    KeyOutcome::None
                }
                _ => {
                    self.note("已取消");
                    KeyOutcome::None
                }
            };
        }
        // 重启守护进程的确认：会短暂中断同步
        if self.pending_restart {
            self.pending_restart = false;
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    KeyOutcome::RestartDaemon
                }
                _ => {
                    self.note("已取消重启");
                    KeyOutcome::None
                }
            };
        }
        // 配置页：↑↓ 选键、回车编辑、Esc 返回；编辑中回车提交、Esc 放弃
        if self.config_page.is_some() {
            let page = self.config_page.as_mut().expect("配置页存在");
            if let Some(buffer) = page.editing.take() {
                let outcome = match key.code {
                    KeyCode::Esc => {
                        self.note("已放弃修改");
                        KeyOutcome::None
                    }
                    KeyCode::Backspace => {
                        let mut buffer = buffer;
                        buffer.pop();
                        self.config_page.as_mut().unwrap().editing = Some(buffer);
                        KeyOutcome::None
                    }
                    KeyCode::Enter => {
                        let key_name = page
                            .items
                            .get(page.cursor)
                            .map(|(key, _)| key.clone())
                            .unwrap_or_default();
                        KeyOutcome::SetConfig {
                            key: key_name,
                            value: buffer,
                        }
                    }
                    KeyCode::Char(ch) => {
                        let mut buffer = buffer;
                        buffer.push(ch);
                        self.config_page.as_mut().unwrap().editing = Some(buffer);
                        KeyOutcome::None
                    }
                    _ => {
                        self.config_page.as_mut().unwrap().editing = Some(buffer);
                        KeyOutcome::None
                    }
                };
                return outcome;
            }
            return match key.code {
                KeyCode::Esc => {
                    self.config_page = None;
                    KeyOutcome::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    page.cursor = page.cursor.saturating_sub(1);
                    KeyOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if page.cursor + 1 < page.items.len() {
                        page.cursor += 1;
                    }
                    KeyOutcome::None
                }
                KeyCode::Enter => {
                    // 进入行内编辑，初值是当前值（改端口这类场景直接覆盖即可）
                    let current = page
                        .items
                        .get(page.cursor)
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default();
                    page.editing = Some(current);
                    KeyOutcome::None
                }
                _ => KeyOutcome::None,
            };
        }

        // 导入整包的确认：会覆盖本机配置、凭据与空间密钥
        if self.pending_import {
            self.pending_import = false;
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    KeyOutcome::ImportBundle
                }
                _ => {
                    self.note("已取消导入");
                    KeyOutcome::None
                }
            };
        }
        // 空间密钥展示页：x 导出到文件（剪贴板不能用来搬密钥——它本身就会被同步），
        // 其他任意键返回。
        if self.exported_key.is_some() {
            return match key.code {
                KeyCode::Char('x') | KeyCode::Char('X') => KeyOutcome::ExportKeyToFile,
                _ => {
                    self.exported_key = None;
                    self.exported_key_note = None;
                    KeyOutcome::None
                }
            };
        }

        // 移除后端的确认：只有 y/回车 才真的移除
        if let Some(name) = self.pending_backend_remove.take() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    KeyOutcome::RemoveBackend(name)
                }
                _ => {
                    self.note("已取消移除");
                    KeyOutcome::None
                }
            };
        }

        // 加入空间的输入态：只收十六进制字符，回车提交、Esc 取消
        if let Some((backend, mut typed)) = self.join_space.take() {
            let outcome = match key.code {
                KeyCode::Esc => {
                    self.note("已取消加入空间");
                    KeyOutcome::None
                }
                KeyCode::Backspace => {
                    typed.pop();
                    self.join_space = Some((backend, typed));
                    KeyOutcome::None
                }
                KeyCode::Enter => {
                    let key_text = typed.trim().to_string();
                    if key_text.len() != 64 || !key_text.chars().all(|c| c.is_ascii_hexdigit()) {
                        // 不合法就留在输入态，用户能接着改
                        self.warn(format!(
                            "空间密钥是 64 位十六进制（现在是 {} 个字符）",
                            key_text.chars().count()
                        ));
                        self.join_space = Some((backend, typed));
                        KeyOutcome::None
                    } else {
                        KeyOutcome::JoinSpace {
                            backend,
                            key: key_text,
                        }
                    }
                }
                KeyCode::Char(ch) if ch.is_ascii_hexdigit() => {
                    if typed.chars().count() < 128 {
                        typed.push(ch.to_ascii_lowercase());
                    }
                    self.join_space = Some((backend, typed));
                    KeyOutcome::None
                }
                _ => {
                    self.join_space = Some((backend, typed));
                    KeyOutcome::None
                }
            };
            return outcome;
        }

        // 表单态：键盘全部交给表单（否则打字会触发退出/复制等命令）
        if self.wizard.is_some() {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let outcome = match key.code {
                KeyCode::Esc => {
                    self.close_wizard();
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
                    // 选择题（寻址）用 ←/→ 拨动；不是选择题时方向键仍按原样走焦点
                    if let Some(form) = self.wizard.as_mut()
                        && !form.cycle_value(-1)
                    {
                        match form.focus() {
                            Focus::Kind => form.kind_left(),
                            Focus::Cancel => form.focus_button(true),
                            _ => {}
                        }
                    }
                    KeyOutcome::None
                }
                KeyCode::Right => {
                    if let Some(form) = self.wizard.as_mut()
                        && !form.cycle_value(1)
                    {
                        match form.focus() {
                            Focus::Kind => form.kind_right(),
                            Focus::Save => form.focus_button(false),
                            _ => {}
                        }
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
                KeyCode::Enter => self.activate_wizard(),
                KeyCode::Char(' ')
                    if !ctrl
                        && self.wizard.as_ref().is_some_and(|form| {
                            matches!(form.focus(), Focus::Save | Focus::Cancel)
                        }) =>
                {
                    self.activate_wizard()
                }
                KeyCode::PageDown => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.scroll_by(5);
                    }
                    KeyOutcome::None
                }
                KeyCode::PageUp => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.scroll_by(-5);
                    }
                    KeyOutcome::None
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(form) = self.wizard.as_mut() {
                        form.push_char(ch);
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
            let name = self.pending_delete_name.take();
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let position = match name.as_ref() {
                        Some(name) => match self.items.iter().find(|item| &item.name == name) {
                            Some(item) => item.position,
                            None => {
                                self.warn("待删除条目已不在历史中，请刷新后重试");
                                return KeyOutcome::None;
                            }
                        },
                        None => position,
                    };
                    self.delete_expected_name = name;
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
            if key.code == KeyCode::Char('r') {
                self.doctor_error = None;
                self.doctor = None;
                return KeyOutcome::Doctor;
            }
            if key.code == KeyCode::Char('a') {
                self.wizard = Some(Wizard::new());
                return KeyOutcome::None;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            return match key.code {
                KeyCode::Char('q') => KeyOutcome::Quit,
                // 滚动键不关闭面板：引导内容在小终端上放不下，得能翻
                // ↑↓ 在面板里选后端（要移除谁）
                KeyCode::Up | KeyCode::Char('k') => {
                    self.panel_backend = self.panel_backend.saturating_sub(1);
                    KeyOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = self.backend_names().len();
                    if self.panel_backend + 1 < count {
                        self.panel_backend += 1;
                    }
                    KeyOutcome::None
                }
                // t / 空格 是"停用·启用"：可逆操作，不需要确认。
                // 空格更像"开关"这个心智模型，t 留给习惯 vim 风格的人。
                KeyCode::Char('t') | KeyCode::Char(' ') => match self.selected_backend() {
                    Some(name) => KeyOutcome::ToggleBackend(name),
                    None => {
                        self.warn("没有可切换的后端");
                        KeyOutcome::None
                    }
                },
                // s：没有空间时一键建加密空间（口令自动生成并在结果里回显一次）
                KeyCode::Char('s') => {
                    let Some(backend) = self.selected_backend() else {
                        self.warn("先添加一个后端，才能把空间元数据发布上去");
                        return KeyOutcome::None;
                    };
                    // 已经有空间时**照样能做**（等价于 cx space init --force），
                    // 但先弹确认：换密钥会让已上传的对象永久打不开。
                    if self.has_space() {
                        self.pending_space_rebuild = Some(backend);
                        KeyOutcome::None
                    } else {
                        KeyOutcome::CreateSpace {
                            backend,
                            force: false,
                        }
                    }
                }
                // i（join）：已经有空间时没用；没有空间才能用空间密钥加入。
                // 不用 j：面板里 j 是"下一个后端"（vim 风格）。
                KeyCode::Char('i') => {
                    let has_space = self.has_space();
                    let Some(name) = self.selected_backend() else {
                        self.warn("先添加一个后端，才谈得上加入空间");
                        return KeyOutcome::None;
                    };
                    if has_space {
                        // 换空间同样是"能做但有风险"：确认后进输入页
                        self.pending_space_switch = Some(name);
                        return KeyOutcome::None;
                    }
                    match self.selected_backend() {
                        Some(name) => {
                            self.join_space = Some((name, String::new()));
                            KeyOutcome::None
                        }
                        None => {
                            self.warn("先添加一个后端，才谈得上加入空间");
                            KeyOutcome::None
                        }
                    }
                }
                // E：编辑选中的后端（表单预填现值；保存前仍做连通性自测）
                KeyCode::Char('E') => match self.selected_backend() {
                    Some(name) => KeyOutcome::EditBackend(name),
                    None => {
                        self.warn("没有可编辑的后端");
                        KeyOutcome::None
                    }
                },
                // P：把空间元数据重新发布到选中后端（等价 cx space publish）
                KeyCode::Char('P') => match self.selected_backend() {
                    Some(name) => KeyOutcome::PublishSpace(name),
                    None => {
                        self.warn("没有可发布的后端");
                        KeyOutcome::None
                    }
                },
                // C：配置键值页（等价 cx config get/set）
                KeyCode::Char('C') => KeyOutcome::OpenConfig,
                // R：重启守护进程（先确认）
                KeyCode::Char('R') => {
                    self.pending_restart = true;
                    KeyOutcome::None
                }
                // O：整套配置导出到配置目录
                KeyCode::Char('O') => KeyOutcome::ExportBundle,
                // I：从配置目录的整包导入（先确认：会覆盖本机配置）
                KeyCode::Char('I') => {
                    self.pending_import = true;
                    KeyOutcome::None
                }
                // e：把空间密钥显示出来（等价于 cx space export-key）
                KeyCode::Char('e') => {
                    if self.has_space() {
                        KeyOutcome::ExportKey
                    } else {
                        self.warn("本机还没有空间，没有密钥可导出");
                        KeyOutcome::None
                    }
                }
                // 移除选中后端：先进确认，避免误删
                KeyCode::Char('x') | KeyCode::Char('d') if !ctrl => match self.selected_backend() {
                    Some(name) => {
                        self.pending_backend_remove = Some(name);
                        KeyOutcome::None
                    }
                    None => {
                        self.warn("没有可移除的后端");
                        KeyOutcome::None
                    }
                },
                // 逐行滚动（J/K）与翻页（PgUp/PgDn）分开：面板里的引导很长，两种都需要
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
            return match key.code {
                KeyCode::Char('q') => KeyOutcome::Quit,
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(self.help_max_scroll);
                    KeyOutcome::None
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    KeyOutcome::None
                }
                KeyCode::PageDown => {
                    self.help_scroll = self
                        .help_scroll
                        .saturating_add(10)
                        .min(self.help_max_scroll);
                    KeyOutcome::None
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                    KeyOutcome::None
                }
                KeyCode::End => {
                    self.help_scroll = self.help_max_scroll;
                    KeyOutcome::None
                }
                KeyCode::Home => {
                    self.help_scroll = 0;
                    KeyOutcome::None
                }
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
            // 添加后端只从"后端管理"面板里发起（面板里按 a）：
            // 主界面只做历史与剪贴板，两种操作不放在同一层，界面才不糊在一起。
            KeyCode::Char('b') => {
                self.show_panel = true;
                self.doctor_error = None;
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
                    self.pending_delete_name = self.selected_item().map(|item| item.name.clone());
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
            return Some(("还没有同步后端（按 b 添加）".to_string(), true));
        }
        if failing > 0 {
            return Some((
                format!(
                    "后端 {} 个：{} 个正在失败（按 b 看原因）",
                    status.backends.len(),
                    failing
                ),
                true,
            ));
        }
        let latest = status
            .backends
            .iter()
            .filter(|backend| backend.enabled)
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
            format!("后端 {healthy} 个无已知错误{extra}｜{tail}（按 b 看详情）"),
            false,
        ))
    }

    /// 二维鼠标路由；浮层打开时永远不穿透到历史。
    pub fn on_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> KeyOutcome {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = ratatui::layout::Position::new(mouse.column, mouse.row);
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollUp => -3,
            _ => 0,
        };
        if self.pending_backend_remove.is_some() || self.pending_delete.is_some() {
            return KeyOutcome::None;
        }
        if self.wizard.is_some() {
            if !self.modal_rect.contains(point) {
                return KeyOutcome::None;
            }
            if delta != 0 {
                if let Some(form) = self.wizard.as_mut() {
                    form.scroll_by(delta);
                }
            } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some((_, index)) = self
                    .wizard_kinds
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                {
                    if let Some(form) = self.wizard.as_mut() {
                        form.set_kind(*index);
                        form.focus_kind();
                    }
                } else if let Some((_, focus)) = self
                    .wizard_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                {
                    let focus = *focus;
                    if let Some(form) = self.wizard.as_mut() {
                        match focus {
                            Focus::Field(index) => form.focus_field(index),
                            Focus::Save => form.focus_button(true),
                            Focus::Cancel => form.focus_button(false),
                            Focus::Kind => {}
                        }
                    }
                    if matches!(focus, Focus::Save | Focus::Cancel) {
                        return self.activate_wizard();
                    }
                }
            }
            return KeyOutcome::None;
        }
        if self.show_help || self.show_panel {
            if self.modal_rect.contains(point) && delta != 0 {
                if self.show_panel {
                    self.panel_scroll = (self.panel_scroll as i32 + delta)
                        .clamp(0, self.panel_max_scroll as i32)
                        as u16;
                } else {
                    self.help_scroll = (self.help_scroll as i32 + delta)
                        .clamp(0, self.help_max_scroll as i32)
                        as u16;
                }
            }
            return KeyOutcome::None;
        }
        if self.preview_rect.contains(point) {
            if delta != 0 {
                self.preview_scroll_by(delta);
            }
        } else if self.list_rect.contains(point) {
            if delta != 0 {
                self.on_scroll(delta);
            } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.click(mouse.row);
            }
        }
        KeyOutcome::None
    }

    /// 鼠标滚轮：按行移动光标（滚轮是最自然的翻列表方式）。
    pub fn on_scroll(&mut self, delta: i32) {
        if self.show_help
            || self.show_panel
            || self.wizard.is_some()
            || self.pending_delete.is_some()
            || self.pending_backend_remove.is_some()
        {
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
                // 只留用户真正会看的东西：异常状态 + 设备名 + 历史条数。
                // 设备 id（随机十六进制）、空间 id、后端个数都不是这里该背的信息——
                // 后端明细在 b 面板里，空间状态异常时下面单独提示。
                let state = if !self.connected {
                    "连接中断（状态已过期）"
                } else if status.paused {
                    "已暂停"
                } else {
                    ""
                };
                let space = if status.encrypted {
                    ""
                } else if status.space_id.is_none() {
                    "｜未建同步空间"
                } else {
                    "｜明文（未加密）"
                };
                let head = if state.is_empty() {
                    String::new()
                } else {
                    format!("{state}｜")
                };
                format!(
                    "{head}{}｜历史 {} 条{space}",
                    status.device_name, status.history_len
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
    /// 还没有加密空间、但已配后端的场景（面板里按 s 建空间用）。
    fn status_without_space(backends: serde_json::Value) -> StatusReport {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "pid": 42,
            "started_ms": 1_759_323_000_000u64,
            "paused": false,
            "device_id": "ab12cd34ef567890",
            "device_name": "笔记本",
            "space_id": null,
            "encrypted": false,
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
        // 一句话说清"去哪加"，不再顺带解释 LAN/凭据（那些进面板再看）
        assert!(text.contains("按 b"), "要指向界面内的配置引导：{text}");
        assert!(!text.contains("LAN"), "提示行不该再塞这些细节：{text}");
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
        assert!(text.contains("1 个无已知错误"), "{text}");
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
        // q 直接退出；面板里现在有专门的交互键（↑↓ 选后端、x 移除、J/K 滚动），
        // 其余键才关闭面板
        assert_eq!(app.on_key(key(KeyCode::Char('z'))), KeyOutcome::None);
        assert!(!app.show_panel, "非交互键应当关闭面板");
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), KeyOutcome::Quit);
    }

    #[test]
    fn 面板打开时按键不会误触列表操作() {
        let mut app = app_with(3);
        assert_eq!(app.on_key(key(KeyCode::Char('b'))), KeyOutcome::Doctor);
        assert!(app.show_panel);
        // 面板开着时 j 是"选后端"，不该移动列表光标
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 0, "面板打开时 j 不该移动列表光标");
        assert!(app.show_panel, "j 是面板内的交互键，不该关掉面板");
        // 非交互键才关闭
        app.on_key(key(KeyCode::Char('z')));
        assert!(!app.show_panel, "非交互键应当关掉面板");

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

        // 非交互键仍然关闭面板（x 现在是"移除选中"，不再承担关闭职责）
        app.on_key(key(KeyCode::Char('z')));
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
    fn 面板里可以选后端并确认移除() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([
                backend("home", true, 0, None),
                backend("oss", true, 0, None),
            ]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.selected_backend().as_deref(), Some("home"));

        // ↓ 选到第二个；到底不再越界
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_backend().as_deref(), Some("oss"));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected_backend().as_deref(),
            Some("oss"),
            "到底不该越界"
        );
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_backend().as_deref(), Some("home"));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(
            app.selected_backend().as_deref(),
            Some("home"),
            "到顶不该越界"
        );

        // x 只是进入确认，不立刻移除
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), KeyOutcome::None);
        assert_eq!(app.pending_backend_remove.as_deref(), Some("home"));
        // 取消
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), KeyOutcome::None);
        assert!(app.pending_backend_remove.is_none());
        assert!(!app.notice.as_ref().unwrap().is_error);

        // y 确认
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            KeyOutcome::RemoveBackend("home".to_string())
        );
        // 回车也算确认
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            KeyOutcome::RemoveBackend("home".to_string())
        );
    }

    #[test]
    fn 面板没有后端时移除给出提示() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('x'))), KeyOutcome::None);
        assert!(app.pending_backend_remove.is_none());
        assert!(app.notice.as_ref().unwrap().text.contains("没有可移除"));
    }

    #[test]
    fn 后端变少时选中下标夹回范围内() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([
                backend("home", true, 0, None),
                backend("oss", true, 0, None)
            ]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_backend().as_deref(), Some("oss"));

        // 移除之后守护进程报回来的后端少了一个：下标必须夹回范围内，
        // 否则面板里既没有 ▶、空格也没反应，看起来像键盘坏了
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("home", true, 0, None)]),
        ))));
        assert_eq!(app.selected_backend().as_deref(), Some("home"));
    }

    #[test]
    fn 已有空间时按_s_进确认页_同意后带_force_重建() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('s'))), KeyOutcome::None);
        assert_eq!(
            app.pending_space_rebuild.as_deref(),
            Some("oss"),
            "应当停在确认页，而不是直接拒绝"
        );
        // 取消：不动手
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), KeyOutcome::None);
        assert!(app.pending_space_rebuild.is_none());
        // 再按一次并同意：等价 cx space init --force
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            KeyOutcome::CreateSpace {
                backend: "oss".to_string(),
                force: true
            }
        );
    }

    #[test]
    fn 已有空间时按_i_确认后进入密钥输入页() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('i'))), KeyOutcome::None);
        assert_eq!(app.pending_space_switch.as_deref(), Some("oss"));
        assert_eq!(app.on_key(key(KeyCode::Char('y'))), KeyOutcome::None);
        assert!(
            app.join_space.is_some(),
            "确认之后应当进入密钥输入页（换空间也做得到，只是要先问）"
        );
    }

    #[test]
    fn 面板里按大写e请求编辑选中后端() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('E'))),
            KeyOutcome::EditBackend("oss".to_string())
        );
    }

    #[test]
    fn 编辑表单提交走保存而不是新增() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        let draft = crate::wizard::BackendDraft {
            kind: crate::wizard::WizardKind::S3,
            values: vec!["oss".into(); 10],
        };
        // 有编辑目标 → SaveBackend（按原名替换）
        app.wizard_edit_target = Some("oss".to_string());
        match app.backend_outcome(draft.clone()) {
            KeyOutcome::SaveBackend { original, .. } => assert_eq!(original, "oss"),
            other => panic!("期望 SaveBackend，实际 {other:?}"),
        }
        assert!(app.wizard_edit_target.is_none(), "提交后要清掉编辑目标");
        // 没有编辑目标 → AddBackend（新后端）
        assert!(matches!(
            app.backend_outcome(draft),
            KeyOutcome::AddBackend(_)
        ));
    }

    #[test]
    fn 取消编辑后再新增不会被当成保存修改() {
        let mut app = App::new();
        app.wizard = Some(crate::wizard::Wizard::new());
        app.wizard_edit_target = Some("oss".to_string());
        // Esc 关掉表单
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        assert!(app.wizard.is_none());
        assert!(
            app.wizard_edit_target.is_none(),
            "取消编辑必须清掉编辑目标，否则下一次新增会覆盖那个后端"
        );
        // 之后新增照旧是新后端
        let draft = crate::wizard::BackendDraft {
            kind: crate::wizard::WizardKind::S3,
            values: vec!["home".into(); 10],
        };
        assert!(matches!(
            app.backend_outcome(draft),
            KeyOutcome::AddBackend(_)
        ));
    }
    #[test]
    fn 面板里按大写c打开配置页() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('C'))), KeyOutcome::OpenConfig);
    }

    #[test]
    fn 配置页回车编辑_提交产生设置意图() {
        let mut app = App::new();
        app.config_page = Some(ConfigPage {
            items: vec![("lan.port".to_string(), "8688".to_string())],
            cursor: 0,
            editing: None,
        });
        // 进入编辑：初值是当前值
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::None);
        assert_eq!(
            app.config_page.as_ref().unwrap().editing.as_deref(),
            Some("8688")
        );
        // 追加一位后提交
        app.on_key(key(KeyCode::Char('0')));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            KeyOutcome::SetConfig {
                key: "lan.port".to_string(),
                value: "86880".to_string()
            }
        );
    }

    #[test]
    fn 配置页按esc先放弃编辑再关闭页面() {
        let mut app = App::new();
        app.config_page = Some(ConfigPage {
            items: vec![("lan.port".to_string(), "8688".to_string())],
            cursor: 0,
            editing: Some("123".to_string()),
        });
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        let page = app.config_page.as_ref().expect("第一次 Esc 只放弃编辑");
        assert!(page.editing.is_none());
        assert_eq!(page.items[0].1, "8688", "放弃编辑不该改值");
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        assert!(app.config_page.is_none(), "第二次 Esc 关闭配置页");
    }

    #[test]
    fn 面板里按大写r先确认再重启守护进程() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('R'))), KeyOutcome::None);
        assert!(app.pending_restart);
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        assert!(!app.pending_restart, "Esc 取消重启");
        app.on_key(key(KeyCode::Char('R')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            KeyOutcome::RestartDaemon
        );
    }
    #[test]
    fn 面板里按大写o导出并按大写i先确认再导入() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('O'))),
            KeyOutcome::ExportBundle
        );
        assert_eq!(app.on_key(key(KeyCode::Char('I'))), KeyOutcome::None);
        assert!(app.pending_import, "导入要先停在确认页");
        assert_eq!(app.on_key(key(KeyCode::Char('n'))), KeyOutcome::None);
        assert!(!app.pending_import);
        app.on_key(key(KeyCode::Char('I')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('y'))),
            KeyOutcome::ImportBundle
        );
    }

    #[test]
    fn 面板里按大p重新发布空间元数据() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('P'))),
            KeyOutcome::PublishSpace("oss".to_string())
        );
    }
    #[test]
    fn 面板里按_e_导出空间密钥() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('e'))), KeyOutcome::ExportKey);
        // 展示页：x 导出到文件（不是复制到剪贴板——剪贴板会被同步出去）
        app.exported_key = Some("a".repeat(64));
        assert_eq!(
            app.on_key(key(KeyCode::Char('x'))),
            KeyOutcome::ExportKeyToFile
        );
        assert!(
            app.exported_key.is_some(),
            "导出后仍停在密钥页，好让用户看到路径"
        );
        // 其他任意键返回
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        assert!(app.exported_key.is_none());
        assert!(app.exported_key_note.is_none(), "返回时要清掉导出结果");
    }
    #[test]
    fn 加入空间支持粘贴整段密钥() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Char('i')));
        assert!(app.join_space.is_some());
        // 从另一台机器复制来的密钥常带换行/空格，甚至前后有提示文字
        let pasted = "  E388B01B\n".to_string() + &"a1b2c3d4".repeat(7);
        app.on_paste(&pasted);
        let (backend, typed) = app.join_space.clone().unwrap();
        assert_eq!(backend, "oss");
        assert_eq!(typed.chars().count(), 64, "应当一次吃下整段密钥");
        assert!(typed.starts_with("e388b01b"), "统一小写：{typed}");
        assert!(typed.chars().all(|c| c.is_ascii_hexdigit()));
        // 满 64 位后回车即为有效提交
        match app.on_key(key(KeyCode::Enter)) {
            KeyOutcome::JoinSpace { key, .. } => assert_eq!(key.len(), 64),
            other => panic!("期望 JoinSpace，实际 {other:?}"),
        }
    }

    #[test]
    fn 加入空间粘贴时忽略非十六进制字符() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Char('i')));
        app.on_paste("空间密钥：abc-def ghi");
        assert_eq!(
            app.join_space.as_ref().unwrap().1,
            "abcdef",
            "只留十六进制字符"
        );
    }
    #[test]
    fn 面板里按_i_进入加入空间的输入态() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('i'))), KeyOutcome::None);
        assert!(
            app.join_space
                .as_ref()
                .is_some_and(|(backend, typed)| backend == "oss" && typed.is_empty()),
            "应当进入输入态，目标是选中的后端"
        );
    }

    #[test]
    fn 加入空间只收十六进制_满_64_位才是有效提交() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Char('i')));
        // 非十六进制字符被忽略
        for ch in "zz".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.join_space.as_ref().unwrap().1, "", "只收 0-9a-f");
        // 位数不够时回车只提示、留在输入态
        for ch in "abc".chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::None);
        assert!(app.join_space.is_some(), "位数不够不该退出输入态");
        assert!(app.notice.as_ref().unwrap().text.contains("64 位"));
        // 补齐 64 位后回车产生 JoinSpace
        for ch in "0123456789abcdef".repeat(4).chars().take(61) {
            app.on_key(key(KeyCode::Char(ch)));
        }
        match app.on_key(key(KeyCode::Enter)) {
            KeyOutcome::JoinSpace { backend, key } => {
                assert_eq!(backend, "oss");
                assert_eq!(key.len(), 64);
            }
            other => panic!("期望 JoinSpace，实际 {other:?}"),
        }
    }

    #[test]
    fn 已有空间时按_i_不进入输入态() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('i'))), KeyOutcome::None);
        // 不直接进输入页：先要用户确认"换空间"（确认后才进输入页）
        assert_eq!(app.pending_space_switch.as_deref(), Some("oss"));
        assert!(app.join_space.is_none(), "确认之前不该进入输入页");
    }
    #[test]
    fn 面板里按_s_要求建加密空间() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('s'))),
            KeyOutcome::CreateSpace {
                backend: "oss".to_string(),
                force: false
            },
            "面板里按 s 应当发起建空间，发布目标是选中的后端"
        );
        assert!(app.show_panel, "建空间不该关掉面板");
    }

    #[test]
    fn 已有空间时按_s_先要确认() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("oss", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('s'))),
            KeyOutcome::None,
            "已有空间时按 s 绝不能发起重建（换密钥会让旧对象解不开）"
        );
        // 不直接重建：先停在确认页（确认后才带 force 执行）
        assert_eq!(app.pending_space_rebuild.as_deref(), Some("oss"));
    }
    #[test]
    fn 没有后端时按_s_给出提示而不是发起建空间() {
        let mut app = App::new();
        // 没有空间、也没有后端：这才是"该先加后端"的场景
        app.apply(UiEvent::Status(Box::new(status_without_space(
            serde_json::json!([]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('s'))), KeyOutcome::None);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("先添加一个后端")),
            "要说明没后端时建不了空间"
        );
    }
    #[test]
    fn 面板里空格就是开关() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("home", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        // 空格在列表页是"暂停"，在面板里必须是"开关"：同一个键在不同上下文
        // 有不同含义，但绝不能把面板里的开关变成暂停同步
        assert_eq!(
            app.on_key(key(KeyCode::Char(' '))),
            KeyOutcome::ToggleBackend("home".to_string())
        );
        assert!(app.show_panel, "开关后端不该关掉面板");
    }

    #[test]
    fn 面板里按_a_直接进表单且填完回到面板() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("home", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(app.on_key(key(KeyCode::Char('a'))), KeyOutcome::None);
        assert!(app.wizard.is_some(), "面板里按 a 应当直接打开表单");
        // 取消后回到面板：用户的落点是"管理后端"，不是主界面
        assert_eq!(app.on_key(key(KeyCode::Esc)), KeyOutcome::None);
        assert!(app.wizard.is_none());
        assert!(app.show_panel, "取消表单要回到面板，而不是掉回历史列表");
    }

    #[test]
    fn 表单里寻址用左右键拨动() {
        let mut app = App::new();
        app.show_panel = true; // 添加后端现在只从「后端管理」面板发起
        app.on_key(key(KeyCode::Char('a')));
        // 切到 S3，再把焦点落到"寻址"
        let form = app.wizard.as_mut().expect("表单已打开");
        form.set_kind(2);
        let index = form
            .fields()
            .iter()
            .position(|field| field.label == "寻址")
            .expect("S3 表单必须有寻址字段");
        form.focus_field(index);
        assert_eq!(form.value(index), "");

        app.on_key(key(KeyCode::Right));
        assert_eq!(
            app.wizard.as_ref().unwrap().value(index),
            "virtual-host",
            "→ 应当拨到下一项"
        );
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.wizard.as_ref().unwrap().value(index), "path");
        app.on_key(key(KeyCode::Left));
        assert_eq!(
            app.wizard.as_ref().unwrap().value(index),
            "virtual-host",
            "← 应当拨回上一项"
        );
    }

    #[test]
    fn 面板里_ctrl_d_仍然滚动而不是触发移除() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("home", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(ctrl(KeyCode::Char('d')));
        assert_eq!(app.panel_scroll(), 10, "Ctrl+D 应当滚面板");
        assert!(
            app.pending_backend_remove.is_none(),
            "Ctrl+D 不该被当成移除后端"
        );
        app.on_key(ctrl(KeyCode::Char('u')));
        assert_eq!(app.panel_scroll(), 0);
    }
    #[test]
    fn 粘贴进表单字段_并去掉末尾换行() {
        let mut app = App::new();
        app.show_panel = true; // 添加后端现在只从「后端管理」面板发起
        app.on_key(key(KeyCode::Char('a')));
        assert!(app.wizard.is_some());
        // 焦点先落到第一个字段（类型行 → Tab 到字段）
        app.on_key(key(KeyCode::Tab));

        // 先清空（后端名预填了示例值），再粘贴一段带换行的密钥：不能把换行带进字段
        app.on_key(ctrl(KeyCode::Char('l')));
        app.on_paste("AKID-1234\n");
        let form = app.wizard.as_ref().unwrap();
        let index = form.focused_field().unwrap();
        assert_eq!(form.value(index), "AKID-1234", "换行必须被去掉");

        // 再粘一段（模拟分两次粘贴）应当追加到已填内容后面
        app.on_paste("SECOND");
        let form = app.wizard.as_ref().unwrap();
        assert_eq!(form.value(index), "AKID-1234SECOND");
        assert!(app.notice.is_none(), "粘贴不该留下提示");
    }

    #[test]
    fn 粘贴进过滤框() {
        let mut app = app_with(3);
        app.on_key(key(KeyCode::Char('/')));
        assert!(app.filter_input);
        app.on_paste("关键词\n");
        assert_eq!(app.filter(), "关键词", "过滤词同样不该带换行");
    }

    #[test]
    fn 没有输入目标时粘贴不改变任何东西() {
        let mut app = app_with(3);
        app.down();
        let before = (app.cursor, app.filter().to_string(), app.wizard.is_some());
        app.on_paste("随便粘点什么");
        assert_eq!(
            (app.cursor, app.filter().to_string(), app.wizard.is_some()),
            before,
            "列表上粘贴不该有任何副作用"
        );

        // 表单开着但焦点在按钮上：也不该写进任何字段
        let mut form_app = App::new();
        form_app.show_panel = true;
        form_app.show_panel = true; // 添加后端现在只从「后端管理」面板发起
        form_app.on_key(key(KeyCode::Char('a')));
        let fields = form_app.wizard.as_ref().unwrap().fields().len();
        for _ in 0..=fields {
            form_app.on_key(key(KeyCode::Tab)); // 走到按钮
        }
        form_app.on_paste("不该落进去");
        let form = form_app.wizard.as_ref().unwrap();
        for index in 0..fields {
            assert!(
                !form.value(index).contains("不该落进去"),
                "焦点在按钮上时粘贴不该写进字段 {index}"
            );
        }
    }
    #[test]
    fn 面板里_t_切换选中后端的启用状态() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status_with_backends(
            false,
            serde_json::json!([backend("home", true, 0, None)]),
        ))));
        app.on_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.on_key(key(KeyCode::Char('t'))),
            KeyOutcome::ToggleBackend("home".to_string()),
            "t 应当针对选中的后端"
        );

        // 没有后端时给出提示而不是静默
        let mut empty = App::new();
        empty.apply(UiEvent::Status(Box::new(status(false))));
        empty.on_key(key(KeyCode::Char('b')));
        assert_eq!(empty.on_key(key(KeyCode::Char('t'))), KeyOutcome::None);
        assert!(empty.notice.as_ref().unwrap().text.contains("没有可切换"));
    }
    #[test]
    fn 取消按钮回车空格完整与不完整表单都绝不保存() {
        for complete in [false, true] {
            for code in [KeyCode::Enter, KeyCode::Char(' ')] {
                for save in [false, true] {
                    let mut app = App::new();
                    app.wizard = Some(Wizard::new());
                    let form = app.wizard.as_mut().unwrap();
                    if complete {
                        for index in 1..form.fields().len() {
                            form.focus_field(index);
                            for ch in "fake-value".chars() {
                                form.push_char(ch);
                            }
                        }
                    }
                    form.focus_button(save);
                    let outcome = app.on_key(key(code));
                    assert_eq!(
                        matches!(outcome, KeyOutcome::AddBackend(_)),
                        save && complete
                    );
                    if !save {
                        assert!(app.wizard.is_none());
                    } else if !complete {
                        assert!(app.wizard.is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn 面板单次_a_打开表单且取消返回面板() {
        for count in [0, 2] {
            let mut app = app_with(count);
            app.on_key(key(KeyCode::Char('b')));
            app.show_panel = true; // 添加后端现在只从「后端管理」面板发起
            app.on_key(key(KeyCode::Char('a')));
            assert!(app.wizard.is_some());
            app.on_key(key(KeyCode::Esc));
            assert!(app.show_panel);
        }
    }

    #[test]
    fn 新历史插入保持选择且确认删除仍绑定原对象() {
        let mut app = app_with(3);
        app.down();
        app.apply(preview(1, &"line\n".repeat(100)));
        app.preview_scroll_by(4);
        let name = app.selected_item().unwrap().name.clone();
        app.on_key(key(KeyCode::Char('d')));
        let mut items = vec![summary(99)];
        items.extend((0..3).map(summary));
        for (position, item) in items.iter_mut().enumerate() {
            item.position = position;
        }
        app.apply(UiEvent::History(items));
        assert_eq!(app.selected_item().unwrap().name, name);
        assert_eq!(app.preview_scroll(), 4);
        assert_eq!(app.on_key(key(KeyCode::Enter)), KeyOutcome::Delete(2));
        assert_eq!(app.delete_expected_name.as_deref(), Some(name.as_str()));
    }

    #[test]
    fn 秘密粘贴与提交原样保留空白字节() {
        let mut app = App::new();
        let mut form = Wizard::new();
        form.set_kind(1);
        form.focus_field(1);
        for ch in "https://example.invalid/dav".chars() {
            form.push_char(ch);
        }
        form.focus_field(2);
        form.push_char('u');
        form.focus_field(3);
        app.wizard = Some(form);
        let secret = "  fictional secret\r\n ";
        app.on_paste(secret);
        let draft = app.wizard.as_mut().unwrap().submit().unwrap();
        assert_eq!(draft.get("口令"), secret);
    }

    #[test]
    fn 状态行包含关键信息() {
        let mut app = App::new();
        assert!(app.status_line().contains("未连接"));
        app.apply(UiEvent::Status(Box::new(status(false))));
        let line = app.status_line();
        // 正常状态不再重复"守护进程已连接"，也不展示随机十六进制的设备/空间 id：
        // 只留设备名与历史条数（后端明细在 b 面板里）。
        assert!(!line.contains("守护进程已连接"), "{line}");
        assert!(line.contains("笔记本"), "{line}");
        assert!(
            !line.contains("ab12cd34"),
            "设备 id 不该出现在状态行：{line}"
        );
        assert!(line.contains("历史 3 条"), "{line}");

        app.apply(UiEvent::Status(Box::new(status(true))));
        assert!(app.status_line().contains("已暂停"));
    }
}
