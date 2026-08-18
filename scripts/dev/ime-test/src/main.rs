//! Minimal TUI diagnostic: isolates whether Chinese IME causes a crossterm
//! panic/exit, or whether the problem is in the application logic.
//!
//! Mirrors Reflect-Agent's exact terminal setup and event-read approach.
//! Prints every event to the alt-screen so you can see exactly what crossterm
//! produces.  Ctrl-C twice to quit.

use std::io::{self, Write};
use std::panic;

use crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, Frame};

fn main() {
    // --- Install a panic hook that restores the terminal BEFORE printing ---
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    // --- Same terminal setup as Reflect-Agent ---
    execute!(io::stdout(), EnableBracketedPaste).unwrap();
    execute!(io::stdout(), EnterAlternateScreen).unwrap();
    terminal::enable_raw_mode().unwrap();
    execute!(io::stdout(), EnableMouseCapture).unwrap();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.clear().unwrap();

    let mut log: Vec<String> = Vec::new();
    log.push("=== IME diagnostic — type Chinese, Ctrl-C twice to quit ===".into());
    let mut pending_exit = false;

    loop {
        // Render log
        let log_clone = log.clone();
        let _ = terminal.draw(|f: &mut Frame| {
            let area = f.area();
            let text = log_clone.iter().rev().take(area.height as usize - 2).cloned().collect::<Vec<_>>().join("\n");
            f.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("IME Test")),
                area,
            );
        });

        // --- Event reader: same catch_unwind approach as Reflect-Agent ---
        match event::poll(std::time::Duration::from_millis(200)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                log.push(format!("[poll error] {e}"));
                continue;
            }
        }

        match panic::catch_unwind(event::read) {
            Ok(Ok(ev)) => {
                log.push(format_event(&ev));

                // Handle exit
                if let Event::Key(k) = &ev {
                    if k.kind != KeyEventKind::Release
                        && k.code == KeyCode::Char('c')
                        && k.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if pending_exit {
                            log.push("[Ctrl-C x2] exiting…".into());
                            break;
                        }
                        pending_exit = true;
                        log.push("[Ctrl-C] press again to exit".into());
                        continue;
                    }
                }
                // Reset exit flag on any non-Ctrl-C event
                if !is_ctrl_c(&ev) {
                    pending_exit = false;
                }
            }
            Ok(Err(e)) => {
                log.push(format!("[read error] {e}"));
            }
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                    .unwrap_or("<non-string panic>");
                log.push(format!("[!! PANIC in read()] {msg}"));
            }
        }
    }

    restore_terminal();
}

fn format_event(ev: &Event) -> String {
    match ev {
        Event::Key(k) => {
            format!(
                "Key  code={:?} mods={:?} kind={:?}",
                k.code, k.modifiers, k.kind
            )
        }
        Event::Mouse(m) => {
            format!("Mouse kind={:?} col={} row={}", m.kind, m.column, m.row)
        }
        Event::Resize(w, h) => format!("Resize {w}x{h}"),
        Event::Paste(s) => format!("Paste({})", s.chars().take(20).collect::<String>()),
        Event::FocusGained => "FocusGained".into(),
        Event::FocusLost => "FocusLost".into(),
    }
}

fn is_ctrl_c(ev: &Event) -> bool {
    matches!(ev, Event::Key(k) if k.code == KeyCode::Char('c')
        && k.modifiers.contains(KeyModifiers::CONTROL))
}

fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = io::stdout().flush();
}
