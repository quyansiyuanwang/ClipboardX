//! 渲染。
//!
//! 与终端解耦：`draw` 只依赖 `ratatui::Frame`，所以可以在 `TestBackend` 上画一帧
//! 再断言"画面上有什么字"——TUI 的渲染逻辑因此也进了单元测试，而不是只能靠人眼。
//!
//! 布局策略：宽屏左右分栏（列表 + 预览），**窄屏改为上下**——在 80 列以下的终端里
//! 左右分栏会把两边都挤到不可读，上下堆叠反而能用。

use crate::app::{App, PreviewState};
use crate::wizard::{Focus, WizardKind};
use cx_proto::control::{BackendStatus, HistorySummary};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap};

/// 低于这个宽度就改成上下布局。
const NARROW_WIDTH: u16 = 84;

/// 画一帧。
///
/// 接收 `&mut App` 是因为渲染同时要回填"列表可视行数"（决定预取窗口与分页步长），
/// 这样布局只有一个权威来源，不必在别处再算一遍。
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // 头部高度按内容算：标题 + 状态（+ 健康提示 + 最近一条提示），**再加一行下边框**。
    // 少算边框那一行会把状态行直接裁掉——这个错误在"未连接"的渲染测试里被抓到了。
    let mut header = 3u16;
    // 只有"需要用户动手"的健康问题才占一行；一切正常时不占版面
    if app.health_line().is_some_and(|(_, warn)| warn) {
        header += 1;
    }
    if app.notice.is_some() {
        header += 1;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header),
            Constraint::Min(6),    // 历史 + 预览
            Constraint::Length(1), // 快捷键 / 过滤输入
        ])
        .split(area);

    draw_body(frame, chunks[1], app);
    draw_header(frame, chunks[0], app);
    draw_footer(frame, chunks[2], app);

    app.wizard_hits.clear();
    app.wizard_kinds.clear();
    // 整屏页面按层级叠加：面板在下，帮助/确认在其上，表单最上。
    // 每个页面都铺满整屏，顺序错了就是"后面那个把前面那个整块盖住"。
    if app.show_panel {
        draw_panel(frame, area, app);
    }
    if app.join_space.is_some() {
        draw_join_space(frame, area, app);
    }
    if let Some(backend) = &app.pending_space_rebuild {
        draw_confirm_space(frame, area, backend, SpaceRisk::Rebuild);
    }
    if let Some(backend) = &app.pending_space_switch {
        draw_confirm_space(frame, area, backend, SpaceRisk::Switch);
    }
    if let Some(key) = &app.exported_key {
        draw_export_key(frame, area, key, app.exported_key_note.as_deref());
    }
    if app.show_help {
        draw_help(frame, area, app);
    }
    if let Some(position) = app.pending_delete {
        draw_confirm_delete(frame, area, position, app.pending_delete_name.as_deref());
    }
    if let Some(name) = &app.pending_backend_remove {
        draw_confirm_remove(frame, area, name);
    }
    if app.wizard.is_some() {
        draw_wizard(frame, area, app);
    }
}

/// 危险动作的两种确认：重建空间 / 换空间。
#[derive(Debug, Clone, Copy)]
enum SpaceRisk {
    /// 等价 `cx space init --force`。
    Rebuild,
    /// 等价重新 `cx space join`。
    Switch,
}
/// 危险动作的确认页：一行问题、一行后果、一行按键。不写教程。
fn draw_confirm_space(frame: &mut Frame<'_>, area: Rect, backend: &str, risk: SpaceRisk) {
    frame.render_widget(Clear, area);
    let (title, question, consequence) = match risk {
        SpaceRisk::Rebuild => (
            " 重建空间 ",
            format!("重建加密空间？（{backend}）"),
            "旧密钥作废：已上传的对象再也解不开",
        ),
        SpaceRisk::Switch => (
            " 换空间 ",
            format!("换掉本机空间？（{backend}）"),
            "当前空间与它的对象会留在原地，但不再使用",
        ),
    };
    let inner_w = area.width.saturating_sub(2) as usize;
    let indent = " ".repeat(inner_w.saturating_sub(inner_w.min(76)) / 2);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{indent}{question}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{indent}{consequence}"),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{indent}y"), Style::default().fg(Color::Yellow)),
            Span::raw(" 继续　"),
            Span::styled("其他键", Style::default().fg(Color::DarkGray)),
            Span::raw(" 取消"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
}

/// 空间密钥页：只有密钥与一行按键。长密钥按显示列折行，绝不截断。
fn draw_export_key(frame: &mut Frame<'_>, area: Rect, key: &str, note: Option<&str>) {
    frame.render_widget(Clear, area);
    let inner_w = area.width.saturating_sub(2) as usize;
    let content_w = inner_w.min(76);
    let indent = " ".repeat(inner_w.saturating_sub(content_w) / 2);
    let key_w = content_w.saturating_sub(1).max(8);
    let style = Style::default().fg(Color::White).bg(Color::Indexed(238));
    let mut lines = vec![Line::from("")];
    for chunk in key.as_bytes().chunks(key_w) {
        let text = String::from_utf8_lossy(chunk);
        let pad = key_w.saturating_sub(display_width(&text));
        lines.push(Line::from(vec![
            Span::raw(format!("{indent} ")),
            Span::styled(format!("{text}{}", " ".repeat(pad)), style),
        ]));
    }
    lines.push(Line::from(""));
    if let Some(note) = note {
        lines.push(Line::from(Span::styled(
            format!("{indent}{note}"),
            Style::default().fg(Color::Green),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled(format!("{indent}x"), Style::default().fg(Color::Yellow)),
        Span::raw(" 导出到文件　"),
        Span::styled("其他键", Style::default().fg(Color::DarkGray)),
        Span::raw(" 返回"),
    ]));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" 空间密钥（{} 位） ", key.chars().count()))
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}

/// 加入空间：一行后端、一行密钥输入、一行进度、一行按键。
fn draw_join_space(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    app.modal_rect = area;
    frame.render_widget(Clear, area);
    let Some((backend, typed)) = app.join_space.clone() else {
        return;
    };
    const LABEL_W: usize = 6;
    let inner_w = area.width.saturating_sub(2) as usize;
    let content_w = inner_w.min(76);
    let indent = " ".repeat(inner_w.saturating_sub(content_w) / 2);
    let count = typed.chars().count();
    let shown: String = if count > 40 {
        typed.chars().skip(count - 40).collect()
    } else {
        typed.clone()
    };
    let label = |text: &str| {
        Span::styled(
            format!("{indent}{}", pad_display(text, LABEL_W)),
            Style::default().fg(Color::Cyan),
        )
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![label("后端"), Span::raw(backend)]),
        Line::from(vec![
            label("密钥"),
            Span::styled(
                format!("{shown}▏"),
                Style::default().fg(Color::White).bg(Color::Indexed(238)),
            ),
        ]),
        Line::from(Span::styled(
            format!("{indent}{}{} / 64", " ".repeat(LABEL_W), count.min(64)),
            Style::default().fg(if count == 64 {
                Color::Green
            } else {
                Color::DarkGray
            }),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{indent}回车"), Style::default().fg(Color::Yellow)),
            Span::raw(" 加入　"),
            Span::styled("Esc", Style::default().fg(Color::DarkGray)),
            Span::raw(" 取消"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 加入空间 ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}
/// 移除后端的确认：与删除历史条目一样，破坏性操作默认"不删"。
fn draw_confirm_remove(frame: &mut Frame<'_>, area: Rect, name: &str) {
    let popup = area;
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            format!("移除后端 {name}？"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  只删本机配置与凭据，不删除已上传到该后端的对象；"),
        Line::from("  其他已配置的后端不受影响；在 b 面板里按 a 可以再加回来。"),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y / 回车", Style::default().fg(Color::Yellow)),
            Span::raw("  移除　　"),
            Span::styled("其他任意键", Style::default().fg(Color::DarkGray)),
            Span::raw("  取消"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 确认移除 ")
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// 删除确认：唯一会真的丢内容的操作，因此单独一个浮层，且默认是"不删"。
fn draw_confirm_delete(frame: &mut Frame<'_>, area: Rect, position: usize, name: Option<&str>) {
    let popup = area;
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            format!("删除第 {position} 条？"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("对象 {}", name.unwrap_or("未知；请重新选择"))),
        Line::from("  只从本机历史里移除。后端对象与其他设备上的副本不受影响，"),
        Line::from("  已复制到别处的内容也不会被追回。"),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y / 回车", Style::default().fg(Color::Yellow)),
            Span::raw("  删除　　"),
            Span::styled("其他任意键", Style::default().fg(Color::DarkGray)),
            Span::raw("  取消"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 确认删除 ")
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// 画主体（列表 + 预览），返回列表的可视行数供调用方回填。
fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> usize {
    let narrow = area.width < NARROW_WIDTH;
    let panes = if narrow {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(area)
    };

    app.set_viewport_rows(panes[0].height.saturating_sub(2) as usize);
    draw_history(frame, panes[0], app);
    draw_preview(frame, panes[1], app);

    panes[0].height.saturating_sub(2) as usize
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(Span::styled(
        " ClipboardX ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let state = app.status_line();
    let state_style = match &app.status {
        _ if !app.connected => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::Red),
        Some(status) if status.paused => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
    };
    let mut lines = vec![title, Line::from(Span::styled(state, state_style))];
    // 健康行只在出问题时才显示（"我明明开着为什么不同步"的答案），
    // 一切正常就不占一行——界面保持安静。
    if let Some((text, true)) = app.health_line() {
        lines.push(Line::from(Span::styled(
            format!(" ! {text}"),
            Style::default().fg(Color::Yellow),
        )));
    }
    // 提示只在"主界面"显示。面板/表单/确认页都是整屏页，它们自己会显示结果；
    // 两头都画会让文字从边框缝里漏出来（实测就是这么花的）。
    let full_screen_page = app.show_panel
        || app.show_help
        || app.wizard.is_some()
        || app.pending_delete.is_some()
        || app.pending_backend_remove.is_some()
        || app.join_space.is_some()
        || app.pending_space_rebuild.is_some()
        || app.pending_space_switch.is_some()
        || app.exported_key.is_some();
    if !full_screen_page && let Some(notice) = &app.notice {
        lines.push(Line::from(Span::styled(
            format!(" {}", notice.text),
            if notice.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Cyan)
            },
        )));
    }
    let block = Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(block, area);
}

fn draw_history(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem<'_>> = app
        .visible()
        .iter()
        .filter_map(|index| app.items.get(*index))
        .map(|item| ListItem::new(row_text(item, app, width)))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(history_title(app)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ratatui::widgets::ListState::default();
    if !app.visible().is_empty() {
        state.select(Some(app.cursor.min(app.visible().len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);

    // 把"列表内容区在哪、滚到第几行"回填给 App：鼠标点击要靠它把屏幕坐标换算成条目下标。
    // offset 是 ratatui 为了"让选中项可见"自己算的，只有渲染之后才拿得到。
    let inner_top = area.y.saturating_add(1);
    let inner_rows = area.height.saturating_sub(2);
    app.set_list_geometry(inner_top, inner_rows, state.offset());
    app.list_rect = Rect::new(
        area.x.saturating_add(1),
        inner_top,
        area.width.saturating_sub(2),
        inner_rows,
    );
}

/// 列表标题：条数 + 光标位置，过滤生效时把过滤词也写出来。
fn history_title(app: &App) -> String {
    let total = app.items.len();
    let shown = app.visible_len();
    let position = if shown == 0 {
        "0/0".to_string()
    } else {
        format!("{}/{}", app.cursor + 1, shown)
    };
    if app.filter_active() {
        format!(
            " 历史 {position}（过滤 “{}” {shown}/{total}）",
            app.filter()
        )
    } else {
        format!(" 历史 {position}（{total} 条）")
    }
}

/// 一行：标记 + 时间 + 类型 + 大小 + 内容摘要。
///
/// 内容摘要来自按需加载的预览：没加载出来时显示 `…`，失败就地把原因标出来，
/// 这样"这一行是什么"不必先选中才知道。
fn row_text(item: &HistorySummary, app: &App, width: usize) -> Line<'static> {
    let marker = origin_marker(item, app);
    let kind = kind_label(&item.kind);
    let state = app.preview_state(&item.name);

    let (summary, summary_style) = match state {
        None | Some(PreviewState::Loading) => {
            ("…".to_string(), Style::default().fg(Color::DarkGray))
        }
        Some(PreviewState::Failed(_)) => {
            ("（读取失败）".to_string(), Style::default().fg(Color::Red))
        }
        Some(PreviewState::Ready(preview)) => {
            let text = preview
                .text
                .as_deref()
                .map(one_line)
                .or_else(|| {
                    preview
                        .image
                        .as_deref()
                        .map(|image| format!("图片 {image}"))
                })
                .unwrap_or_else(|| "（空内容）".to_string());
            (text, Style::default())
        }
    };

    // 来源设备名只给"来自别的设备"的条目看：
    // - 本机产生的条目占多数，每行都写自己的设备名是噪音（标记 ↑ 已经说明了）；
    // - 但"这条是从哪台机器来的"在多设备场景下正是要看的东西。
    // 列表窄到一定程度就整体不显示，免得把内容摘要挤没。
    let source = match (marker, width) {
        ("↑", _) => None,
        (_, width) if width < MIN_SOURCE_WIDTH => None,
        _ => Some(format!("{} ", clamp_width_value(origin_name(item), 12))),
    };
    let source_len = source.as_ref().map(|text| display_width(text)).unwrap_or(0);

    let head = format!(
        "{marker} {} {kind} {} ",
        relative_time(item.created_ms),
        human_bytes(item.bytes as u64)
    );
    let used = display_width(&head) + source_len + 2; // 高亮符号 "▶ " 占两列
    let room = width.saturating_sub(used);
    if room == 0 {
        return Line::from(Span::raw(head.trim_end().to_string()));
    }
    let mut spans = vec![Span::raw(head)];
    if let Some(source) = source {
        spans.push(Span::styled(source, Style::default().fg(Color::Magenta)));
    }
    spans.push(Span::styled(
        clamp_width_value(summary, room),
        summary_style,
    ));
    Line::from(spans)
}

/// 行内显示来源设备名所需的最小列宽（再窄就只剩下标记与摘要）。
const MIN_SOURCE_WIDTH: usize = 40;

/// 来源设备名（名称为空时退回短设备 id）。
fn origin_name(item: &HistorySummary) -> String {
    if item.origin.trim().is_empty() {
        crate::app::short(&item.origin_device)
    } else {
        item.origin.trim().to_string()
    }
}

/// 单行化：把换行/制表符折成空格，便于列表里一眼看全。
fn one_line(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "image" => "图片",
        "text" | "" => "文本",
        other => other,
    }
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let item = app.selected_item();
    let title = match (item, app.preview_scroll()) {
        (Some(item), 0) => format!(" 内容预览（第 {} 条）", item.position),
        (Some(item), offset) => format!(
            " 内容预览（第 {} 条，已下滚 {offset} 行，0 回到开头）",
            item.position
        ),
        (None, _) => " 内容预览 ".to_string(),
    };

    let text = match (app.selected_preview(), app.selected_preview_error(), item) {
        (Some(preview), _, Some(item)) => preview_text(preview, item, app),
        (None, Some(err), _) => Text::from(vec![Line::from(Span::styled(
            format!("读取预览失败：{err}"),
            Style::default().fg(Color::Red),
        ))]),
        (None, None, None) => Text::from(vec![Line::from(Span::styled(
            if app.filter_active() {
                "当前过滤没有匹配到内容（Esc 清除过滤）"
            } else {
                "历史里还没有内容。复制点什么，或按 p 发布。"
            },
            Style::default().fg(Color::DarkGray),
        ))]),
        // 有内容摘要在手上时走第一个分支；这里兜住其余组合（正在读取等）
        _ => Text::from(vec![Line::from(Span::styled(
            "（正在读取…）",
            Style::default().fg(Color::DarkGray),
        ))]),
    };

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let lines = wrapped_lines(text, inner.width);
    app.set_preview_geometry(inner, lines.len());
    let block = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((
            if app.selected_preview_state().is_some() {
                app.preview_scroll()
            } else {
                0
            },
            0,
        ));
    frame.render_widget(block, area);
}

/// 来源标记：↑ 本机发出、↓ 来自别的设备、· 还不知道（没拿到状态）。
///
/// 判据是**来源设备 id 与本机设备 id 是否相同**。早先这里比的是 `origin == "remote"`，
/// 而 `origin` 装的是设备名（例如 `ALEXLAPTOP`），于是所有条目都被标成"本机"——
/// 快照里一眼就能看出来，现在改成按 id 比。
fn origin_marker(item: &HistorySummary, app: &App) -> &'static str {
    match &app.status {
        None => "·",
        Some(status) if item.origin_device == status.device_id => "↑",
        Some(_) => "↓",
    }
}

/// 来源描述：自己的条目说"本机"，别人的带设备名与短 id（完整 id 会把这一行撑爆）。
fn origin_label(item: &HistorySummary, app: &App) -> String {
    let mine = app
        .status
        .as_ref()
        .map(|status| item.origin_device == status.device_id)
        .unwrap_or(false);
    if mine {
        return "本机".to_string();
    }
    let name = if item.origin.is_empty() {
        "未知设备".to_string()
    } else {
        item.origin.clone()
    };
    format!("{name}（{}）", crate::app::short(&item.origin_device))
}

fn preview_text(
    preview: &cx_proto::control::HistoryPreview,
    item: &HistorySummary,
    app: &App,
) -> Text<'static> {
    let origin = origin_label(item, app);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("类型 ", Style::default().fg(Color::DarkGray)),
            Span::raw(kind_label(&preview.kind).to_string()),
            Span::styled("   大小 ", Style::default().fg(Color::DarkGray)),
            Span::raw(human_bytes(preview.bytes as u64)),
            Span::styled("   来源 ", Style::default().fg(Color::DarkGray)),
            Span::raw(origin),
        ]),
        Line::from(vec![
            Span::styled("时间 ", Style::default().fg(Color::DarkGray)),
            Span::raw(absolute_time(item.created_ms)),
            Span::styled("   哈希 ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_hash(&item.hash)),
        ]),
    ];

    if let Some(text) = &preview.text {
        lines.push(Line::from(""));
        for line in text.lines() {
            lines.push(Line::from(line.to_string()));
        }
        if preview.truncated {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "…内容较长，这里只显示前一段；cx show <序号> 可看全文",
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else if let Some(image) = &preview.image {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "这是一张图片（终端里不渲染图片）。",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            format!("尺寸信息：{image}"),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "按回车写回剪贴板。",
            Style::default().fg(Color::DarkGray),
        )));
    }
    Text::from(lines)
}

/// 显式按终端列宽包装；滚动上限与实际绘制使用同一份行。
fn wrapped_lines(text: Text<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut result = Vec::new();
    for line in text.lines {
        let mut spans = Vec::new();
        let mut used = 0;
        for span in line.spans {
            for ch in span.content.chars() {
                let columns = Line::from(ch.to_string()).width();
                if used + columns > width && used > 0 {
                    result.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                spans.push(Span::styled(ch.to_string(), span.style));
                used += columns;
            }
        }
        result.push(Line::from(spans));
    }
    result
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.filter_input {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" 过滤 ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{}▌", app.filter())),
                Span::styled(
                    "　回车 确定　Esc 取消　（匹配类型 / 来源 / 对象名 / 哈希 / 已加载的内容摘要）",
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            area,
        );
        return;
    }

    // 页脚只放"下一步最可能用到的键"；宽终端才补全，细节都在 ? 帮助页里
    let hint = if app.show_help {
        " 任意键返回　q 退出 "
    } else if app.filter_active() {
        if area.width >= 84 {
            " 回车 写回剪贴板　/ 改过滤　Esc 清过滤　J/K 滚动　? 帮助　q 退出 "
        } else {
            " 回车 写回剪贴板　Esc 清过滤　? 帮助 "
        }
    } else if area.width >= 100 {
        " 回车 写回剪贴板　空格 暂停　d 删除　b 后端管理　/ 过滤　? 帮助　q 退出 "
    } else {
        " 回车 写回剪贴板　d 删除　b 后端管理　/ 过滤　? 帮助 "
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

/// 添加后端表单：像网页表单一样，一屏内看到类型、全部字段与按钮。
///
/// 布局是"标签列 + 输入框列"，焦点行用高亮标出；密文字段只回显 ●；
/// 提交失败时在字段右侧就地标红，并在下方给一行整体提示。
fn draw_wizard(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let popup = area;
    app.modal_rect = popup;
    frame.render_widget(Clear, popup);
    let inner = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let Some(form) = app.wizard.as_mut() else {
        return;
    };

    // 页面铺满整屏，但表单本身是一列**居中定宽**的内容：宽终端上标签列与输入列
    // 不会被拉成两米宽，读起来才像一张表。
    let content_w = inner.width.min(76);
    let content = Rect::new(
        inner.x + inner.width.saturating_sub(content_w) / 2,
        inner.y,
        content_w,
        inner.height,
    );

    // 标签列宽度 = 最宽的那个标签（含必填星号）+ 1 列间隔；值列占剩下的全部。
    let label_w = form
        .fields()
        .iter()
        .map(|field| display_width(field.label) + usize::from(field.required()))
        .max()
        .unwrap_or(8)
        + 1;
    // 极窄（内容区 <46 列）时改成"标签一行、值一行"，否则同行排法只剩不到一半给值。
    let stacked = (content_w as usize) < 46;
    let value_w = if stacked {
        (content_w as usize).saturating_sub(2)
    } else {
        (content_w as usize).saturating_sub(label_w)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<(usize, u16, u16, Focus)> = Vec::new();
    let mut kinds = Vec::new();
    let mut kind_spans = Vec::new();
    let mut x = 0u16;
    for (index, kind) in WizardKind::ALL.iter().enumerate() {
        let label = format!(
            " {}{} ",
            if index == form.kind_index() {
                "◉"
            } else {
                "○"
            },
            kind.label()
        );
        let width = display_width(&label) as u16;
        let style = if index == form.kind_index() {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        kind_spans.push(Span::styled(label, style));
        kinds.push((x, width, index));
        x = x.saturating_add(width);
    }
    lines.push(Line::from(kind_spans));
    lines.push(Line::from(Span::styled(
        clamp_width_value(form.kind().hint().to_string(), content_w as usize),
        Style::default().fg(Color::DarkGray),
    )));
    if form.kind() == WizardKind::Webdav {
        lines.push(Line::from(Span::styled(
            if form.value(2).trim().is_empty() {
                "认证：匿名（用户名与口令留空）"
            } else {
                "认证：账号口令（口令原样保存）"
            },
            Style::default().fg(Color::DarkGray),
        )));
    }
    let field_start = lines.len();
    let mut field_rows: Vec<usize> = Vec::with_capacity(form.fields().len());
    for (index, field) in form.fields().iter().enumerate() {
        let focused = form.focused_field() == Some(index);
        let invalid = form.is_invalid(index);
        let anonymous = form.kind() == WizardKind::Webdav
            && field.label == "口令"
            && form.value(2).trim().is_empty();
        let label = format!(
            "{}{}",
            field.label,
            if field.required() && !anonymous {
                "*"
            } else {
                ""
            }
        );
        let room = value_w.saturating_sub(2);
        let raw = form.value(index);
        let shown = if field.is_choice() {
            // 选择题：`‹ 自动 ›`，箭头明确表示左右能拨（只回显中文字面，配置值是英文）
            let choice = field.choice_label(raw).unwrap_or("自定义");
            if focused {
                format!("‹ {choice} ›")
            } else {
                choice.to_string()
            }
        } else if field.secret {
            "●".repeat(raw.chars().count().min(room))
        } else {
            input_tail(raw, room)
        };
        let shown = if focused {
            format!("{shown}▏")
        } else if shown.is_empty() {
            "（未填）".to_string()
        } else {
            shown
        };
        // 值单元格：左侧留一格内边距，右侧补齐到值列宽度——聚焦整行高亮时
        // 看上去就是一个输入框，未聚焦时也保持列对齐。
        let value_style = if invalid {
            Style::default().fg(Color::White).bg(Color::Red)
        } else if focused {
            Style::default().fg(Color::White).bg(Color::Indexed(238))
        } else {
            Style::default().fg(Color::White)
        };
        let pad = value_w.saturating_sub(1 + display_width(&shown));
        let cell = Span::styled(format!(" {shown}{}", " ".repeat(pad)), value_style);
        let label_style = if invalid {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Cyan)
        };
        field_rows.push(lines.len());
        if stacked {
            hits.push((lines.len(), 0, content_w, Focus::Field(index)));
            lines.push(Line::from(Span::styled(label, label_style)));
            hits.push((lines.len(), 0, content_w, Focus::Field(index)));
            lines.push(Line::from(cell));
        } else {
            // 整行都能点：点标签或点值都聚焦这个字段
            hits.push((lines.len(), 0, content_w, Focus::Field(index)));
            lines.push(Line::from(vec![
                Span::styled(pad_display(&label, label_w), label_style),
                cell,
            ]));
        }
    }

    let button_line = lines.len();
    let save = " [ 保存并测试 ] ";
    let cancel = " [ 取消 ] ";
    let save_width = display_width(save) as u16;
    let cancel_width = display_width(cancel) as u16;
    // 按钮与值列左对齐（stacked 时顶格），看起来仍是一张表
    let indent = if stacked { 0 } else { label_w as u16 };
    hits.push((button_line, indent, save_width, Focus::Save));
    hits.push((
        button_line,
        indent.saturating_add(save_width).saturating_add(2),
        cancel_width,
        Focus::Cancel,
    ));
    let mut button_spans = Vec::new();
    if indent > 0 {
        button_spans.push(Span::raw(" ".repeat(indent as usize)));
    }
    button_spans.push(Span::styled(
        save,
        if form.focus() == Focus::Save {
            Style::default().fg(Color::Black).bg(Color::Green)
        } else {
            Style::default().fg(Color::Green)
        },
    ));
    button_spans.push(Span::raw("  "));
    button_spans.push(Span::styled(
        cancel,
        if form.focus() == Focus::Cancel {
            Style::default().fg(Color::Black).bg(Color::Gray)
        } else {
            Style::default().fg(Color::Gray)
        },
    ));
    lines.push(Line::from(button_spans));

    // 错误与当前字段帮助可滚动，不能挤掉字段/按钮。
    if let Some(message) = &form.message {
        lines.extend(wrapped_lines(
            Text::from(Line::from(Span::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            ))),
            content_w,
        ));
    }
    if let Some(index) = form.focused_field() {
        lines.extend(wrapped_lines(
            Text::from(Line::from(Span::styled(
                form.fields()[index].help.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            content_w,
        ));
    }
    lines.extend(wrapped_lines(
        Text::from("Tab/↑↓ 焦点　←→ 切换类型/选项　Ctrl+Y 示例　Enter 激活　Esc 取消"),
        content_w,
    ));

    // 定宽内容列是水平居中的：文本本身也要整体右移（命中矩形用的是 content.x，
    // 只改矩形不改文字的话，点到的位置和看到的位置会错开）。
    let shift = content.x.saturating_sub(inner.x) as usize;
    if shift > 0 {
        let pad = " ".repeat(shift);
        lines = lines
            .into_iter()
            .map(|line| {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(line.spans);
                Line::from(spans)
            })
            .collect();
    }

    let focus_line = match form.focus() {
        Focus::Kind => 0,
        Focus::Field(index) => field_rows.get(index).copied().unwrap_or(field_start),
        Focus::Save | Focus::Cancel => button_line,
    };
    form.viewport(content.height, lines.len(), focus_line);
    let scroll = form.scroll;
    for (row, x, width, focus) in hits {
        if row >= scroll as usize
            && row < scroll as usize + content.height as usize
            && x < content.width
        {
            app.wizard_hits.push((
                Rect::new(
                    content.x + x,
                    content.y + (row - scroll as usize) as u16,
                    width.min(content.width - x),
                    1,
                ),
                focus,
            ));
        }
    }
    if scroll == 0 && content.height > 0 {
        for (x, width, index) in kinds {
            if x < content.width {
                app.wizard_kinds.push((
                    Rect::new(content.x + x, content.y, width.min(content.width - x), 1),
                    index,
                ));
            }
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 添加后端 ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .scroll((scroll, 0)),
        popup,
    );
}
/// 输入框采用末尾视口；按显示列计算，不截断 UTF-8。
fn input_tail(text: &str, width: usize) -> String {
    let mut result = String::new();
    for ch in text.chars().rev() {
        let candidate = format!("{ch}{result}");
        if display_width(&candidate) > width {
            break;
        }
        result = candidate;
    }
    result
}
/// 后端管理面板：一张表（后端健康 + 开关）+ doctor 里 TUI 看不到的几项。
///
/// 为什么是表：原来是"每个后端一段文字"，看得出行、看不出**列**——想比较"哪个在失败、
/// 哪个被停用了、有几个后端"得逐行读。表格把名称 / 类型 / 目标 / 收发计数 / 状态 / 开关
/// 对齐，一眼扫一列就够了。停用·启用、移除、添加也都收在同一个面板里：
/// 改配置不必退出界面再进来。
fn draw_panel(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let popup = area;
    app.modal_rect = popup;
    frame.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title(" 后端管理 ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 4 || inner.width < 20 {
        return;
    }

    let backends: &[BackendStatus] = app
        .status
        .as_ref()
        .map(|status| status.backends.as_slice())
        .unwrap_or(&[]);
    // 窄屏（<62 列）砍掉"类型 / 目标 / 收发计数"三列：保留名称、状态与开关，
    // 剩下的靠选中行下面那一行错误/提示补足。
    let narrow = inner.width < 62;

    // 有后端才画表格：表头 + 每后端一行；诊断区永远保留最少两行。
    let table_height = if backends.is_empty() {
        0
    } else {
        (backends.len() as u16 + 1).min(inner.height.saturating_sub(4))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(table_height),
            Constraint::Length(1), // 最近一次操作的结果 / 选中后端的错误
            Constraint::Min(2),    // 连接与安全（可滚动）
            Constraint::Length(1), // 按键
        ])
        .split(inner);

    if !backends.is_empty() {
        let selected = app.panel_backend();
        draw_backend_table(frame, chunks[0], backends, selected, narrow);
    }

    // 结果行：优先显示"刚做过什么"，其次显示选中后端的错误。
    // 选中的那个后端没出错、但别的后端在失败时，也要把那条错误露出来——
    // 否则"表格里显示失败 4 次"却看不到原因，还得挨个选中去找。
    let selected_backend = backends.get(app.panel_backend());
    let error_backend = selected_backend
        .filter(|backend| backend.last_error.is_some())
        .or_else(|| backends.iter().find(|backend| backend.last_error.is_some()));
    let result_line = match &app.notice {
        Some(notice) => Some(Line::from(Span::styled(
            format!(" {}", notice.text),
            if notice.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Cyan)
            },
        ))),
        None => error_backend.and_then(|backend| {
            backend.last_error.as_ref().map(|error| {
                Line::from(Span::styled(
                    format!(" {}：{error}", backend.name),
                    Style::default().fg(Color::Red),
                ))
            })
        }),
    };
    if let Some(line) = result_line {
        frame.render_widget(Paragraph::new(line), chunks[1]);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    match &app.status {
        None => lines.push(Line::from(Span::styled(
            "还没连上守护进程，看不到后端状态。",
            Style::default().fg(Color::Red),
        ))),
        Some(status) if status.backends.is_empty() => {
            // 首次配置：只留三步，其余说明在命令与文档里。这一页要整屏看，不是拿来堆教程的。
            lines.push(Line::from(Span::styled(
                "还没有同步后端。按 a 填表添加：",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(
                " 1) 按 a 选一种：自建 cx-server / WebDAV / S3（含阿里云 OSS、MinIO、R2）",
            ));
            lines.push(Line::from(
                " 2) 填桶（或地址）与凭据：密钥密文输入，保存前会真的连一次做自测",
            ));
            lines.push(Line::from(
                " 3) 加完回到本页按 空格 启用；首次同步还要建加密空间：",
            ));
            lines.push(Line::from(Span::styled(
                "    cx space init --passphrase-stdin && cx space publish <后端名>",
                Style::default().fg(Color::Cyan),
            )));
        }
        // 有后端时上面的表格已经把每个后端都说清楚了，这里不再重复
        Some(_) => {}
    }

    // 有后端但本机还没有加密空间：这是唯一必须动手的配置步骤，直接在面板里给出按键。
    // （诊断项来自 doctor，管道里没有"空间是否已建"这条，所以单独判断 status）
    if let Some(status) = &app.status
        && !status.backends.is_empty()
        && status.space_id.is_none()
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "需要处理",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled(" ▲ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                "还没有加密空间",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("：按 s 在本机建一个，或按 i 用密钥加入（口令/密钥会自动验证）"),
        ]));
    }

    // 诊断区只列「需要你动手」的项：一切正常就不占任何版面。
    // 剪贴板后端、监听方式、历史存储这类是给排障的人看的，界面上没有意义。
    const NOISE: [&str; 3] = ["剪贴板", "监听方式", "历史存储"];
    if let Some(doctor) = &app.doctor {
        let problems: Vec<_> = doctor
            .checks
            .iter()
            .filter(|check| !check.ok && !NOISE.contains(&check.name.as_str()))
            .collect();
        if !problems.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "需要处理",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            for check in problems {
                lines.push(Line::from(vec![
                    Span::styled(" ▲ ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("{}：", check.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(check.detail.clone()),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "  完整检查：cx doctor",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if let Some(error) = &app.doctor_error {
        lines.push(Line::from(Span::styled(
            format!("诊断读取失败：{error}；按 r 重试"),
            Style::default().fg(Color::Red),
        )));
    } else if app.doctor.is_none() {
        lines.push(Line::from(Span::styled(
            "（正在读取诊断…；按 r 重试）",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // 诊断区单独渲染：它可能很长（引导 + 诊断），只有它需要滚动，
    // 表格与按键提示固定不动——滚一下把表头滚没了才是最难受的。
    let lines = wrapped_lines(Text::from(lines), chunks[2].width);
    app.panel_max_scroll = lines
        .len()
        .saturating_sub(chunks[2].height as usize)
        .min(u16::MAX as usize) as u16;
    app.clamp_panel_scroll();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((app.panel_scroll(), 0)),
        chunks[2],
    );

    let keys = if chunks[3].width < 76 {
        // 窄屏只留最常用的四个键：空格开关、a 添加、x 移除、Esc 关闭
        "  ↑↓ 选择　空格 开关　a 添加　x 移除　Esc 关闭"
    } else {
        "  ↑↓ 选后端　空格 开关　x 移除　a 添加　s 建空间　i 加入　e 导出密钥　Esc 返回"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keys,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[3],
    );
}

/// 表格：名称 / 类型 / 目标 / 收发计数 / 状态 / 开关。
///
/// 只有被选中那一行是粗体 + ▶，颜色只用于状态列与开关列：
/// 整行上底色会让"状态"这一列的颜色失效，反而不容易扫。
fn draw_backend_table(
    frame: &mut Frame<'_>,
    area: Rect,
    backends: &[BackendStatus],
    selected: usize,
    narrow: bool,
) {
    let header_cells: Vec<&str> = if narrow {
        vec!["", "名称", "状态", "开关"]
    } else {
        vec!["", "名称", "类型", "目标", "发布/接收", "状态", "开关"]
    };
    let header = Row::new(header_cells).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row<'_>> = backends
        .iter()
        .enumerate()
        .map(|(index, backend)| {
            let is_selected = index == selected;
            let (state_text, state_style) = backend_state(backend);
            let mut cells: Vec<Cell<'_>> = vec![
                Cell::from(Span::styled(
                    if is_selected { "▶" } else { " " },
                    Style::default().fg(Color::Cyan),
                )),
                Cell::from(Span::styled(
                    backend.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            ];
            if !narrow {
                cells.push(Cell::from(Span::raw(backend.kind.clone())));
                // 地址 / 桶 @ 端点 / 空间：配了两个同类后端时才分得清谁是谁
                cells.push(Cell::from(Span::styled(
                    clamp_width_value(backend.detail.clone().unwrap_or_default(), 30),
                    Style::default().fg(Color::DarkGray),
                )));
                cells.push(Cell::from(Span::raw(format!(
                    "{}/{}",
                    backend.published, backend.applied
                ))));
            }
            cells.push(Cell::from(Span::styled(state_text, state_style)));
            cells.push(Cell::from(Span::styled(
                if backend.enabled { "开" } else { "关" },
                if backend.enabled {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )));
            Row::new(cells).style(if is_selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();

    let widths: Vec<Constraint> = if narrow {
        vec![
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Min(12),
            Constraint::Length(4),
        ]
    } else {
        vec![
            Constraint::Length(2),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Min(10),
            Constraint::Length(9),
            Constraint::Min(12),
            Constraint::Length(4),
        ]
    };
    frame.render_widget(Table::new(rows, widths).header(header), area);
}

/// 一个后端在表格"状态"列里的文字与颜色（形状与颜色一起变，不只靠颜色区分）。
fn backend_state(backend: &BackendStatus) -> (String, Style) {
    if !backend.enabled {
        return ("已停用".to_string(), Style::default().fg(Color::DarkGray));
    }
    if backend.failures > 0 {
        return (
            format!("失败 {} 次，{}后重试", backend.failures, retry_in(backend)),
            Style::default().fg(Color::Red),
        );
    }
    match backend.last_ok_ms {
        Some(ms) => (
            format!("最近成功 {}", relative_time(ms)),
            Style::default().fg(Color::Green),
        ),
        None => ("还没成功过".to_string(), Style::default().fg(Color::Yellow)),
    }
}

/// 退避还剩多久。
fn retry_in(backend: &cx_proto::control::BackendStatus) -> String {
    let now = cx_core::now_ms();
    if backend.next_attempt_ms <= now {
        "立即".to_string()
    } else {
        let seconds = (backend.next_attempt_ms - now) / 1000;
        if seconds < 60 {
            format!("{seconds} 秒")
        } else {
            format!("{} 分钟", seconds / 60)
        }
    }
}
fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let popup = area;
    app.modal_rect = popup;
    frame.render_widget(Clear, popup);
    // 分组压行：同一类键写在一行，帮助页从 29 行收到 11 行（宽终端仍然好读）
    let lines = vec![
        Line::from(Span::styled(
            "快捷键",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(" ↑/k ↓/j 选择　PgUp/PgDn 翻页　Home/g 最新　End/G 最旧"),
        Line::from(" 回车 写回剪贴板　d 删除（先确认）　/ 过滤　Esc 清过滤"),
        Line::from(" J/K 预览滚动　Ctrl+D/U 预览翻页　0 回到开头"),
        Line::from(" 空格 暂停/恢复　p 立即发布　r 刷新　R 重读配置"),
        Line::from(" b 后端管理（a 添加　空格 开关　x 移除）　? 帮助　q 退出"),
        Line::from(" 鼠标 滚轮翻列表，左键点行即选中；表单里点行即聚焦"),
        Line::from(""),
        Line::from(Span::styled(
            " ↑ 本机发出，↓ 来自别的设备，· 状态未知",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let lines = wrapped_lines(Text::from(lines), popup.width.saturating_sub(2));
    app.help_max_scroll = lines
        .len()
        .saturating_sub(popup.height.saturating_sub(2) as usize)
        .min(u16::MAX as usize) as u16;
    app.help_scroll = app.help_scroll.min(app.help_max_scroll);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 帮助（↑↓/PgUp/PgDn/滚轮滚动，Esc 返回） "),
            )
            .scroll((app.help_scroll, 0)),
        popup,
    );
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// 相对时间：列表里"多久以前"比绝对时间好读。
pub(crate) fn relative_time(ms: u64) -> String {
    let now = cx_core::now_ms();
    if ms == 0 || ms > now {
        return "--".to_string();
    }
    let seconds = (now - ms) / 1000;
    match seconds {
        0..=9 => "刚刚".to_string(),
        10..=59 => format!("{seconds} 秒前"),
        60..=3599 => format!("{} 分钟前", seconds / 60),
        3600..=86_399 => format!("{} 小时前", seconds / 3600),
        86_400..=604_799 => format!("{} 天前", seconds / 86_400),
        _ => absolute_time(ms),
    }
}

/// 绝对时间（UTC，`MM-DD HH:MM UTC`）。
fn absolute_time(ms: u64) -> String {
    let total_seconds = (ms / 1000) as i64;
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = cx_proto::civil::civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let _ = year;
    format!("{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KB", value / KIB)
    } else {
        format!("{:.1} MB", value / (KIB * KIB))
    }
}

/// 文本的显示宽度（CJK 算两列）。
/// 按"显示列"右侧补空格（CJK 占两列，`format!` 的宽度补不齐）。
fn pad_display(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    let mut used = display_width(text);
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}
fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

/// 按显示宽度截断（超过就加 …）。CJK 按两列计算，所以不会把中文截成半宽。
fn clamp_width_value(text: String, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(&text) <= max {
        return text;
    }
    // 二分找最长的可容纳前缀（宽度按列算，避免中文被截成两半）
    let chars: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        // +1 是给末尾的省略号留一列
        if display_width(&candidate) < max {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut cut: String = chars[..low.min(chars.len())].iter().collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::client::UiEvent;
    use cx_proto::control::{HistoryPreview, HistorySummary, StatusReport};

    fn item(position: usize, origin: &str) -> HistorySummary {
        HistorySummary {
            position,
            name: format!("1759323998000-ab12cd34-9f3c1e77-{position}"),
            kind: "text".to_string(),
            created_ms: cx_core::now_ms() - 120_000,
            bytes: 1234,
            hash: "a1b2c3d4".repeat(8),
            origin: origin.to_string(),
            origin_device: "ab12cd34".to_string(),
        }
    }

    /// 来源标记必须按"来源设备 id 是否等于本机"判断。
    ///
    /// 这里曾经比的是 `origin == "remote"`，而 origin 装的是设备名，
    /// 于是所有条目都被标成"本机"——是个一眼看不出来、但会误导人的错误。
    #[test]
    fn 来源标记按设备_id_判断而不是设备名() {
        let app = app_with(1);
        let device_id = app.status.as_ref().unwrap().device_id.clone();

        let mut mine = item(0, "ALEXLAPTOP");
        mine.origin_device = device_id.clone();
        assert_eq!(origin_marker(&mine, &app), "↑", "来源就是本机");

        let mut remote = item(0, "ALEXLAPTOP");
        remote.origin_device = "ffffffffffffffff".to_string();
        assert_eq!(origin_marker(&remote, &app), "↓", "来源是别的设备");
        assert!(origin_label(&remote, &app).contains("ALEXLAPTOP"));
        assert!(
            origin_label(&remote, &app).contains("ffffffff"),
            "别人的条目要带短设备 id"
        );
        assert_eq!(origin_label(&mine, &app), "本机");

        // 没拿到状态时不下结论，标成"未知"而不是猜"本机"
        let blind = App::new();
        assert_eq!(origin_marker(&remote, &blind), "·");
    }

    fn loaded(position: usize, text: &str) -> UiEvent {
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

    fn app_with(count: usize) -> App {
        let mut app = App::new();
        app.apply(UiEvent::History(
            (0..count).map(|i| item(i, "cli")).collect(),
        ));
        app.apply(UiEvent::Status(Box::new(status(false))));
        app
    }

    /// 画一帧并返回屏幕文本。
    fn render(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("测试终端应当能创建");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("渲染不应失败");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn has(screen: &str, needle: &str) -> bool {
        compact(screen).contains(&compact(needle))
    }

    #[test]
    fn 画面上能看到状态与历史() {
        let mut app = app_with(3);
        app.apply(loaded(0, "第一条内容"));
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "ClipboardX"), "{screen}");
        assert!(has(&screen, "笔记本"), "{screen}");
        assert!(has(&screen, "历史"), "{screen}");
        assert!(has(&screen, "内容预览"), "{screen}");
    }

    #[test]
    fn 行内直接显示内容摘要而不是只有哈希() {
        let mut app = app_with(3);
        app.apply(loaded(0, "这是一段可以直接看到的内容"));
        app.apply(loaded(1, "第二条内容"));
        let screen = render(&mut app, 140, 30);
        assert!(
            has(&screen, "这是一段可以直接看到的内容"),
            "列表行里应当能看到内容摘要：{screen}"
        );
        assert!(has(&screen, "第二条内容"), "{screen}");
        // 还没加载的那条显示占位符
        assert!(has(&screen, "…"), "{screen}");
    }

    #[test]
    fn 未加载的行显示占位符_加载失败的行标出原因() {
        let mut app = app_with(2);
        app.apply(UiEvent::Preview {
            name: "1759323998000-ab12cd34-9f3c1e77-0".to_string(),
            result: Err("内容已过期".to_string()),
        });
        let screen = render(&mut app, 140, 30);
        assert!(has(&screen, "读取失败"), "{screen}");
        assert!(has(&screen, "读取预览失败：内容已过期"), "{screen}");
    }

    #[test]
    fn 列表标题显示光标位置与总数() {
        let mut app = app_with(12);
        app.down();
        app.down();
        let screen = render(&mut app, 140, 30);
        assert!(has(&screen, "3/12"), "应当显示第几条/共几条：{screen}");
        assert!(has(&screen, "12 条"), "{screen}");
    }

    #[test]
    fn 过滤生效时标题与页脚都提示() {
        let mut app = app_with(4);
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::NONE,
        ));
        for ch in "图片".chars() {
            app.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let screen = render(&mut app, 140, 30);
        assert!(has(&screen, "过滤"), "{screen}");
        assert!(has(&screen, "0/4"), "{screen}");
        assert!(
            has(&screen, "Esc 取消"),
            "输入态页脚应当说明怎么退出：{screen}"
        );
    }

    #[test]
    fn 过滤无结果时预览区给出出路而不是空白() {
        let mut app = app_with(3);
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::NONE,
        ));
        for ch in "image".chars() {
            app.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let screen = render(&mut app, 140, 30);
        assert!(has(&screen, "没有匹配到内容"), "{screen}");
        assert!(has(&screen, "Esc 清除过滤"), "{screen}");
    }

    #[test]
    fn 暂停时状态栏变黄且显示已暂停() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(true))));
        let screen = render(&mut app, 120, 24);
        assert!(has(&screen, "已暂停"), "{screen}");
    }

    #[test]
    fn 未连接时给出明确提示() {
        let mut app = App::new();
        let screen = render(&mut app, 120, 24);
        assert!(has(&screen, "未连接守护进程"), "{screen}");
        assert!(has(&screen, "历史里还没有内容"), "{screen}");
    }

    #[test]
    fn 相对时间比绝对时间更好读() {
        let now = cx_core::now_ms();
        assert_eq!(relative_time(now - 3_000), "刚刚");
        assert_eq!(relative_time(now - 120_000), "2 分钟前");
        assert_eq!(relative_time(now - 7_200_000), "2 小时前");
        assert_eq!(relative_time(now - 3 * 86_400_000), "3 天前");
        // 太久以前退回绝对时间
        assert!(relative_time(now - 40 * 86_400_000).contains('-'));
        // 时钟异常（未来时间）不该 panic
        assert_eq!(relative_time(now + 60_000), "--");
        assert_eq!(relative_time(0), "--");
    }

    #[test]
    fn 中文按两列截断且不超宽() {
        let text = "这是一段很长的中文内容需要被截断".to_string();
        let cut = clamp_width_value(text.clone(), 20);
        assert!(
            display_width(&cut) <= 20,
            "截断后宽度 {} 超了",
            display_width(&cut)
        );
        assert!(cut.ends_with('…'), "{cut}");
        // 够宽时原样返回
        assert_eq!(clamp_width_value(text.clone(), 200), text);
        // 宽度为 0 时返回空串而不是 panic
        assert_eq!(clamp_width_value(text, 0), "");
    }

    #[test]
    fn 图片不渲染但给出说明与尺寸() {
        let mut app = app_with(1);
        app.apply(UiEvent::Preview {
            name: "1759323998000-ab12cd34-9f3c1e77-0".to_string(),
            result: Ok(Box::new(HistoryPreview {
                position: 0,
                kind: "image".to_string(),
                text: None,
                image: Some("1920x1080 PNG".to_string()),
                truncated: false,
                bytes: 4096,
            })),
        });
        let screen = render(&mut app, 130, 30);
        assert!(has(&screen, "图片"), "{screen}");
        assert!(has(&screen, "1920x1080"), "{screen}");
        assert!(has(&screen, "按回车写回剪贴板"), "{screen}");
    }

    #[test]
    fn 窄终端改成上下布局仍然可读() {
        let mut app = app_with(3);
        app.apply(loaded(0, "窄屏下也要能看到内容"));
        let narrow = render(&mut app, 70, 30);
        assert!(has(&narrow, "窄屏下也要能看到内容"), "{narrow}");
        assert!(has(&narrow, "内容预览"), "{narrow}");
        // 上下布局：标题在上半屏，预览在下半屏。
        // 注意要比"去掉空白"之后的文本：TestBackend 给 CJK 宽字符插了占位空格，
        // 直接 contains("历史") 是匹配不上的（这个坑踩过一次了）。
        let lines: Vec<&str> = narrow.lines().collect();
        let list_y = lines
            .iter()
            .position(|l| compact(l).contains("0=最新"))
            .unwrap_or(0);
        let preview_y = lines
            .iter()
            .position(|l| compact(l).contains("内容预览"))
            .unwrap_or(0);
        assert!(preview_y > list_y, "窄屏应当是上下排列：\n{narrow}");
    }

    #[test]
    fn 宽终端保持左右布局() {
        let mut app = app_with(3);
        let screen = render(&mut app, 140, 30);
        let lines: Vec<&str> = screen.lines().collect();
        let list_y = lines
            .iter()
            .position(|l| compact(l).contains("历史") && l.contains('┌'))
            .unwrap_or(99);
        let preview_y = lines
            .iter()
            .position(|l| compact(l).contains("内容预览"))
            .unwrap_or(99);
        assert_eq!(list_y, preview_y, "宽屏应当左右并排：\n{screen}");
    }

    #[test]
    fn 预览滚动后标题显示偏移量与回位提示() {
        let mut app = app_with(1);
        let long: String = (1..=200).map(|i| format!("第 {i} 行\n")).collect();
        app.apply(loaded(0, &long));
        app.set_viewport_rows(5);
        app.preview_scroll_by(3);
        let screen = render(&mut app, 130, 30);
        assert!(has(&screen, "已下滚 3 行"), "{screen}");
        assert!(has(&screen, "0 回到开头"), "{screen}");
    }

    #[test]
    fn 帮助页列出全部快捷键() {
        let mut app = app_with(1);
        app.show_help = true;
        let screen = render(&mut app, 130, 40);
        for needle in [
            "快捷键",
            "回车",
            "空格",
            "PgUp",
            "过滤",
            "Ctrl+D",
            "暂停",
            "退出",
        ] {
            assert!(has(&screen, needle), "帮助页缺少 {needle}：\n{screen}");
        }
        // 说明性长句已删：帮助页只留按键，不再解释"摘要按需加载"的实现细节
        assert!(
            !has(&screen, "按需加载"),
            "帮助页不该再塞实现细节：{screen}"
        );
    }

    #[test]
    fn 小终端也不会崩() {
        for (width, height) in [(20u16, 6u16), (40, 8), (60, 10), (200, 60)] {
            let mut app = app_with(5);
            let _ = render(&mut app, width, height);
        }
    }

    #[test]
    fn 删除确认浮层说明影响范围与按键() {
        let mut app = app_with(3);
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('d'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "确认删除"), "{screen}");
        assert!(has(&screen, "删除第 0 条？"), "{screen}");
        assert!(
            has(&screen, "其他设备上的副本不受影响"),
            "要说明只动本机：{screen}"
        );
        assert!(has(&screen, "y / 回车"), "{screen}");
        assert!(has(&screen, "取消"), "{screen}");
    }

    #[test]
    fn 页脚与帮助都提到删除键() {
        let mut app = app_with(1);
        let screen = render(&mut app, 140, 24);
        assert!(has(&screen, "d 删除"), "页脚应当提到 d：{screen}");

        let mut help = app_with(1);
        help.show_help = true;
        let screen = render(&mut help, 130, 40);
        assert!(has(&screen, "删除"), "{screen}");
        assert!(has(&screen, "先确认"), "帮助里要说明需要确认：{screen}");
    }
    #[test]
    fn 渲染后用真实坐标点击能选中对应的行() {
        let mut app = app_with(6);
        app.apply(loaded(0, "第一条内容"));
        app.apply(loaded(2, "第三条内容"));
        let screen = render(&mut app, 120, 30);
        let lines: Vec<&str> = screen.lines().collect();

        // 不写死行号：从画面上找到那一行再点它——头部高度会随提示行变化，
        // 写死行号只会让测试跟着布局一起漂（第一版就是这么错的）。
        let row_of = |needle: &str| {
            lines
                .iter()
                .position(|line| compact(line).contains(needle))
                .map(|row| row as u16)
                .unwrap_or_else(|| panic!("画面上找不到 {needle}：\n{screen}"))
        };

        let first = row_of("第一条内容");
        assert_eq!(app.click(first), Some(0));
        assert_eq!(app.selected_position(), Some(0));

        let third = row_of("第三条内容");
        assert_eq!(app.click(third), Some(2));
        assert_eq!(app.selected_position(), Some(2));

        // 点头部（标题那一行）不该有反应
        app.home();
        assert_eq!(app.click(0), None);
        assert_eq!(app.selected_position(), Some(0));
    }
    #[test]
    fn 列表滚动后点击仍然对得上() {
        let mut app = app_with(40);
        app.set_viewport_rows(6);
        // 先移到很下面，让 ratatui 不得不滚动列表
        for _ in 0..30 {
            app.down();
        }
        let _ = render(&mut app, 120, 12); // 小屏：只能显示几行，必然滚动
        // 选中项所在的那一行，点它应当仍然是当前这条
        let before = app.cursor;
        let _ = app.click(3); // 屏幕第一行
        assert!(
            app.cursor <= before,
            "点屏幕上第一行应当选中它（滚动过之后它就是可见范围的第一条）"
        );
    }
    #[test]
    fn 列表行显示远端条目的来源设备_本机的则不显示() {
        let mut app = app_with(2);
        app.apply(loaded(0, "远端来的内容"));
        app.apply(loaded(1, "本机的内容"));

        // 把第 0 条改成"来自别的设备"，第 1 条保持本机
        let device_id = app.status.as_ref().unwrap().device_id.clone();
        let mut remote = item(0, "ALEXLAPTOP");
        remote.name = "1759323998000-ab12cd34-9f3c1e77-0".to_string();
        remote.origin_device = "ffffffffffffffff".to_string();
        let mut mine = item(1, "ALEXLAPTOP");
        mine.name = "1759323998000-ab12cd34-9f3c1e77-1".to_string();
        mine.origin_device = device_id;
        app.apply(UiEvent::History(vec![remote, mine]));

        let screen = render(&mut app, 140, 30);
        assert!(has(&screen, "远端来的内容"), "{screen}");
        // 设备名出现在行里（远端那条）
        assert!(
            has(&screen, "ALEXLAPTOP"),
            "远端条目应当显示来源设备名：{screen}"
        );
    }

    #[test]
    fn 列表太窄时不显示来源设备名() {
        let mut app = app_with(1);
        let mut remote = item(0, "ALEXLAPTOP");
        remote.origin_device = "ffffffffffffffff".to_string();
        app.apply(UiEvent::History(vec![remote]));
        app.apply(loaded(0, "内容"));

        // 列表内容宽 = 终端宽 - 4，因此 40 列的终端才真正低于阈值。
        // 断言要看**列表那一行**：预览面板本来就会显示来源设备名，
        // 断言整屏会把预览里的也算上（第一版就是这么错的）。
        let screen = render(&mut app, 40, 30);
        let row = screen
            .lines()
            .find(|line| compact(line).contains("内容"))
            .unwrap_or_default();
        assert!(
            !compact(row).contains("ALEXLAPTOP"),
            "窄屏下列表行不该显示设备名：{row:?}\n{screen}"
        );
        assert!(compact(row).contains("内容"), "内容摘要仍要看得见：{row:?}");
    }

    #[test]
    fn 设备名过长时截断且不超宽() {
        let mut app = app_with(1);
        let mut remote = item(0, "一台名字特别长的设备某某某某某某");
        remote.origin_device = "ffffffffffffffff".to_string();
        app.apply(UiEvent::History(vec![remote.clone()]));
        app.apply(loaded(0, "内容"));

        // 直接量这一行（不经过屏幕抓行：抓行会抓错面板，第一版就是这么错的）
        let line = row_text(&remote, &app, 80);
        let width = line.width();
        assert!(width <= 80, "行宽 {width} 超了 80");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(text.contains('…'), "长设备名应当被截断：{text}");
        // 设备名按**显示列**截断到 12 列（12 列 = 6 个汉字），不是按字符数
        assert!(text.contains("一台名字特"), "{text}");
        let source = line.spans[1].content.to_string();
        assert!(
            display_width(&source) <= 13,
            "设备名占了 {} 列",
            display_width(&source)
        );
    }
    #[test]
    fn 面板里显示后端的标识以便区分同类型后端() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [
                    { "name": "oss-a", "kind": "s3", "enabled": true, "failures": 0,
                      "next_attempt_ms": 0, "published": 1, "applied": 1,
                      "detail": "bucket-a @ https://oss-cn-hangzhou.aliyuncs.com" },
                    { "name": "oss-b", "kind": "s3", "enabled": true, "failures": 0,
                      "next_attempt_ms": 0, "published": 2, "applied": 2,
                      "detail": "bucket-b @ https://oss-cn-beijing.aliyuncs.com" }
                ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        let screen = render(&mut app, 140, 30);
        // 两个同类后端能靠标识区分开——这正是加 detail 的目的。
        // 标识在面板里会被截断到 28 列，因此断言到"足以区分"的前缀为止。
        assert!(has(&screen, "bucket-a"), "{screen}");
        assert!(has(&screen, "bucket-b"), "{screen}");
        assert!(has(&screen, "oss-cn-h"), "端点前缀应当可见：{screen}");
        assert!(has(&screen, "oss-cn-b"), "两个后端的端点要能区分：{screen}");

        // 老版本守护进程不发 detail 时也不能崩、不能显示占位垃圾
        let mut old = App::new();
        old.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [ { "name": "home", "kind": "server", "enabled": true,
                                "failures": 0, "next_attempt_ms": 0, "published": 0, "applied": 0 } ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("缺少 detail 的状态也应当能反序列化"),
        )));
        old.show_panel = true;
        let screen = render(&mut old, 140, 30);
        assert!(has(&screen, "home"), "{screen}");
        assert!(has(&screen, "后端管理"), "{screen}");
    }

    #[test]
    fn 面板里选中后端有标记且移除确认说明影响范围() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [
                    backend("home", "server", true, 0, None),
                    backend("oss", "s3", true, 0, None)
                ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "▶"), "选中的后端要有标记：{screen}");
        assert!(has(&screen, "↑↓ 选后端"), "页脚要说明怎么选：{screen}");
        assert!(
            has(&screen, "a 添加"),
            "已经有一个后端时也要说明怎么再加一个：{screen}"
        );
        assert!(has(&screen, "x 移除"), "{screen}");
        assert!(
            has(&screen, "空格 开关"),
            "要告诉用户能停用（空格就是开关）：{screen}"
        );
        assert!(has(&screen, "i 加入"), "要告诉用户能加入已有空间：{screen}");

        // 确认浮层
        app.pending_backend_remove = Some("home".to_string());
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "确认移除"), "{screen}");
        assert!(has(&screen, "移除后端 home？"), "{screen}");
        assert!(
            has(&screen, "不删除已上传到该后端的对象"),
            "要说清影响范围：{screen}"
        );
        assert!(has(&screen, "y / 回车"), "{screen}");
    }
    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn mouse(
        x: u16,
        y: u16,
        kind: crossterm::event::MouseEventKind,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn draw_回填真实列表高度用于分页与预取() {
        use crossterm::event::KeyCode;
        for (width, height) in [(80, 24), (60, 20), (120, 30)] {
            let mut app = app_with(100);
            render(&mut app, width, height);
            let rows = app.list_rect.height as usize;
            assert!(rows > 1);
            assert_eq!(app.viewport_rows(), rows);
            assert!(app.take_preview_requests().len() >= rows.min(40));
            app.on_key(key(KeyCode::PageDown));
            assert_eq!(app.cursor, rows);
        }
    }

    #[test]
    fn 长单行_cjk_末尾可达且刷新不重置阅读() {
        let mut app = app_with(1);
        app.apply(loaded(0, &format!("{}TAIL-END", "中文长行".repeat(200))));
        render(&mut app, 80, 24);
        app.preview_scroll_by(10000);
        let offset = app.preview_scroll();
        assert!(offset > 10);
        let screen = render(&mut app, 80, 24);
        assert!(has(&screen, "TAIL-END"), "{screen}");
        app.apply(UiEvent::History(vec![item(0, "cli")]));
        app.apply(UiEvent::Status(Box::new(status(false))));
        render(&mut app, 80, 24);
        assert_eq!(app.preview_scroll(), offset);
        app.forget_previews();
        render(&mut app, 80, 24);
        assert_eq!(
            app.preview_scroll(),
            offset,
            "loading must not clamp the saved anchor"
        );
        app.apply(loaded(0, &format!("{}TAIL-END", "中文长行".repeat(200))));
        render(&mut app, 80, 24);
        assert_eq!(app.preview_scroll(), offset);
        let screen = render(&mut app, 60, 20);
        assert!(
            has(&screen, "TAIL-END"),
            "resize should retain trailing anchor: {screen}"
        );
    }

    #[test]
    fn 二维鼠标命中且所有浮层都不穿透() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut app = app_with(20);
        render(&mut app, 120, 30);
        let row = app.list_rect.y + 2;
        let preview_x = app.preview_rect.x;
        app.on_mouse(mouse(
            preview_x,
            row,
            MouseEventKind::Down(MouseButton::Left),
        ));
        assert_eq!(app.cursor, 0, "预览点击不能选历史");
        app.on_mouse(mouse(
            app.list_rect.x,
            row,
            MouseEventKind::Down(MouseButton::Left),
        ));
        assert_eq!(app.cursor, 2);
        for modal in 0..5 {
            let mut app = app_with(20);
            match modal {
                0 => app.show_help = true,
                1 => app.show_panel = true,
                2 => app.wizard = Some(crate::wizard::Wizard::new()),
                3 => app.pending_delete = Some(0),
                _ => app.pending_backend_remove = Some("fake".to_string()),
            }
            render(&mut app, 120, 30);
            let x = app.list_rect.x;
            let y = app.list_rect.y + 2;
            app.on_mouse(mouse(x, y, MouseEventKind::Down(MouseButton::Left)));
            app.on_mouse(mouse(x, y, MouseEventKind::ScrollDown));
            assert_eq!(app.cursor, 0, "modal {modal} 穿透");
        }
    }

    #[test]
    fn 小终端表单焦点与输入尾部可达且鼠标取消不提交() {
        use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
        for (width, height) in [(80, 24), (60, 20), (120, 30)] {
            let mut app = app_with(1);
            app.show_panel = true; // 添加后端现在只从「后端管理」面板发起
            app.on_key(key(KeyCode::Char('a')));
            app.wizard.as_mut().unwrap().set_kind(2);
            let count = app.wizard.as_ref().unwrap().fields().len();
            for index in 0..count {
                app.wizard.as_mut().unwrap().focus_field(index);
                let screen = render(&mut app, width, height);
                assert!(
                    app.wizard_hits
                        .iter()
                        .any(|(_, focus)| *focus == Focus::Field(index)),
                    "{screen}"
                );
            }
            let endpoint = app
                .wizard
                .as_ref()
                .unwrap()
                .fields()
                .iter()
                .position(|field| field.label == "端点")
                .expect("S3 表单必须有端点字段");
            let form = app.wizard.as_mut().unwrap();
            form.focus_field(endpoint);
            for ch in format!("{}VISIBLE-END", "https://example.invalid/".repeat(10)).chars() {
                form.push_char(ch);
            }
            let screen = render(&mut app, width, height);
            assert!(has(&screen, "VISIBLE-END▏"), "{screen}");
            app.wizard.as_mut().unwrap().focus_button(false);
            render(&mut app, width, height);
            let rect = app
                .wizard_hits
                .iter()
                .find(|(_, focus)| *focus == Focus::Cancel)
                .unwrap()
                .0;
            assert_eq!(
                app.on_mouse(mouse(
                    rect.x + 1,
                    rect.y,
                    MouseEventKind::Down(MouseButton::Left)
                )),
                crate::KeyOutcome::None
            );
            assert!(app.wizard.is_none());
        }
    }

    #[test]
    fn 窄终端表单标签与值分行且值不被挤没() {
        let mut app = App::new();
        app.show_panel = true;
        app.on_key(key(crossterm::event::KeyCode::Char('a')));
        let form = app.wizard.as_mut().unwrap();
        form.set_kind(2);
        let endpoint = form
            .fields()
            .iter()
            .position(|field| field.label == "端点")
            .expect("S3 表单必须有端点字段");
        form.focus_field(endpoint);
        for ch in "https://example.invalid/end".chars() {
            form.push_char(ch);
        }
        // 44 列：表单内容区不到 46 列，走"标签一行、值一行"
        let screen = render(&mut app, 44, 20);
        let lines: Vec<String> = screen.lines().map(compact).collect();
        let label_line = lines
            .iter()
            .position(|line| line.contains("端点"))
            .unwrap_or_else(|| panic!("找不到端点标签行：{screen}"));
        assert!(
            !lines[label_line].contains("example.invalid"),
            "标签行不该再挤着值（同行排法在窄屏看不到自己打了什么）：{screen}"
        );
        assert!(
            lines
                .get(label_line + 1)
                .is_some_and(|line| line.contains("example.invalid/end")),
            "值应当独占下一行，且末尾可见：{screen}"
        );
    }

    #[test]
    fn 小终端帮助末尾可达且不关闭() {
        use crossterm::event::KeyCode;
        for (width, height) in [(80, 24), (60, 20), (120, 30)] {
            let mut app = app_with(1);
            app.show_help = true;
            render(&mut app, width, height);
            app.on_key(key(KeyCode::End));
            let screen = render(&mut app, width, height);
            assert!(app.show_help);
            assert!(has(&screen, "鼠标"), "{screen}");
        }
    }

    #[test]
    fn 表单里寻址画成可选箭头() {
        let mut app = App::new();
        app.wizard = Some(crate::wizard::Wizard::new());
        let form = app.wizard.as_mut().unwrap();
        form.set_kind(2);
        let index = form
            .fields()
            .iter()
            .position(|field| field.label == "寻址")
            .expect("S3 表单必须有寻址字段");
        form.focus_field(index);
        let screen = render(&mut app, 120, 34);
        assert!(
            has(&screen, "‹ 自动 ›"),
            "聚焦时要用箭头表示左右能拨：{screen}"
        );

        // 拨到"路径式"：显示中文，写进配置的仍是 path
        app.wizard.as_mut().unwrap().cycle_value(2);
        let screen = render(&mut app, 120, 34);
        assert!(has(&screen, "‹ 路径式 ›"), "{screen}");
    }

    #[test]
    fn 密钥页完整显示_64_位并提供导出() {
        let key = "e388b01b".to_string() + &"a1b2c3d4".repeat(7);
        assert_eq!(key.chars().count(), 64);
        for (w, h) in [(100u16, 20u16), (56, 18)] {
            let mut app = App::new();
            app.show_panel = true;
            app.exported_key = Some(key.clone());
            let screen = render(&mut app, w, h);
            // 完整：每一段拼接起来必须等于原文（折行不能丢字符）
            let joined: String = screen.chars().filter(|c| c.is_ascii_hexdigit()).collect();
            assert!(joined.contains(&key), "{w}x{h} 下密钥被截断了：{screen}");
            assert!(has(&screen, "64 位"), "要标出长度：{screen}");
            assert!(has(&screen, "x"), "要提示可以导出：{screen}");
            assert!(has(&screen, "导出到文件"), "{screen}");
        }
    }
    #[test]
    fn 安全与时间文案不误导() {
        assert!(absolute_time(1_759_323_998_000).ends_with("UTC"));
        let mut app = app_with(1);
        assert!(!app.health_line().unwrap().0.contains("只留在本机"));
        app.apply(UiEvent::Disconnected);
        assert!(app.status_line().contains("已过期"));
        app.show_panel = true;
        let screen = render(&mut app, 180, 70);
        assert!(!screen.contains("<SecretAccessKey>"));
        assert!(!has(&screen, "保存前自测"));
    }

    #[test]
    fn 诊断失败面板可见且_r_重试不关闭() {
        use crossterm::event::KeyCode;
        let mut app = app_with(1);
        app.show_panel = true;
        app.apply(UiEvent::Failure {
            command: "doctor".to_string(),
            message: "synthetic failure".to_string(),
        });
        let screen = render(&mut app, 180, 100);
        assert!(
            has(&screen, "诊断读取失败：synthetic failure；按 r 重试"),
            "{screen}"
        );
        assert!(!has(&screen, "正在读取诊断"));
        assert_eq!(
            app.on_key(key(KeyCode::Char('r'))),
            crate::KeyOutcome::Doctor
        );
        assert!(app.show_panel);
        assert!(app.doctor_error.is_none());
    }

    #[test]
    fn 空历史时不会_panic() {
        let mut app = App::new();
        let screen = render(&mut app, 100, 20);
        assert!(has(&screen, "0/0"), "{screen}");
    }

    // ------------------------------------------------------------ 后端面板

    fn backend(
        name: &str,
        kind: &str,
        enabled: bool,
        failures: u32,
        error: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "kind": kind,
            "enabled": enabled,
            "last_ok_ms": cx_core::now_ms() - 120_000,
            "last_error": error,
            "failures": failures,
            "next_attempt_ms": cx_core::now_ms() + 45_000,
            "published": 12,
            "applied": 5
        })
    }

    fn doctor() -> UiEvent {
        UiEvent::Doctor(Box::new(
            serde_json::from_value(serde_json::json!({
                "checks": [
                    {"name": "目录", "ok": true, "detail": "可写"},
                    {"name": "局域网", "ok": true, "detail": "已启用（端口 8699），ALEXLAPTOP（a5af2a1b）"},
                    {"name": "密钥权限", "ok": true, "detail": "1 个密钥文件都没有组/其他权限"},
                    {"name": "剪贴板", "ok": true, "detail": "系统剪贴板；平台事件通知：可用"}
                ]
            }))
            .expect("诊断 JSON 应当能反序列化"),
        ))
    }

    #[test]
    fn 面板列出后端健康与错误原因() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [
                    backend("home", "server", true, 0, None),
                    backend("oss", "s3", true, 4, Some("连接超时：dial tcp 10.0.0.9:9000")),
                    backend("dav", "webdav", false, 0, None)
                ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.apply(doctor());
        app.show_panel = true;
        let screen = render(&mut app, 120, 30);

        assert!(has(&screen, "后端管理"), "{screen}");
        // 表头：一眼看得出这几列是什么
        assert!(has(&screen, "名称"), "{screen}");
        assert!(has(&screen, "开关"), "要有一列明确显示启用状态：{screen}");
        assert!(has(&screen, "home"), "{screen}");
        assert!(has(&screen, "oss"), "{screen}");
        assert!(has(&screen, "已停用"), "停用的后端要标出来：{screen}");
        assert!(has(&screen, "失败 4 次"), "要给出失败次数：{screen}");
        assert!(has(&screen, "连接超时"), "要给出最近一次错误：{screen}");
        assert!(has(&screen, "秒后重试"), "要给出退避时间：{screen}");
        // 诊断区只列"需要你动手"的项：一切正常时不显示任何检查项，
        // 剪贴板/监听方式这类对用户没有意义的行也不再出现
        assert!(
            !has(&screen, "ALEXLAPTOP"),
            "正常的局域网检查不该占版面：{screen}"
        );
        assert!(!has(&screen, "密钥权限"), "{screen}");
        assert!(!has(&screen, "监听方式"), "{screen}");
        assert!(!has(&screen, "连接与安全"), "{screen}");
    }

    #[test]
    fn 窄屏面板砍掉目标列但保留状态与开关() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [
                    { "name": "oss", "kind": "s3", "enabled": true, "failures": 4,
                      "next_attempt_ms": 0, "published": 1, "applied": 1,
                      "detail": "bucket-a @ https://oss-cn-hangzhou.aliyuncs.com" }
                ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        // 整屏渲染后内容区 = 宽度 - 2，取 56 才落到窄屏分支（<62）
        let screen = render(&mut app, 56, 24);
        assert!(has(&screen, "名称"), "{screen}");
        assert!(has(&screen, "开关"), "{screen}");
        assert!(
            has(&screen, "失败 4 次"),
            "窄屏也必须看得到状态与退避：{screen}"
        );
        assert!(!has(&screen, "bucket-a"), "窄屏不画目标列：{screen}");
    }

    #[test]
    fn 有后端但没空间时面板给出按_s_的提示() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": null, "encrypted": false,
                "backends": [backend("oss", "s3", true, 0, None)],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        let screen = render(&mut app, 100, 24);
        assert!(has(&screen, "还没有加密空间"), "{screen}");
        assert!(has(&screen, "按 s"), "要告诉用户按什么键：{screen}");
        assert!(has(&screen, "s 建空间"), "页脚也要有：{screen}");
    }
    #[test]
    fn 面板里显示上一操作的结果() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [ backend("home", "server", true, 0, None) ],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        app.show_panel = true;
        app.note("后端 home 已停用，云端对象和局域网设置未改变");
        let screen = render(&mut app, 120, 30);
        // 界面内执行的动作结果必须留在面板里可见：否则用户按了空格却不知道发生了什么
        assert!(
            has(&screen, "已停用，云端对象和局域网设置未改变"),
            "面板里要能看到刚做完的动作：{screen}"
        );
    }

    #[test]
    fn 面板在没有后端时给出添加命令() {
        let mut app = app_with(1);
        app.show_panel = true;
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "还没有同步后端"), "{screen}");
        assert!(
            has(&screen, "按 a 选一种"),
            "要给出可执行的第一步：{screen}"
        );
    }

    #[test]
    fn 头部健康提示在空后端与失败时都出现() {
        // 空后端
        let mut app = app_with(1);
        let screen = render(&mut app, 130, 24);
        assert!(has(&screen, "还没有同步后端"), "{screen}");

        // 有后端在失败
        let mut failing = App::new();
        failing.apply(UiEvent::Status(Box::new(
            serde_json::from_value(serde_json::json!({
                "version": "0.1.0", "pid": 1, "started_ms": 0u64, "paused": false,
                "device_id": "ab12cd34ef567890", "device_name": "笔记本",
                "space_id": "space-1", "encrypted": true,
                "backends": [backend("oss", "s3", true, 2, Some("连接超时"))],
                "history_len": 0, "seen_len": 0, "skipped_oversize": 0,
                "skipped_ignored": 0, "last_published_hash": null,
                "clipboard": "系统剪贴板（测试）", "watch": "平台事件通知", "no_clipboard": false
            }))
            .expect("状态 JSON 应当能反序列化"),
        )));
        let screen = render(&mut failing, 130, 24);
        assert!(has(&screen, "1 个正在失败"), "{screen}");
        assert!(has(&screen, "按 b"), "{screen}");
        // 状态行本身还在（动态头部高度不能把它挤掉）；正常态不再重复"守护进程已连接"
        assert!(has(&screen, "笔记本"), "状态行不该被裁掉：{screen}");
    }
}
