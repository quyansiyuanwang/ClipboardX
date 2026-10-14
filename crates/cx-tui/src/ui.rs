//! 渲染。
//!
//! 与终端解耦：`draw` 只依赖 `ratatui::Frame`，所以可以在 `TestBackend` 上画一帧
//! 再断言"画面上有什么字"——TUI 的渲染逻辑因此也进了单元测试，而不是只能靠人眼。

use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

/// 画一帧。
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // 标题 + 状态
            Constraint::Min(6),    // 历史 + 预览
            Constraint::Length(1), // 快捷键
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app);
    draw_footer(frame, chunks[2], app);

    if app.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(" ClipboardX ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("（终端界面）", Style::default().fg(Color::DarkGray)),
    ]);
    let state = app.status_line();
    let state_style = match &app.status {
        None => Style::default().fg(Color::Red),
        Some(status) if status.paused => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Green),
    };
    let mut lines = vec![title, Line::from(Span::styled(state, state_style))];
    if let Some(notice) = &app.notice {
        lines.push(Line::from(Span::styled(
            format!(" {}", notice.text),
            if notice.is_error {
                Style::default().fg(Color::Red)
            }
            else {
                Style::default().fg(Color::Cyan)
            },
        )));
    }
    let block = Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(block, area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    draw_history(frame, panes[0], app);
    draw_preview(frame, panes[1], app);
}

fn draw_history(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items: Vec<ListItem<'_>> = app
        .items
        .iter()
        .map(|item| {
            let marker = if item.origin == "remote" { "↓" } else { "↑" };
            let text = format!(
                "{} #{} {} {} {}",
                marker,
                item.position,
                human_time(item.created_ms),
                human_bytes(item.bytes as u64),
                short_hash(&item.hash)
            );
            ListItem::new(Line::from(text))
        })
        .collect();

    let title = format!(" 历史（{} 条，0 = 最新）", app.items.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ratatui::widgets::ListState::default();
    if !app.items.is_empty() {
        state.select(Some(app.cursor.min(app.items.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = match app.selected_position() {
        Some(position) => format!(" 内容预览（第 {position} 条）"),
        None => " 内容预览 ".to_string(),
    };
    let text = match (&app.preview, &app.preview_error) {
        (_, Some(err)) => Text::from(vec![
            Line::from(Span::styled(
                format!("读取预览失败：{err}"),
                Style::default().fg(Color::Red),
            )),
        ]),
        (None, None) => Text::from(vec![
            Line::from(Span::styled("（正在读取…）", Style::default().fg(Color::DarkGray))),
        ]),
        (Some(preview), None) => {
            let mut lines = Vec::new();
            lines.push(Line::from(vec![
                Span::styled("类型 ", Style::default().fg(Color::DarkGray)),
                Span::raw(preview.kind.clone()),
                Span::styled("   大小 ", Style::default().fg(Color::DarkGray)),
                Span::raw(human_bytes(preview.bytes as u64)),
                Span::styled("   位置 ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("#{}", preview.position)),
            ]));
            if let Some(text) = &preview.text {
                lines.push(Line::from(""));
                for line in text.lines().take(200) {
                    lines.push(Line::from(line.to_string()));
                }
                if preview.truncated {
                    lines.push(Line::from(Span::styled(
                        "…（内容已截断，cx show 可看全）",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            else if preview.image.is_some() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "这是一张图片。终端里不渲染图片，按回车即可写回剪贴板。",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Text::from(lines)
        }
    };

    let block = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(block, area);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hint = if app.show_help {
        " 任意键返回　q 退出 "
    }
    else {
        " ↑↓/jk 选择　回车 写回剪贴板　空格 暂停/恢复　p 发布　r 刷新　R 重载配置　? 帮助　q 退出 "
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered(area, 60, 60);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled("快捷键", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("  ↑ / k        上移"),
        Line::from("  ↓ / j        下移"),
        Line::from("  Home / g     回到最新"),
        Line::from("  End / G      跳到最旧"),
        Line::from("  回车         把选中项写回剪贴板"),
        Line::from("  空格         暂停 / 恢复同步"),
        Line::from("  p            立即发布本机剪贴板"),
        Line::from("  r            刷新状态与历史"),
        Line::from("  R            重新读取配置"),
        Line::from("  ?            帮助（本页）"),
        Line::from("  q / Esc      退出"),
        Line::from(""),
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

/// 居中放一个固定大小的弹窗（自动收进屏幕内）。
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// 毫秒时间戳 → `MM-DD HH:MM`（本地时区不做换算，直接用 UTC，避免引时区库）。
fn human_time(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (_, month, day) = cx_proto::civil::civil_from_days(days as i64);
    format!(
        "{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        (rest % 3600) / 60
    )
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    }
    else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
    else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::client::UiEvent;
    use cx_proto::control::{HistoryPreview, HistorySummary, StatusReport};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn item(position: usize, origin: &str) -> HistorySummary {
        HistorySummary {
            position,
            name: format!("1759323998000-ab12cd34-9f3c1e77-{position}"),
            kind: "text".to_string(),
            created_ms: 1_759_323_998_000 + position as u64 * 60_000,
            bytes: 12,
            hash: format!("{}{}", "ab".repeat(8), position),
            origin: origin.to_string(),
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

    /// 画一帧并把缓冲区拼成文本（按行、去尾空白）。
    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
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
        out
    }

    /// 忽略空白后再比较。
    ///
    /// 原因：`TestBackend` 对宽字符（中文）会在缓冲区里留一个占位空格，
    /// 于是"同步中"读出来是"同 步 中"。这是测试后端的行为，不是画面问题。
    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn has(screen: &str, needle: &str) -> bool {
        screen.contains(needle) || compact(screen).contains(&compact(needle))
    }

    #[test]
    fn 画面上能看到状态与历史() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        app.apply(UiEvent::History(vec![item(0, "remote"), item(1, "local")]));
        app.apply(UiEvent::Preview(HistoryPreview {
            position: 0,
            kind: "text".to_string(),
            text: Some("这是来自另一台设备的文本".to_string()),
            image: None,
            truncated: false,
            bytes: 30,
        }));

        let screen = render(&app, 100, 30);
        assert!(has(&screen, "ClipboardX"), "{screen}");
        assert!(has(&screen, "同步中"), "{screen}");
        assert!(has(&screen, "笔记本"), "{screen}");
        assert!(has(&screen, "历史（2 条"), "{screen}");
        assert!(has(&screen, "这是来自另一台设备的文本"), "{screen}");
        assert!(has(&screen, "↓"), "远端条目应有方向标记");
        assert!(has(&screen, "↑"), "本机条目应有方向标记");
        assert!(has(&screen, "回车 写回剪贴板"), "页脚应给出快捷键提示");
    }

    #[test]
    fn 暂停时状态栏变黄且显示已暂停() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(true))));
        let screen = render(&app, 100, 20);
        assert!(has(&screen, "已暂停"), "{screen}");
    }

    #[test]
    fn 未连接时给出明确提示() {
        let app = App::new();
        let screen = render(&app, 100, 20);
        assert!(has(&screen, "未连接守护进程"), "{screen}");
        assert!(has(&screen, "正在读取"), "{screen}");
    }

    #[test]
    fn 图片不渲染但给出说明() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![item(0, "local")]));
        app.apply(UiEvent::Preview(HistoryPreview {
            position: 0,
            kind: "image".to_string(),
            text: None,
            image: Some("iVBORw0KGgo=".to_string()),
            truncated: false,
            bytes: 4096,
        }));
        let screen = render(&app, 100, 24);
        assert!(has(&screen, "图片"), "{screen}");
        assert!(has(&screen, "回车即可写回剪贴板"), "{screen}");
        assert!(has(&screen, "4.0 KB"), "{screen}");
    }

    #[test]
    fn 预览失败时显示原因而不是空白() {
        let mut app = App::new();
        app.apply(UiEvent::History(vec![item(0, "local")]));
        app.apply(UiEvent::Failure {
            command: "history.show".to_string(),
            message: "第 9 条不存在".to_string(),
        });
        let screen = render(&app, 100, 24);
        assert!(has(&screen, "读取预览失败"), "{screen}");
        assert!(has(&screen, "第 9 条不存在"), "{screen}");
    }

    #[test]
    fn 帮助页列出全部快捷键() {
        let mut app = App::new();
        app.show_help = true;
        let screen = render(&app, 100, 30);
        for expected in [
            "快捷键",
            "回车",
            "空格",
            "立即发布本机剪贴板",
            "重新读取配置",
            "只读控制套接字",
        ] {
            assert!(has(&screen, expected), "帮助页应包含 {expected}：{screen}");
        }
    }

    #[test]
    fn 小终端也不会崩() {
        let mut app = App::new();
        app.apply(UiEvent::Status(Box::new(status(false))));
        app.apply(UiEvent::History(vec![item(0, "local")]));
        // 20x6 是能跑 TUI 的下限，这里确保不 panic
        let screen = render(&app, 20, 6);
        assert!(!screen.is_empty());
    }

    #[test]
    fn 空历史时不会_panic() {
        let app = App::new();
        let screen = render(&app, 80, 20);
        assert!(has(&screen, "历史（0 条"), "{screen}");
    }
}
