//! TUI entry shell: terminal raw mode, mouse capture, keyboard enhancement, event loop, external editor.

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, EventStream, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::AppConfig;
use crate::graph::GraphClient;
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, SyncCommand};
use crate::ui::{
    self, action,
    action::Action,
    app::{AppState, UiMsg, UiState},
    composer, event,
};

struct TerminalGuard {
    keyboard_enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let keyboard_enhanced = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )
        .is_ok();

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((Self { keyboard_enhanced }, terminal))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let sync_ctrl = sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), core_tx);

    let mut ui = UiState::from_config(&config);
    let mut app = AppState::new(config, auth, graph, storage, sync_ctrl.clone(), ui_tx);
    app.load_cached_initial();
    app.spawn_bootstrap(&mut ui);

    let (guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        if ui.editor_requested {
            ui.editor_requested = false;
            guard.restore();
            let result = composer::external_edit(ui.composer.text());
            let mut stdout = io::stdout();
            enable_raw_mode()?;
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste
            )?;
            if guard.keyboard_enhanced {
                let _ = execute!(
                    stdout,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    )
                );
            }
            terminal.clear()?;

            match result {
                Ok(edited) => {
                    ui.composer.load_text(&edited);
                    ui.info("외부 편집기 내용을 반영했습니다");
                }
                Err(err) => {
                    ui.error(format!("외부 편집기 실행 실패: {err}"));
                }
            }
        }

        let mut cursor_pos = None;
        terminal.draw(|frame| {
            cursor_pos = ui::draw(frame, &mut app, &mut ui);
        })?;
        if let Some((cx, cy)) = cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }

        tokio::select! {
            _ = tick.tick() => {
                ui.toasts.tick(std::time::Instant::now());
            }
            maybe_core = core_rx.recv() => {
                match maybe_core {
                    Some(ev) => app.handle_core_event(&mut ui, ev),
                    None => break,
                }
            }
            maybe_ui = ui_rx.recv() => {
                if let Some(msg) = maybe_ui {
                    app.handle_ui_msg(&mut ui, msg);
                }
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        if let Some(act) = event::key_to_action(
                            key,
                            ui.focus(),
                            &ui.keymap,
                            ui.accepts_text_input(),
                        ) {
                            if action::dispatch(act, &mut app, &mut ui) {
                                break;
                            }
                        }
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                if action::dispatch(Action::ScrollUp, &mut app, &mut ui) {
                                    break;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if action::dispatch(Action::ScrollDown, &mut app, &mut ui) {
                                    break;
                                }
                            }
                            _ => {
                                if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                                    if action::dispatch(act, &mut app, &mut ui) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(CrosstermEvent::Paste(text))) => {
                        if action::dispatch(Action::Paste(text), &mut app, &mut ui) {
                            break;
                        }
                    }
                    Some(Ok(CrosstermEvent::Resize(w, h))) => {
                        ui.term_width = w;
                        ui.term_height = h;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    app.stash_draft(&ui);
    let _ = sync_ctrl.request(SyncCommand::Shutdown);
    drop(guard);
    Ok(())
}
