use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table},
};

use crate::app::{App, ConfirmButton, Screen, SuccessAction};
use crate::grow::Status;
use crate::utils::format_bytes;

/// 写入/扩容进度屏共用的 spinner 帧（10 帧，与 app.tick 的 % 10 取模对应）
const SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Center a popup of given width/height within the terminal area.
fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(height), Constraint::Fill(1)])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(width), Constraint::Fill(1)])
        .split(popup_layout[1])[1]
}

// ── Shared dialog helpers ─────────────────────────────────────────────

/// Create a centered dialog block with the given title and border color.
fn dialog_block<'a>(title: &'a str, border_color: Color) -> Block<'a> {
    Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::default())
        .border_style(Style::default().fg(border_color))
}

/// Render a standard dialog: clear area, draw border block, draw paragraph content.
fn render_dialog(frame: &mut Frame, area: Rect, block: Block<'_>, lines: Vec<Line<'_>>) {
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render a Yes/No button row centered with consistent spacing.
fn yes_no_row(no_style: Style, yes_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("  No ", no_style),
        Span::raw("    "),
        Span::styled(" Yes ", yes_style),
    ])
    .centered()
}

/// Build (no_style, yes_style) for dialogs with Yes/No buttons.
fn confirm_button_styles(
    app: &App,
    yes_active: Style,
    yes_inactive: Style,
    no_active: Style,
    no_inactive: Style,
) -> (Style, Style) {
    let no_style = if app.confirm_button == ConfirmButton::No { no_active } else { no_inactive };
    let yes_style = if app.confirm_button == ConfirmButton::Yes { yes_active } else { yes_inactive };
    (no_style, yes_style)
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < 50 || area.height < 15 {
        let msg = Paragraph::new("Terminal too small!\n\nPlease enlarge to at least 50x15.")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
        frame.render_widget(msg, area);
        return;
    }

    match app.screen {
        Screen::DiskList => render_main(app, frame),
        Screen::Confirmation => {
            render_main(app, frame);
            render_confirmation_dialog(app, frame);
        }
        Screen::Writing => {
            render_main(app, frame);
            render_progress_dialog(app, frame);
        }
        Screen::Growing => {
            render_main(app, frame);
            render_growing_dialog(app, frame);
        }
        Screen::WriteError => {
            render_main(app, frame);
            render_write_error_dialog(app, frame);
        }
        Screen::Success => render_success_screen(app, frame),
    }

    if app.show_help {
        render_help_dialog(frame);
    }

    for (index, notification) in app.notifications.iter().enumerate() {
        render_notification(notification, index, frame);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Main disk list view
// ═══════════════════════════════════════════════════════════════════════

fn render_main(app: &mut App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_disks_table(app, frame, chunks[0]);
    render_disk_summary(app, frame, chunks[1]);
    render_context_help(app, frame, chunks[2]);
}

fn render_disks_table(app: &mut App, frame: &mut Frame, area: Rect) {
    if !app.has_disks() {
        let block = Block::default()
            .title(" Select Target Disk ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.error))
            .border_type(BorderType::default());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let msg = Paragraph::new("No writable disks found!\n\nConnect a disk and press r to refresh.")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
        frame.render_widget(msg, inner);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Name").style(Style::default().add_modifier(Modifier::BOLD).fg(app.theme.header)),
        Cell::from("Size").style(Style::default().add_modifier(Modifier::BOLD).fg(app.theme.header)),
        Cell::from("Transport").style(Style::default().add_modifier(Modifier::BOLD).fg(app.theme.header)),
        Cell::from("Type").style(Style::default().add_modifier(Modifier::BOLD).fg(app.theme.header)),
        Cell::from("Model").style(Style::default().add_modifier(Modifier::BOLD).fg(app.theme.header)),
    ])
    .bottom_margin(1);

    let rows: Vec<Row> = app
        .disks
        .iter()
        .map(|disk| {
            Row::new(vec![
                Cell::from(disk.name.as_str()),
                Cell::from(disk.size_str.as_str()),
                Cell::from(disk.transport.as_str()),
                Cell::from(disk.disk_type.as_str()),
                Cell::from(disk.model.as_deref().unwrap_or_default()),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(app.theme.disk_name_width),
        Constraint::Length(app.theme.disk_size_width),
        Constraint::Length(app.theme.disk_bus_width),
        Constraint::Length(app.theme.disk_type_width),
        Constraint::Min(app.theme.disk_model_width),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .title(" Select Target Disk ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.focus_border))
                .border_type(BorderType::default()),
        )
        .column_spacing(2)
        .row_highlight_style(
            Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(table, area, &mut app.disks_state);
}

fn render_disk_summary(app: &App, frame: &mut Frame, area: Rect) {
    let text = if let Some(disk) = app.selected_disk() {
        let removable_str = if disk.is_removable { "Removable" } else { "Fixed" };
        let mount_str = if disk.is_mounted {
            if let Some(ref mp) = disk.mount_point {
                format!("Mounted ({})", mp)
            } else {
                "Mounted".to_string()
            }
        } else {
            "Unmounted".to_string()
        };

        Line::from(vec![
            Span::from(format!("{} | {} | {} | {}", disk.model.as_deref().unwrap_or(""), disk.transport, disk.dev_path, removable_str)),
            Span::from(format!(" | {}", mount_str)),
        ])
    } else {
        Line::from("No disk selected")
    };

    let paragraph = Paragraph::new(text)
        .block(Block::default().title(" Disk Info ").borders(Borders::ALL))
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

fn render_context_help(app: &App, frame: &mut Frame, area: Rect) {
    let spans = match app.screen {
        Screen::DiskList => {
            let mut v = vec![
                Span::from("j/k ").bold().yellow(),
                Span::from("Navigate | "),
            ];
            if app.has_disks() && app.selected_disk().is_some() {
                v.push(Span::from("Enter ").bold().yellow());
                v.push(Span::from("Select | "));
            }
            v.push(Span::from("r ").bold().yellow());
            v.push(Span::from("Refresh | "));
            v.push(Span::from("q ").bold().yellow());
            v.push(Span::from("Shut down | "));
            v.push(Span::from("? ").bold().yellow());
            v.push(Span::from("Help"));
            v
        }
        Screen::Confirmation => vec![
            Span::from("←/→ ").bold().yellow(),
            Span::from("Choose | "),
            Span::from("Tab ").bold().yellow(),
            Span::from("Toggle | "),
            Span::from("Enter ").bold().yellow(),
            Span::from("Confirm | "),
            Span::from("Esc ").bold().yellow(),
            Span::from("Back"),
        ],
        Screen::Writing => vec![
            Span::from("Esc/a ").bold().yellow(),
            Span::from("Abort write"),
        ],
        Screen::Growing => vec![
            Span::from("Auto-expanding partition... ")
                .style(Style::default().fg(Color::Cyan)),
            Span::from("please wait").style(Style::default().fg(Color::DarkGray)),
        ],
        Screen::WriteError => vec![
            Span::from("Enter/Esc ").bold().yellow(),
            Span::from("Return to disk list"),
        ],
        Screen::Success => {
            if app.reboot_counting {
                vec![
                    Span::from("Enter ").bold().yellow(),
                    Span::from("Skip countdown | "),
                    Span::from("Rebooting...").style(Style::default().fg(Color::Cyan)),
                ]
            } else {
                vec![
                    Span::from("←/→ ").bold().yellow(),
                    Span::from("Choose | "),
                    Span::from("Tab ").bold().yellow(),
                    Span::from("Toggle | "),
                    Span::from("Enter ").bold().yellow(),
                    Span::from("Confirm"),
                ]
            }
        }
    };
    frame.render_widget(Line::from(spans).centered(), area);
}

// ═══════════════════════════════════════════════════════════════════════
// Confirmation dialog
// ═══════════════════════════════════════════════════════════════════════

fn render_confirmation_dialog(app: &App, frame: &mut Frame) {
    let disk = match app.selected_disk() {
        Some(d) => d,
        None => return,
    };
    let img_bytes = app.image_file_size().unwrap_or(0);

    let area = centered_rect(60, 12, frame.area());
    let block = dialog_block(" !! DANGEROUS OPERATION !! ", Color::Yellow);

    let (no_style, yes_style) = confirm_button_styles(
        app,
        /* yes active */ Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
        /* yes inactive */ Style::default().fg(Color::Red),
        /* no active */ Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD),
        /* no inactive */ Style::default().fg(Color::DarkGray),
    );

    render_dialog(frame, area, block, vec![
        Line::from(""),
        Line::from("This will ERASE ALL DATA on the target disk!")
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from(vec![
            Span::styled("Target: ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{} ({})", disk.dev_path, disk.size_str), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Image:  ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format_bytes(img_bytes), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        yes_no_row(no_style, yes_style),
        Line::from(""),
    ]);
}

// ═══════════════════════════════════════════════════════════════════════
// Progress dialog (real dd stats + instantaneous speed)
// ═══════════════════════════════════════════════════════════════════════

fn render_progress_dialog(app: &App, frame: &mut Frame) {
    let progress = app.progress.as_ref().unwrap();

    let area = centered_rect(64, 12, frame.area());

    let spinner = SPINNER_CHARS[progress.spinner_index];

    let title = format!(
        " {} Writing to /dev/{} ({}) ",
        spinner, progress.disk_name, progress.disk_model,
    );

    let border_block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::default())
        .border_style(Style::default().fg(Color::Cyan));

    let inner = border_block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(border_block, area);

    let pct = progress.pct();
    let pct_display = pct * 100.0;

    let inner_width = inner.width.saturating_sub(2) as usize;
    let filled = (pct * inner_width as f64).round() as usize;
    let filled = filled.min(inner_width);
    let bar_str = format!(
        "│{}{}│",
        app.theme.progress_bar_filled.repeat(filled),
        app.theme.progress_bar_empty.repeat(inner_width - filled),
    );

    let lines = vec![
        Line::from(""),
        Line::from(format!(
            "  {:.1}%  {}/{}  {:.1} MB/s",
            pct_display,
            format_bytes(progress.written_bytes),
            format_bytes(progress.total_bytes),
            progress.speed,
        ))
        .style(Style::default().fg(Color::White)),
        Line::from(""),
        Line::from(bar_str).style(Style::default().fg(Color::Cyan)),
        Line::from(""),
        Line::from("  Please wait while the image is being written...")
            .style(Style::default().fg(Color::DarkGray)),
        Line::from("  Press Esc to abort and return to disk selection")
            .style(Style::default().fg(Color::DarkGray)),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

// ═══════════════════════════════════════════════════════════════════════
// Growing dialog (post-dd auto-expand)
// ═══════════════════════════════════════════════════════════════════════

fn render_growing_dialog(app: &App, frame: &mut Frame) {
    let Some(g) = app.grow.as_ref() else { return };

    let area = centered_rect(56, 10, frame.area());

    let spinner = SPINNER_CHARS[g.spinner_index];

    let title = format!(" {} Expanding /dev/{} ", spinner, g.disk_name);

    let border_block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::default())
        .border_style(Style::default().fg(Color::Cyan));

    let inner = border_block.inner(area);

    frame.render_widget(Clear, area);
    frame.render_widget(border_block, area);

    let lines = vec![
        Line::from(""),
        Line::from(format!("  {}", g.phase_text))
            .style(Style::default().fg(Color::White)),
        Line::from(""),
        Line::from("  Filling the remaining disk space with the last partition.")
            .style(Style::default().fg(Color::DarkGray)),
        Line::from("  Filesystem checks on large disks may take minutes.")
            .style(Style::default().fg(Color::DarkGray)),
        Line::from(""),
        Line::from("  Do NOT power off during this step.")
            .style(Style::default().fg(Color::Yellow)),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

// ═══════════════════════════════════════════════════════════════════════
// Write error dialog
// ═══════════════════════════════════════════════════════════════════════

fn render_write_error_dialog(app: &App, frame: &mut Frame) {
    let area = centered_rect(56, 10, frame.area());
    let block = dialog_block(" Write Failed ", Color::Red);

    let disk_info = app.progress.as_ref()
        .map(|p| format!("/dev/{} ({})", p.disk_name, p.disk_model))
        .unwrap_or_default();

    render_dialog(frame, area, block, vec![
        Line::from(""),
        Line::from("Installation failed!")
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            .centered(),
        Line::from(""),
        Line::from(format!("Target: {}", disk_info))
            .style(Style::default().fg(Color::White))
            .centered(),
        Line::from(""),
        Line::from("The disk may be damaged or the image was not written")
            .style(Style::default().fg(Color::DarkGray))
            .centered(),
        Line::from("completely. Please verify and try again.")
            .style(Style::default().fg(Color::DarkGray))
            .centered(),
        Line::from(""),
        Line::from("Press Enter or Esc to return to disk list")
            .style(Style::default().fg(Color::Yellow))
            .centered(),
    ]);
}

// ═══════════════════════════════════════════════════════════════════════
// Success / reboot screen
// ═══════════════════════════════════════════════════════════════════════

/// Wrap text at word boundaries to fit the dialog width.
fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + word.chars().count() < width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// One-line grow result for the Success screen; Partial additionally
/// carries self-contained manual recovery commands (tmpfs data is lost
/// on reboot, so the command must be visible before the user leaves).
fn grow_result_lines(app: &App) -> Vec<Line<'static>> {
    let Some(o) = app.grow_outcome.as_ref() else {
        return vec![];
    };

    let (color, label) = match o.status {
        Status::Expanded => (Color::Green, "Auto-expand"),
        Status::Skipped => (Color::Yellow, "Auto-expand skipped"),
        Status::Partial => (Color::Yellow, "Auto-expand partial"),
        Status::Failed | Status::Disabled => (Color::Red, "Auto-expand failed"),
    };

    let mut lines = vec![];
    let summary = match o.status {
        Status::Expanded => {
            let size = if o.old_bytes > 0 {
                format!("{} → {}", format_bytes(o.old_bytes), format_bytes(o.new_bytes))
            } else {
                format_bytes(o.new_bytes)
            };
            format!("last partition grown to {}", size)
        }
        _ => o.reason.clone(),
    };
    lines.push(
        Line::from(format!(" {}: {} ", label, summary))
            .style(Style::default().fg(color))
            .centered(),
    );

    if !o.manual_cmd.is_empty() {
        lines.push(
            Line::from(" Manual recovery (run after reboot):")
                .style(Style::default().fg(Color::Yellow))
                .centered(),
        );
        for chunk in wrap_words(&o.manual_cmd, 48) {
            lines.push(
                Line::from(format!("   {chunk}"))
                    .style(Style::default().fg(Color::Yellow)),
            );
        }
    }
    lines
}

fn render_success_screen(app: &App, frame: &mut Frame) {
    let grow_lines = grow_result_lines(app);
    let height = (14 + grow_lines.len() as u16).min(frame.area().height.saturating_sub(1));
    let area = centered_rect(56, height, frame.area());
    let block = dialog_block(" Installation Successful ", app.theme.success);

    let reboot_btn = if app.success_action == SuccessAction::Reboot && !app.reboot_counting {
        Span::styled(" Reboot ", Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
    } else if app.reboot_counting {
        Span::styled(" Reboot ", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(" Reboot ", Style::default().fg(Color::Cyan))
    };

    let back_btn = if app.success_action == SuccessAction::Back && !app.reboot_counting {
        Span::styled(" Back ", Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" Back ", Style::default().fg(Color::DarkGray))
    };

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(""),
        Line::from("Image has been written successfully!")
            .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            .centered(),
        Line::from(""),
    ];

    if !grow_lines.is_empty() {
        lines.extend(grow_lines);
        lines.push(Line::from(""));
    }

    lines.extend(vec![
        Line::from("NOTICE BEFORE REBOOTING:")
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .centered(),
        Line::from(""),
        Line::from("  * Please REMOVE the USB drive or Disc.")
            .style(Style::default().fg(Color::White)),
        Line::from("  * Ensure media is removed to avoid boot loops.")
            .style(Style::default().fg(Color::White)),
        Line::from(""),
    ]);

    if app.reboot_counting {
        lines.push(
            Line::from(format!("  Rebooting in {} seconds... (Enter to skip)", app.reboot_countdown))
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                .centered(),
        );
        lines.push(Line::from(""));
    } else {
        lines.push(
            Line::from(vec![
                Span::raw("  "),
                reboot_btn,
                Span::raw("    "),
                back_btn,
            ])
            .centered(),
        );
        lines.push(Line::from(""));
    }

    render_dialog(frame, area, block, lines);
}

// ═══════════════════════════════════════════════════════════════════════
// Help dialog
// ═══════════════════════════════════════════════════════════════════════

fn render_help_dialog(frame: &mut Frame) {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(20), Constraint::Fill(1)])
        .split(frame.area());

    let area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(60), Constraint::Fill(1)])
        .split(popup_layout[1])[1];

    let help_text = vec![
        Line::from("Navigation:").bold().yellow(),
        Line::from("  j / ↓         - Scroll down"),
        Line::from("  k / ↑         - Scroll up"),
        Line::from("  Enter         - Select disk to write"),
        Line::from(""),
        Line::from("Actions:").bold().yellow(),
        Line::from("  r             - Refresh disk list"),
        Line::from("  q             - Shut down"),
        Line::from("  Esc / a       - Abort write operation"),
        Line::from(""),
        Line::from("Confirmation:").bold().yellow(),
        Line::from("  ← → or Tab    - Toggle No/Yes"),
        Line::from("  Enter         - Confirm selection"),
        Line::from(""),
        Line::from("Press any key to close").centered().italic(),
    ];

    let block = Paragraph::new(help_text)
        .block(
            Block::default()
                .title(" IMG Installer - Help ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_type(BorderType::default())
                .border_style(Style::default().fg(Color::Green)),
        )
        .style(Style::default().fg(Color::White));

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
}

// ═══════════════════════════════════════════════════════════════════════
// Toast notifications
// ═══════════════════════════════════════════════════════════════════════

fn render_notification(notification: &crate::notification::Notification, index: usize, frame: &mut Frame) {
    use crate::notification::NotificationLevel;

    let (color, title) = match notification.level {
        NotificationLevel::Info => (Color::Green, "Info"),
        NotificationLevel::Warning => (Color::Yellow, "Warning"),
        NotificationLevel::Error => (Color::Red, "Error"),
    };

    let text = ratatui::text::Text::from(vec![
        Line::from(title).style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Line::from(notification.message.as_str()),
    ]);

    let h = (text.height() as u16).saturating_add(2);
    let w = (text.width() as u16).saturating_add(4).min(frame.area().width.saturating_sub(2));

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(h * index as u16), Constraint::Length(h), Constraint::Min(1)])
        .split(frame.area());

    let area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(w), Constraint::Length(2)])
        .split(popup_layout[1])[1];

    let block = Paragraph::new(text)
        .block(
            Block::default()
                .title(ratatui::text::Span::styled(title, Style::default().fg(color)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color)),
        )
        .style(Style::default().fg(color));

    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
}