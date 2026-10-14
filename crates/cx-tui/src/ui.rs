//! 渲染。
//!
//! 与终端解耦：`draw` 只依赖 `ratatui::Frame`，所以可以在 `TestBackend` 上画一帧
//! 再断言"画面上有什么字"——TUI 的渲染逻辑因此也进了单元测试，而不是只能靠人眼。
//!
//! 布局策略：宽屏左右分栏（列表 + 预览），**窄屏改为上下**——在 80 列以下的终端里
//! 左右分栏会把两边都挤到不可读，上下堆叠反而能用。

use crate::app::{App, PreviewState};
use crate::wizard::{Focus, WizardKind};
use cx_proto::control::HistorySummary;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

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
    if app.health_line().is_some() {
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

    let preview_rows = draw_body(frame, chunks[1], app);
    draw_header(frame, chunks[0], app);
    draw_footer(frame, chunks[2], app, preview_rows);

    if app.show_help {
        draw_help(frame, area);
    }
    if app.show_panel {
        draw_panel(frame, area, app);
    }
    if let Some(position) = app.pending_delete {
        draw_confirm_delete(frame, area, position);
    }
    if app.wizard.is_some() {
        draw_wizard(frame, area, app);
    }
    if let Some(name) = &app.pending_backend_remove {
        draw_confirm_remove(frame, area, name);
    }
}

/// 移除后端的确认：与删除历史条目一样，破坏性操作默认"不删"。
fn draw_confirm_remove(frame: &mut Frame<'_>, area: Rect, name: &str) {
    let popup = centered(area, 64, 30);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            format!("移除后端 {name}？"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  只删本机配置与凭据，不删除已上传到该后端的对象；"),
        Line::from("  其他已配置的后端不受影响。之后可以再按 a 添加回来。"),
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
fn draw_confirm_delete(frame: &mut Frame<'_>, area: Rect, position: usize) {
    let popup = centered(area, 62, 30);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            format!("删除第 {position} 条？"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
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

    draw_history(frame, panes[0], app);
    draw_preview(frame, panes[1], app);

    // 列表内高（去掉上下边框）
    panes[0].height.saturating_sub(2) as usize
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            " ClipboardX ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("（终端界面）", Style::default().fg(Color::DarkGray)),
    ]);
    let state = app.status_line();
    let state_style = match &app.status {
        None => Style::default().fg(Color::Red),
        Some(status) if status.paused => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
    };
    let mut lines = vec![title, Line::from(Span::styled(state, state_style))];
    // 健康行：空后端 / 有后端在失败时出现（都是"我明明开着为什么不同步"的答案）
    if let Some((text, warn)) = app.health_line() {
        lines.push(Line::from(Span::styled(
            format!(" {text}"),
            if warn {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )));
    }
    if let Some(notice) = &app.notice {
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
            " 历史 {position}（过滤 “{}”，命中 {shown}/{total}，0 = 最新）",
            app.filter()
        )
    } else {
        format!(" 历史 {position}（共 {total} 条，0 = 最新）")
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

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
                "历史里还没有内容。复制点什么，或者按 p 发布当前剪贴板。"
            },
            Style::default().fg(Color::DarkGray),
        ))]),
        // 有内容摘要在手上时走第一个分支；这里兜住其余组合（正在读取等）
        _ => Text::from(vec![Line::from(Span::styled(
            "（正在读取…）",
            Style::default().fg(Color::DarkGray),
        ))]),
    };

    let block = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.preview_scroll(), 0));
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

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App, _rows: usize) {
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

    let hint = if app.show_help {
        " 任意键返回　q 退出 "
    } else if app.filter_active() {
        " j/k 选择　PgUp/PgDn 翻页　J/K 滚动预览　回车 写回剪贴板　/ 改过滤　Esc 清过滤　? 帮助　q 退出 "
    } else {
        " j/k 选择　PgUp/PgDn 翻页　J/K 滚动预览　回车 写回剪贴板　空格 暂停/恢复　d 删除　b 后端状态　a 添加后端　/ 过滤　? 帮助　q 退出 "
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
    let Some(form) = app.wizard.as_ref() else {
        return;
    };
    let popup = centered(area, 86, 84);
    frame.render_widget(Clear, popup);

    let fields = form.fields();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        " 添加同步后端",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // 类型单选行
    let mut kind_line = vec![Span::styled(
        format!("{:<10}", "类型"),
        Style::default().fg(Color::DarkGray),
    )];
    for (index, kind) in WizardKind::ALL.iter().enumerate() {
        let selected = index == form.kind_index();
        let focused = matches!(form.focus(), Focus::Kind);
        let style = match (selected, focused) {
            (true, true) => Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            (true, false) => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            (false, _) => Style::default().fg(Color::DarkGray),
        };
        kind_line.push(Span::styled(
            format!(" {} {} ", if selected { "◉" } else { "○" }, kind.label()),
            style,
        ));
    }
    lines.push(Line::from(kind_line));
    lines.push(Line::from(Span::styled(
        format!("           {}", form.kind().hint()),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    // 字段：标签列 + 输入框
    for (index, field) in fields.iter().enumerate() {
        let focused = form.focused_field() == Some(index);
        let invalid = form.is_invalid(index);
        let label_style = if invalid {
            Style::default().fg(Color::Red)
        } else if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let raw = form.value(index);
        let shown = if field.secret {
            // 密文只显示长度，既不回显内容也能让人确认"确实输入了"
            if raw.is_empty() {
                String::new()
            } else {
                format!("{}{}", "●".repeat(raw.chars().count().min(16)), "")
            }
        } else {
            raw.to_string()
        };
        let value_style = if invalid {
            Style::default().fg(Color::Red)
        } else if focused {
            Style::default().fg(Color::Black).bg(Color::White)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", field.label), label_style),
            Span::styled(
                format!("{} ", if field.required() { "*" } else { " " }),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                if focused {
                    format!("{shown}▏")
                } else if shown.is_empty() {
                    "（未填）".to_string()
                } else {
                    shown
                },
                value_style,
            ),
            Span::styled(
                format!("   {}", field.help),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    lines.push(Line::from(""));
    // 按钮
    let save_style = if matches!(form.focus(), Focus::Save) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    let cancel_style = if matches!(form.focus(), Focus::Cancel) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    lines.push(Line::from(vec![
        Span::styled("          ", Style::default()),
        Span::styled(" [ 保存并测试 ] ", save_style),
        Span::raw("  "),
        Span::styled(" [ 取消 ] ", cancel_style),
    ]));
    if let Some(message) = &form.message {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Tab/↑↓ 换行　←→ 选类型　直接输入　Ctrl+Y 填示例　Ctrl+L 清空　回车 保存　Esc 取消　（也可用鼠标点行）",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 添加后端 ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}
/// "后端与连接"面板：后端健康、局域网对端、以及 doctor 里的其余检查。
///
/// 为什么需要它：TUI 原来只看得到"后端 N 个"，看不出**哪个后端在失败、退避到什么时候、
/// 局域网到底连上没有**——而这些恰恰是"同步不工作"时最先要问的问题。
fn draw_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let popup = centered(area, 78, 74);
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "后端与连接",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    match &app.status {
        None => lines.push(Line::from(Span::styled(
            "还没连上守护进程，看不到后端状态。",
            Style::default().fg(Color::Red),
        ))),
        Some(status) if status.backends.is_empty() => {
            lines.push(Line::from(Span::styled(
                "还没有配置同步后端：内容只留在本机历史里。",
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(" 按 ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "a",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " 在界面里直接填表添加（类型、字段、密钥都在表单里填，保存前自测）",
                    Style::default().fg(Color::Cyan),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                " 下面是等价的命令行做法，适合脚本化或批量配置：",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " 1) 选一种落地方式",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(
                "    server  自建服务端 cx-server（自己完全掌控）",
            ));
            lines.push(Line::from("    webdav  WebDAV（Nextcloud 等）"));
            lines.push(Line::from(
                "    s3      S3 兼容（AWS / MinIO / R2 / 阿里云 OSS）",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " 2) 准备凭据（密钥走标准输入，不进命令行历史）",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from("    s3      AccessKeyId + SecretAccessKey"));
            lines.push(Line::from(
                "            阿里云：RAM 控制台创建，只给该桶的读写权限",
            ));
            lines.push(Line::from(
                "    webdav  用户名 + 口令（建议用应用专用密码）",
            ));
            lines.push(Line::from(
                "    server  空间 id + 注册密钥（cx-server token issue 生成）",
            ));
            lines.push(Line::from(
                "    例：printf '%s' '<SecretAccessKey>' | cx backend add s3 …",
            ));
            lines.push(Line::from(Span::styled(
                "    凭据落在 secrets/ 下（权限 0600），不会写进 config.toml",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " 3) 填字段并添加",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from("    s3      桶名、端点、区域、前缀"));
            lines.push(Line::from(Span::styled(
                "            区域必须与端点同区域：阿里云杭州 = oss-cn-hangzhou.aliyuncs.com + cn-hangzhou",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from("    webdav  URL、用户名、口令"));
            lines.push(Line::from("    server  服务端地址、空间 id、注册密钥"));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " 4) 加完先自测，再让守护进程用上它",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(
                "    cx backend test <名字>     # 不依赖守护进程",
            ));
            lines.push(Line::from(
                "    cx stop && cx start       # 新增后端需要重启",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "界面内的配置向导（字段引导 + 密文输入 + 保存前自测）正在开发。",
                Style::default().fg(Color::Cyan),
            )));
        }
        Some(status) => {
            for backend in &status.backends {
                let (mark, style) = if !backend.enabled {
                    ("○", Style::default().fg(Color::DarkGray))
                } else if backend.failures > 0 || backend.last_error.is_some() {
                    ("●", Style::default().fg(Color::Red))
                } else {
                    ("●", Style::default().fg(Color::Green))
                };
                let selected = status
                    .backends
                    .iter()
                    .position(|item| item.name == backend.name)
                    == Some(app.panel_backend());
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "▶ " } else { "  " },
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(format!("{mark} "), style),
                    Span::styled(
                        format!("{:<10}", backend.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{:<8}", backend.kind), Style::default()),
                    Span::raw(format!(
                        "发布 {:<5} 接收 {:<5} ",
                        backend.published, backend.applied
                    )),
                    if !backend.enabled {
                        Span::styled("已禁用".to_string(), Style::default().fg(Color::DarkGray))
                    } else if backend.failures > 0 {
                        Span::styled(
                            format!("失败 {} 次，{} 后重试", backend.failures, retry_in(backend)),
                            Style::default().fg(Color::Red),
                        )
                    } else {
                        Span::styled(
                            match backend.last_ok_ms {
                                Some(ms) => format!("最近成功 {}", relative_time(ms)),
                                None => "还没成功过".to_string(),
                            },
                            Style::default().fg(Color::DarkGray),
                        )
                    },
                ]));
                if let Some(error) = &backend.last_error {
                    lines.push(Line::from(Span::styled(
                        format!("    └ {error}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
    }

    // doctor 里那些 TUI 看不到、但排查时最先要问的几项
    if let Some(doctor) = &app.doctor {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "连接与安全",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for check in &doctor.checks {
            if !matches!(
                check.name.as_str(),
                "局域网" | "密钥权限" | "剪贴板" | "监听方式"
            ) {
                continue;
            }
            lines.push(Line::from(vec![
                Span::styled(
                    // 形状与颜色一起变：只靠颜色区分，色觉障碍的用户就看不出来了
                    if check.ok { "● " } else { "▲ " },
                    if check.ok {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(format!("{:<8}", check.name), Style::default()),
                Span::raw(check.detail.clone()),
            ]));
        }
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "（正在读取诊断…）",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ 选后端　t 停用/启用　x 移除　b/Esc 关闭　J/K、PgUp/PgDn 滚动　0 回开头",
        Style::default().fg(Color::DarkGray),
    )));

    // 引导内容是长文，小终端一屏放不下，因此面板可滚动
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title(" 后端与连接 "))
            .wrap(Wrap { trim: false })
            .scroll((app.panel_scroll(), 0)),
        popup,
    );
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
fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 66, 80);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "快捷键",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  ↑ / k            上移一行"),
        Line::from("  ↓ / j            下移一行"),
        Line::from("  PgUp / PgDn      上下翻页"),
        Line::from("  Home / g         回到最新"),
        Line::from("  End / G          跳到最旧"),
        Line::from("  回车             把选中项写回剪贴板"),
        Line::from("  /                过滤（回车确定，Esc 取消）"),
        Line::from("  Esc              有过滤时先清过滤，再按退出"),
        Line::from("  J / K            预览下滚 / 上滚一行"),
        Line::from("  Ctrl+D / Ctrl+U  预览上下翻页"),
        Line::from("  0                预览回到开头"),
        Line::from("  空格             暂停 / 恢复同步"),
        Line::from("  p                立即发布本机剪贴板"),
        Line::from("  r                刷新状态与历史"),
        Line::from("  R                重新读取配置"),
        Line::from("  a                添加同步后端（表单：类型 / 字段 / 密钥，保存前自测）"),
        Line::from("  鼠标             滚轮翻列表；左键点某一行即选中它；表单里点某行即聚焦"),
        Line::from("  d                删除选中项（本机历史，会先确认）"),
        Line::from("  b                后端与连接面板（各后端健康、局域网对端）"),
        Line::from("  ?                帮助（本页）"),
        Line::from("  q                退出"),
        Line::from(""),
        Line::from(Span::styled(
            "  ↑ 本机发出，↓ 来自别的设备，· 状态未知；行内摘要是按需加载的，",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  因此过滤只能匹配已加载的内容摘要（类型/来源/哈希总能匹配）。",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  界面只读控制套接字：不直接动剪贴板、不直接访问网络。",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title(" 帮助 "))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// 居中放一个固定比例的弹窗（自动收进屏幕内）。
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
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

/// 绝对时间（本地时区，`MM-DD HH:MM`）。
fn absolute_time(ms: u64) -> String {
    let total_seconds = (ms / 1000) as i64;
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = cx_proto::civil::civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let _ = year;
    format!("{month:02}-{day:02} {hour:02}:{minute:02}")
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
        assert!(has(&screen, "共 12 条"), "{screen}");
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
        assert!(has(&screen, "命中 0/4"), "{screen}");
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
            .position(|l| compact(l).contains("0=最新"))
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
        assert!(has(&screen, "按需加载"), "应当说明过滤的局限：{screen}");
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
        assert!(has(&screen, "删除选中项"), "{screen}");
        assert!(has(&screen, "会先确认"), "帮助里要说明需要确认：{screen}");
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
        assert!(has(&screen, "x 移除"), "{screen}");
        assert!(has(&screen, "t 停用/启用"), "要告诉用户能停用：{screen}");

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

        assert!(has(&screen, "后端与连接"), "{screen}");
        assert!(has(&screen, "home"), "{screen}");
        assert!(has(&screen, "oss"), "{screen}");
        assert!(has(&screen, "已禁用"), "禁用的后端要标出来：{screen}");
        assert!(has(&screen, "失败 4 次"), "要给出失败次数：{screen}");
        assert!(has(&screen, "连接超时"), "要给出最近一次错误：{screen}");
        assert!(has(&screen, "秒后重试"), "要给出退避时间：{screen}");
        // 局域网对端只有 doctor 知道，面板里必须能看到
        assert!(has(&screen, "局域网"), "{screen}");
        assert!(has(&screen, "ALEXLAPTOP"), "{screen}");
        assert!(has(&screen, "密钥权限"), "{screen}");
    }

    #[test]
    fn 面板在没有后端时给出添加命令() {
        let mut app = app_with(1);
        app.show_panel = true;
        let screen = render(&mut app, 120, 30);
        assert!(has(&screen, "还没有配置同步后端"), "{screen}");
        assert!(
            has(&screen, "cx backend add"),
            "要给出可执行的命令：{screen}"
        );
    }

    #[test]
    fn 头部健康提示在空后端与失败时都出现() {
        // 空后端
        let mut app = app_with(1);
        let screen = render(&mut app, 130, 24);
        assert!(has(&screen, "还没有配置同步后端"), "{screen}");

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
        // 状态行本身还在（动态头部高度不能把它挤掉）
        assert!(has(&screen, "同步中"), "状态行不该被裁掉：{screen}");
    }
}
