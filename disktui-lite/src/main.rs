use std::io::{self, Write};
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use disktui_lite::app::{App, AppResult, ExitAction};
use disktui_lite::handler::{handle_key_events, poll_dd_progress};
#[cfg(target_os = "linux")]
use disktui_lite::init;
use disktui_lite::tui::Tui;
use disktui_lite::ui;

fn main() -> AppResult<()> {
    // Self-fork: if invoked with --dd, run as dd subprocess and exit.
    // This gives us process isolation for uu_dd: we can kill it with
    // Child::kill() and read progress via /proc/$PID/io.
    if std::env::args().any(|a| a == "--dd") {
        let dd_args = std::iter::once(std::ffi::OsString::from("dd"))
            .chain(std::env::args_os()
                .skip_while(|arg| arg != "--dd")
                .skip(1));
        let code = uu_dd::uumain(dd_args);
        std::process::exit(code);
    }

    // Detect if we are PID 1 (running as init in initramfs)
    let is_pid1 = cfg!(target_os = "linux") && std::process::id() == 1;

    // Init phase (PID 1 only)
    #[cfg(target_os = "linux")]
    if is_pid1 && let Err(e) = init::run_init() {
        init::emergency_halt(&format!("Init failed: {}", e));
    }

    // Ignore SIGINT: we handle abort via UI (Esc), not Ctrl+C.
    #[cfg(unix)]
    {
        use nix::sys::signal::{SigHandler, Signal};
        unsafe {
            nix::sys::signal::signal(Signal::SIGINT, SigHandler::SigIgn)?;
        }
    }

    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;
    let mut tui = Tui::new(terminal);
    tui.init()?;

    let mut app = match App::new() {
        Ok(app) => app,
        Err(e) => {
            drop(tui);
            if is_pid1 {
                #[cfg(target_os = "linux")]
                init::emergency_halt(&format!("Failed to initialize: {}", e));
            }
            eprintln!("Failed to initialize: {}", e);
            return Err(e);
        }
    };

    // ── TUI main loop ────────────────────────────────────────────
    while app.running {
        tui.draw(|frame| ui::render(&mut app, frame))?;

        // Poll dd progress
        poll_dd_progress(&mut app);

        // Handle input (100ms timeout = tick rate)
        if crossterm::event::poll(Duration::from_millis(100))?
            && let crossterm::event::Event::Key(key) = crossterm::event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            let _ = handle_key_events(key, &mut app);
        }

        app.tick();
    }

    // ── Handle exit action ───────────────────────────────────────
    match app.exit_action {
        ExitAction::PowerOff => {
            tui.exit()?;
            print!("\x1Bc");
            println!("Shutting down...");
            io::stdout().flush().ok();
            #[cfg(target_os = "linux")]
            {
                nix::unistd::sync();
                let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
            }
            #[cfg(target_os = "linux")]
            if is_pid1 {
                init::emergency_halt("Poweroff failed");
            }
            return Ok(());
        }
        ExitAction::Reboot => {
            tui.exit()?;
            print!("\x1Bc");
            println!("Rebooting...");
            io::stdout().flush().ok();
            #[cfg(target_os = "linux")]
            {
                nix::unistd::sync();
                let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
            }
            #[cfg(target_os = "linux")]
            if is_pid1 {
                init::emergency_halt("Reboot failed");
            }
            return Ok(());
        }
        ExitAction::None => {}
    }

    #[cfg(target_os = "linux")]
    if is_pid1 {
        eprintln!("Installer exited. Rebooting in 3 seconds...");
        std::thread::sleep(std::time::Duration::from_secs(3));
        nix::unistd::sync();
        let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    Ok(())
}