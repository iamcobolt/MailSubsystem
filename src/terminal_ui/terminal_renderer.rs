use chrono::Local;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

use super::app::{
    App, AppTab, ChatFocus, FolderListItem, OverlayState, TranscriptEntryKind, CATEGORY_OPTIONS,
    SPAM_OPTIONS,
};

pub fn draw(frame: &mut Frame, app: &App) {
    let layout = if app.banner_message.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(2),
                Constraint::Min(10),
                Constraint::Length(4),
            ])
            .split(frame.area())
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(10),
                Constraint::Length(4),
            ])
            .split(frame.area())
    };

    let (banner_area, tabs_area, content_area, footer_area) = if app.banner_message.is_some() {
        (Some(layout[0]), layout[1], layout[2], layout[3])
    } else {
        (None, layout[0], layout[1], layout[2])
    };

    if let Some(area) = banner_area {
        let banner = Paragraph::new(app.banner_message.clone().unwrap_or_default())
            .block(
                Block::default()
                    .title("Connection")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            )
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(banner, area);
    }

    draw_tabs(frame, app, tabs_area);
    match app.active_tab {
        AppTab::Chat => draw_chat(frame, app, content_area),
        AppTab::Emails => draw_emails(frame, app, content_area),
        AppTab::Folders => draw_folders(frame, app, content_area),
        AppTab::Status => draw_status(frame, app, content_area),
    }
    draw_footer(frame, app, footer_area);

    if let Some(overlay) = &app.overlay {
        match overlay {
            OverlayState::Help => draw_help_overlay(frame, app),
            OverlayState::AgentPicker { selected } => draw_agent_picker(frame, app, *selected),
            OverlayState::DeleteConfirm { thread_id } => draw_delete_confirm(frame, thread_id),
        }
    }
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let tabs = Tabs::new(
        app.tab_titles()
            .iter()
            .map(|title| Line::from((*title).to_string()))
            .collect::<Vec<_>>(),
    )
    .select(app.active_tab.index())
    .style(Style::default().fg(Color::Gray))
    .highlight_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .divider(" ");
    frame.render_widget(
        tabs.block(
            Block::default()
                .title("MailSubsystem")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn draw_chat(frame: &mut Frame, app: &App, area: Rect) {
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(40)])
        .split(area);

    draw_threads(frame, app, body_chunks[0]);
    let chat_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(body_chunks[1]);

    draw_transcript(frame, app, chat_chunks[0]);
    draw_chat_composer(frame, app, chat_chunks[1]);
}

fn draw_threads(frame: &mut Frame, app: &App, area: Rect) {
    let items = app.thread_items();
    let list_items = if items.is_empty() {
        vec![ListItem::new(
            "No threads yet. Press n to start with Mail Assistant.",
        )]
    } else {
        items
            .into_iter()
            .map(|thread| {
                let marker = if thread.is_active { "●" } else { " " };
                let context = if thread.has_context { " [ctx]" } else { "" };
                ListItem::new(vec![
                    Line::from(format!("{marker} {}{}", thread.title, context))
                        .style(Style::default().add_modifier(Modifier::BOLD)),
                    Line::from(thread.subtitle).style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect()
    };

    let title = if app.chat_focus == ChatFocus::Threads {
        "Threads [focus]"
    } else {
        "Threads"
    };
    let list = List::new(list_items)
        .block(block(title, app.chat_focus == ChatFocus::Threads))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .highlight_symbol(">");
    let mut state = ListState::default();
    if !app.threads.is_empty() {
        state.select(Some(app.selected_thread_idx));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let entries = app.transcript_entries();
    let mut lines = Vec::new();
    for entry in entries {
        let header_style = match entry.kind {
            TranscriptEntryKind::User => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            TranscriptEntryKind::Agent => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            TranscriptEntryKind::Status => Style::default().fg(Color::Yellow),
        };
        lines.push(Line::from(Span::styled(entry.label, header_style)));
        for line in entry.body.lines() {
            lines.push(Line::from(line.to_string()));
        }
        lines.push(Line::default());
    }

    let title = format!("{} — {}", app.header_title(), app.header_subtitle());
    let scroll = transcript_scroll_from_bottom(app.transcript_scroll, &lines, area);
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block(
            if app.chat_focus == ChatFocus::Transcript {
                format!("{title} [focus]")
            } else {
                title
            },
            app.chat_focus == ChatFocus::Transcript,
        ))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn transcript_scroll_from_bottom(scroll_from_bottom: u16, lines: &[Line<'_>], area: Rect) -> u16 {
    let inner_height = area.height.saturating_sub(2);
    if inner_height == 0 {
        return 0;
    }

    let content_height = wrapped_line_count(lines, area.width.saturating_sub(2));
    let max_scroll = content_height.saturating_sub(inner_height);
    max_scroll.saturating_sub(scroll_from_bottom)
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> u16 {
    let width = width.max(1) as usize;
    lines.iter().fold(0u16, |count, line| {
        let line_height = line.width().max(1).div_ceil(width);
        count.saturating_add(line_height.min(u16::MAX as usize) as u16)
    })
}

fn draw_chat_composer(frame: &mut Frame, app: &App, area: Rect) {
    let placeholder = if app.send_pending {
        "Waiting for Mail Assistant..."
    } else {
        "Type a message and press Enter"
    };
    let composer_text = if app.composer.is_empty() {
        Span::styled(placeholder, Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(app.composer.as_str())
    };

    let composer = Paragraph::new(Line::from(composer_text)).block(block(
        if app.chat_focus == ChatFocus::Composer {
            "Message [focus]"
        } else {
            "Message"
        },
        app.chat_focus == ChatFocus::Composer,
    ));
    frame.render_widget(composer, area);
}

fn draw_emails(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(10)])
        .split(area);

    let search_label = if app.email_search_mode {
        format!(
            "Search: {}_ | Category: {} | Spam: {} | Folder: {}",
            app.email_search_input,
            CATEGORY_OPTIONS[app.email_category_idx],
            SPAM_OPTIONS[app.email_spam_idx],
            app.folder_filter_label()
        )
    } else {
        app.emails_filter_summary()
    };
    let filters = Paragraph::new(search_label)
        .block(block("Filters", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(filters, chunks[0]);

    if app.email_detail_expanded {
        draw_email_detail(frame, app, chunks[1], true);
        return;
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(chunks[1]);

    draw_emails_list(frame, app, body[0]);
    draw_email_detail(frame, app, body[1], false);
}

fn draw_emails_list(frame: &mut Frame, app: &App, area: Rect) {
    let list_items = if app.emails.is_empty() {
        vec![ListItem::new("No emails matched the current filters.")]
    } else {
        app.emails
            .iter()
            .map(|email| {
                let subject = email
                    .subject
                    .clone()
                    .unwrap_or_else(|| "(no subject)".to_string());
                let sender = email
                    .sender
                    .clone()
                    .unwrap_or_else(|| "(unknown sender)".to_string());
                let category = email.category.clone().unwrap_or_else(|| "-".to_string());
                let status = email
                    .action_status
                    .clone()
                    .unwrap_or_else(|| "pending".to_string());
                ListItem::new(vec![
                    Line::from(subject).style(Style::default().add_modifier(Modifier::BOLD)),
                    Line::from(format!("{sender} • {category} • {status}"))
                        .style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect()
    };

    let list = List::new(list_items)
        .block(block("Emails", false))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .highlight_symbol(">");
    let mut state = ListState::default();
    if !app.emails.is_empty() {
        state.select(Some(app.selected_email_idx));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_email_detail(frame: &mut Frame, app: &App, area: Rect, expanded: bool) {
    let title = if expanded {
        "Email Detail [expanded]"
    } else {
        "Email Detail"
    };
    let body = if app.email_detail_loading {
        "Loading email detail...".to_string()
    } else if let Some(email) = &app.email_detail {
        build_email_detail(email)
    } else {
        "Select an email to inspect its detail view.".to_string()
    };
    let widget = Paragraph::new(body)
        .block(block(title, false))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn build_email_detail(email: &super::client::EmailRecord) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Subject: {}",
        email.subject.as_deref().unwrap_or("(no subject)")
    ));
    parts.push(format!(
        "From: {}",
        email.sender.as_deref().unwrap_or("(unknown sender)")
    ));
    if let Some(received) = email.received_date {
        parts.push(format!(
            "Received: {}",
            received.with_timezone(&Local).format("%Y-%m-%d %H:%M")
        ));
    }
    parts.push(format!(
        "Category: {}",
        email.category.as_deref().unwrap_or("-")
    ));
    parts.push(format!("Spam: {}", email.spam_status));
    parts.push(format!(
        "Location: {}",
        email.location.as_deref().unwrap_or("-")
    ));
    parts.push(String::new());
    parts.push(format!(
        "Summary: {}",
        email
            .human_summary
            .as_deref()
            .or(email.topic.as_deref())
            .unwrap_or("No summary stored for this email.")
    ));
    parts.push(String::new());
    let body = email
        .body_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .or(email.raw_email_content.as_deref())
        .unwrap_or("No synced body content is available for this message yet.");
    parts.push(body.to_string());
    parts.join("\n")
}

fn draw_folders(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let folder_items = app.folders_flattened();
    let list_items = if folder_items.is_empty() {
        vec![ListItem::new("No folders loaded yet.")]
    } else {
        folder_items
            .iter()
            .map(render_folder_item)
            .collect::<Vec<_>>()
    };

    let list = List::new(list_items)
        .block(block("Folders", false))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .highlight_symbol(">");
    let mut state = ListState::default();
    if !folder_items.is_empty() {
        state.select(Some(app.selected_folder_idx));
    }
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let detail = Paragraph::new(format!(
        "Current email folder filter: {}\n\nUse h/l or left/right to collapse and expand folders.\nPress Enter to switch to Emails filtered to the selected folder.",
        app.folder_filter_label()
    ))
    .block(block("Folder Actions", false))
    .wrap(Wrap { trim: true });
    frame.render_widget(detail, chunks[1]);
}

fn render_folder_item(folder: &FolderListItem) -> ListItem<'static> {
    let indent = "  ".repeat(folder.depth);
    let marker = if folder.has_children {
        if folder.expanded {
            "▾"
        } else {
            "▸"
        }
    } else {
        "•"
    };
    ListItem::new(format!(
        "{}{} {} ({})",
        indent, marker, folder.name, folder.message_count
    ))
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(8)])
        .split(area);

    let stats_body = if app.status_loading && app.status_stats.is_none() {
        "Loading dashboard stats...".to_string()
    } else if let Some(stats) = &app.status_stats {
        format!(
            "Window: last {} days since {}\nTotal emails: {}\nAnalyzed: {}\nFiled: {}\nInbox remaining: {}\nSpam: {}\nPhishing: {}\nFolders: {}\nWindow received: {} | marketing {} | otp {}",
            stats.window_days,
            stats.since.with_timezone(&Local).format("%Y-%m-%d"),
            stats.total_emails,
            stats.analyzed_count,
            stats.filed_count,
            stats.inbox_remaining,
            stats.spam_count,
            stats.phishing_count,
            stats.folder_count,
            stats.window.total_received,
            stats.window.marketing_count,
            stats.window.otp_count,
        )
    } else {
        "No status data loaded yet.".to_string()
    };
    let stats_widget = Paragraph::new(stats_body)
        .block(block("Dashboard", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(stats_widget, chunks[0]);

    let run_items = if app.status_runs.is_empty() {
        vec![ListItem::new("No recent agent runs found.")]
    } else {
        app.status_runs
            .iter()
            .map(|run| {
                let duration = run
                    .duration_ms
                    .map(|value| format!("{value} ms"))
                    .unwrap_or_else(|| "-".to_string());
                ListItem::new(vec![
                    Line::from(format!(
                        "{} • {} • {}",
                        run.agent_name, run.status, run.task_id
                    ))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                    Line::from(format!(
                        "{} • steps {} • llm {} • tools {} • {}",
                        run.started_at.with_timezone(&Local).format("%b %d %H:%M"),
                        run.steps,
                        run.llm_calls,
                        run.tool_calls,
                        duration
                    ))
                    .style(Style::default().fg(Color::DarkGray)),
                ])
            })
            .collect()
    };
    let runs = List::new(run_items).block(block("Recent Runs", false));
    frame.render_widget(runs, chunks[1]);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(1)])
        .split(area);

    let hint_text = Paragraph::new(app.footer_hint())
        .block(block("Workspace", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(hint_text, chunks[0]);

    let status = Paragraph::new(format!(
        "{} to {} | {} | 1-4 tabs | r refresh | F1 help | q quit",
        app.connection_label(),
        app.api_url(),
        app.status_message
    ))
    .style(Style::default().fg(Color::Gray));
    frame.render_widget(status, chunks[1]);
}

fn draw_help_overlay(frame: &mut Frame, app: &App) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);
    let body = app.help_lines().join("\n");
    let widget = Paragraph::new(body)
        .block(
            Block::default()
                .title("Help")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

fn draw_agent_picker(frame: &mut Frame, app: &App, selected: usize) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);

    let title_bar = Tabs::new(
        ["Chat"]
            .iter()
            .map(|title| Line::from((*title).to_string()))
            .collect::<Vec<_>>(),
    )
    .select(0)
    .style(Style::default().fg(Color::Gray))
    .highlight_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
    .divider(" ");

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(area);
    frame.render_widget(
        Block::default()
            .title("New Assistant Thread")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
        area,
    );
    frame.render_widget(title_bar, chunks[0]);

    let items = app
        .agents
        .iter()
        .map(|agent| {
            let visibility = if agent.advanced_only {
                "advanced specialist"
            } else if agent.is_default {
                "default assistant"
            } else {
                "assistant"
            };
            ListItem::new(vec![
                Line::from(format!("{} [{}]", agent.label, visibility))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(agent.description.clone()).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(Some(selected));
    let list = List::new(items)
        .highlight_symbol(">")
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .block(Block::default().borders(Borders::NONE));
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn draw_delete_confirm(frame: &mut Frame, thread_id: &str) {
    let area = centered_rect(50, 30, frame.area());
    frame.render_widget(Clear, area);
    let widget = Paragraph::new(format!(
        "Delete thread {}?\n\nPress Enter or y to confirm.\nPress Esc or n to cancel.",
        thread_id
    ))
    .block(
        Block::default()
            .title("Delete Thread")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red)),
    )
    .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

fn block<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let border_style = if focused {
        Style::default().fg(Color::Blue)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
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
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    #[test]
    fn chat_tab_renders_visible_composer_contents() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let (mut app, _) = App::new("http://127.0.0.1:3100".to_string());
        app.active_tab = AppTab::Chat;
        app.chat_focus = ChatFocus::Composer;
        app.composer = "hello from the tui".to_string();

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw chat tab");

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        assert!(rendered.contains("Message [focus]"));
        assert!(rendered.contains("hello from the tui"));
    }

    #[test]
    fn transcript_scroll_zero_tracks_bottom() {
        let lines = (0..20)
            .map(|index| Line::from(format!("line {index}")))
            .collect::<Vec<_>>();
        let area = Rect::new(0, 0, 40, 10);

        assert_eq!(transcript_scroll_from_bottom(0, &lines, area), 12);
        assert_eq!(transcript_scroll_from_bottom(3, &lines, area), 9);
    }

    #[test]
    fn transcript_scroll_accounts_for_wrapped_lines() {
        let lines = vec![
            Line::from("short"),
            Line::from("this line wraps in a narrow transcript area"),
        ];
        let area = Rect::new(0, 0, 12, 4);

        assert_eq!(wrapped_line_count(&lines, 10), 6);
        assert_eq!(transcript_scroll_from_bottom(0, &lines, area), 4);
    }
}
