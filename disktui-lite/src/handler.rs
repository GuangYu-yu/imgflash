use anyhow::Context;

use crate::app::{App, ConfirmButton, GrowProgress, Screen, SuccessAction, WriteProgress};
use crate::grow::{self, GrowOutcome, Status};

pub fn handle_key_events(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    // Help overlay: any key dismisses
    if app.show_help {
        app.show_help = false;
        return Ok(());
    }

    match app.screen {
        Screen::DiskList => handle_disk_list(key, app),
        Screen::Confirmation => handle_confirmation(key, app),
        Screen::Writing => handle_writing(key, app),
        // No abort during grow: killing the child mid-sfdisk would deliberately
        // create the torn partition-table window (accepted only for power loss)
        Screen::Growing => Ok(()),
        Screen::WriteError => handle_write_error(key, app),
        Screen::Success => handle_success(key, app),
    }
}

// ── Disk list ───────────────────────────────────────────────────────────

fn handle_disk_list(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Char('q') => {
            app.quit();
        }
        KeyCode::Char('?') => {
            app.show_help = true;
        }
        KeyCode::Char('r') => {
            if let Err(e) = app.refresh_disks() {
                app.notify(format!("Failed to refresh: {}", e), crate::notification::NotificationLevel::Error);
            } else {
                app.notify("Disk list refreshed", crate::notification::NotificationLevel::Info);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.disks.is_empty() {
                let i = app.disks_state.selected().unwrap_or(0);
                let next = (i + 1).min(app.disks.len() - 1);
                app.disks_state.select(Some(next));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !app.disks.is_empty() {
                let i = app.disks_state.selected().unwrap_or(0);
                let prev = i.saturating_sub(1);
                app.disks_state.select(Some(prev));
            }
        }
        KeyCode::Enter => {
            try_select_disk(app);
        }
        _ => {}
    }
    Ok(())
}

fn try_select_disk(app: &mut App) {
    let disk = match app.selected_disk() {
        Some(d) => d.clone(),
        None => return,
    };

    if disk.is_mounted {
        app.notify(
            format!("/dev/{} is currently mounted!", disk.name),
            crate::notification::NotificationLevel::Error,
        );
        return;
    }

    if !app.image_exists() {
        app.notify(
            "Image file not found at /image/image.img",
            crate::notification::NotificationLevel::Error,
        );
        return;
    }

    if let Some(img_size) = app.image_file_size()
        && disk.size_bytes < img_size
    {
        app.notify(
            "Disk is smaller than the image! Write will fail.",
            crate::notification::NotificationLevel::Error,
        );
        return;
    }

    app.goto_confirmation();
}

// ── Confirmation ────────────────────────────────────────────────────────

/// Handle Left/Right/Tab for Yes/No dialog navigation.
fn handle_dialog_toggle(key: &crossterm::event::KeyEvent, btn: &mut ConfirmButton) {
    use crossterm::event::KeyCode;
    match key.code {
        KeyCode::Left | KeyCode::Char('h') => *btn = ConfirmButton::No,
        KeyCode::Right | KeyCode::Char('l') => *btn = ConfirmButton::Yes,
        KeyCode::Tab => btn.toggle(),
        _ => {}
    }
}

fn handle_confirmation(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    use crossterm::event::KeyCode;

    handle_dialog_toggle(&key, &mut app.confirm_button);

    if key.code == KeyCode::Enter {
        if app.confirm_button == ConfirmButton::Yes {
            if let Err(e) = start_write(app) {
                app.notify(format!("Failed to start write: {}", e), crate::notification::NotificationLevel::Error);
                app.goto_disk_list();
            }
        } else {
            app.goto_disk_list();
        }
    }
    Ok(())
}

// ── Writing (progress) ──────────────────────────────────────────────────

fn handle_writing(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Esc | KeyCode::Char('a') => {
            abort_write(app);
        }
        _ => {}
    }
    Ok(())
}

// ── Write error ─────────────────────────────────────────────────────────

fn handle_write_error(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Enter | KeyCode::Esc => {
            app.goto_disk_list();
        }
        _ => {}
    }
    Ok(())
}

// ── Success (reboot countdown) ──────────────────────────────────────────

fn handle_success(key: crossterm::event::KeyEvent, app: &mut App) -> anyhow::Result<()> {
    use crossterm::event::KeyCode;

    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
            if !app.reboot_counting {
                app.success_action.toggle();
            }
        }
        KeyCode::Enter => {
            if !app.reboot_counting {
                match app.success_action {
                    SuccessAction::Reboot => {
                        app.reboot_counting = true;
                        app.reboot_last_tick = app.tick_count;
                    }
                    SuccessAction::Back => {
                        app.goto_disk_list();
                    }
                }
            } else {
                app.skip_reboot_countdown();
            }
        }
        _ => {}
    }
    Ok(())
}

// ── Start dd write ──────────────────────────────────────────────────────
//
// We spawn ourselves with --dd flag, which makes the child process
// invoke uu_dd::uumain() and exit. This gives us process isolation:
// - Child::kill() can truly abort the write
// - /proc/$PID/io gives clean per-process wchar for progress tracking

fn start_write(app: &mut App) -> anyhow::Result<()> {
    let disk = match app.selected_disk() {
        Some(d) => d.clone(),
        None => return Ok(()),
    };

    let img_bytes = app.image_file_size()
        .context("Failed to get image file size")?;

    // Spawn self as dd subprocess
    let child = std::process::Command::new("/proc/self/exe")
        .arg("--dd")
        .arg(format!("if={}", App::IMAGE_FILE))
        .arg(format!("of=/dev/{}", disk.name))
        .arg("bs=4M")
        .arg("conv=fsync,sparse")
        .arg("status=none")
        .spawn()
        .context("Failed to spawn dd process")?;

    let progress = WriteProgress::new(&disk, img_bytes, child);
    app.goto_writing(progress);

    Ok(())
}

/// Abort write: kill the dd child process and return to disk list.
fn abort_write(app: &mut App) {
    if let Some(ref mut p) = app.progress {
        p.abort();
        app.notify("Write operation aborted.", crate::notification::NotificationLevel::Warning);
    }
    app.goto_disk_list();
}

// ── Poll dd progress (called every tick) ────────────────────────────────

pub fn poll_dd_progress(app: &mut App) {
    if app.screen != Screen::Writing {
        return;
    }

    let progress = app.progress.as_mut().unwrap();

    if progress.finished {
        return;
    }

    let (process_status, written) = progress.check_and_read_io();
    progress.update_progress(written, App::TICK_INTERVAL.as_secs_f64());

    match process_status {
        Some(true) => {
            // dd success → grow (if enabled) → Success; grow never blocks reboot
            if grow::quick_enabled() {
                start_grow(app);
            } else {
                app.goto_success();
            }
        }
        Some(false) => {
            app.goto_write_error();
        }
        None => {}
    }
}

// ── Start grow subprocess ───────────────────────────────────────────────
//
// Spawns /proc/self/exe --grow <disk> after a successful dd write.
// Same self-fork pattern as start_write: process isolation, and the
// outcome is reported via /run/grow.result (atomic writes).

fn start_grow(app: &mut App) {
    let disk_name = app.progress.as_ref()
        .map(|p| p.disk_name.clone())
        .unwrap_or_default();

    // Delete stale IPC files from any earlier run so the result we read
    // later can only belong to this run
    for f in [grow::STATUS_FILE, grow::RESULT_FILE, grow::LOG_FILE] {
        let _ = std::fs::remove_file(f);
    }

    match std::process::Command::new("/proc/self/exe")
        .arg("--grow")
        .arg(format!("/dev/{}", disk_name))
        .spawn()
    {
        Ok(child) => {
            app.goto_growing(GrowProgress::new(&disk_name, child));
        }
        Err(_) => {
            // Spawn failure = infrastructure fault: record Failed and move on
            app.grow_outcome = Some(GrowOutcome {
                status: Status::Failed,
                device: format!("/dev/{}", disk_name),
                reason: "failed to spawn grow process".to_string(),
                ..GrowOutcome::default()
            });
            app.goto_success();
        }
    }
}

// ── Poll grow progress (called every tick) ──────────────────────────────

pub fn poll_grow_progress(app: &mut App) {
    if app.screen != Screen::Growing {
        return;
    }
    let Some(g) = app.grow.as_mut() else {
        return;
    };

    g.poll_status();
    let exited = g.child_exited();
    let timed_out = !exited && g.stalled_for() > grow::HANG_TIMEOUT;
    if !exited && !timed_out {
        return;
    }
    let disk_name = g.disk_name.clone();
    // borrow of `g` ends here — `app` is fully available again

    let outcome = if exited {
        // Child exited: result must exist (atomic write precedes exit);
        // missing result = crash → synthesize Failed
        grow::read_result().unwrap_or_else(|| GrowOutcome {
            status: Status::Failed,
            device: format!("/dev/{}", disk_name),
            reason: "grow process exited without result".to_string(),
            ..GrowOutcome::default()
        })
    } else {
        // Hang: no status change for HANG_TIMEOUT while still running.
        // The child is NOT killed — a kill mid-sfdisk would deliberately
        // create the torn partition-table window; reboot carries the same
        // accepted risk.
        GrowOutcome {
            status: Status::Failed,
            device: format!("/dev/{}", disk_name),
            reason: "grow timed out (no progress)".to_string(),
            ..GrowOutcome::default()
        }
    };

    // Disabled is internal: no grow line on the Success screen
    app.grow_outcome = (outcome.status != Status::Disabled).then_some(outcome);
    app.goto_success();
}