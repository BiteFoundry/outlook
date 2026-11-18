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
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{AppConfig, Persona};
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

const MAX_COALESCED_EVENTS: usize = 64;

enum LoopStep {
    Continue,
    Quit,
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
) -> LoopStep {
    match ev {
        CrosstermEvent::Key(key) => {
            if let Some(act) = event::key_to_action_for_tab(
                key,
                ui.focus(),
                ui.active_nav_tab,
                &ui.keymap,
                ui.accepts_text_input(),
            ) {
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return LoopStep::Quit;
                }
            }
        }
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                *dirty = true;
                if action::dispatch(Action::ScrollUp, app, ui) {
                    return LoopStep::Quit;
                }
            }
            MouseEventKind::ScrollDown => {
                *dirty = true;
                if action::dispatch(Action::ScrollDown, app, ui) {
                    return LoopStep::Quit;
                }
            }
            _ => {
                if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                    *dirty = true;
                    if action::dispatch(act, app, ui) {
                        return LoopStep::Quit;
                    }
                }
            }
        },
        CrosstermEvent::Paste(text) => {
            *dirty = true;
            if action::dispatch(Action::Paste(text), app, ui) {
                return LoopStep::Quit;
            }
        }
        CrosstermEvent::Resize(w, h) => {
            ui.term_width = w;
            ui.term_height = h;
            *dirty = true;
        }
        _ => {}
    }
    LoopStep::Continue
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    run_with_persona(config, Persona::detect(), auth, graph, storage).await
}

pub async fn run_with_persona(
    config: AppConfig,
    persona: Persona,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let sync_ctrl = sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), core_tx);

    let mut ui = UiState::from_config_with_persona(&config, persona);
    let mut app = AppState::new_with_persona(
        config,
        persona,
        auth,
        graph,
        storage,
        sync_ctrl.clone(),
        ui_tx,
    );
    app.load_cached_initial();
    app.spawn_bootstrap(&mut ui);

    let (guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut dirty = true;

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
            dirty = true;

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

        if let Some((page_key, initial_md)) = ui.onenote_external_edit.take() {
            guard.restore();
            let result = composer::external_edit(&initial_md);
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
            dirty = true;

            match result {
                Ok(edited) => {
                    if edited.trim() != initial_md.trim() {
                        app.replace_onenote_page_body(&mut ui, page_key, edited);
                    } else {
                        ui.info("변경 사항이 없어 저장하지 않았습니다");
                    }
                }
                Err(err) => {
                    ui.error(format!("외부 편집기 실행 실패: {err}"));
                }
            }
        }

        if dirty {
            let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
            let mut cursor_pos = None;
            let draw_res = terminal
                .draw(|frame| {
                    cursor_pos = ui::draw(frame, &mut app, &mut ui);
                })
                .map(|_| ());
            if draw_res.is_ok() {
                if let Some((cx, cy)) = cursor_pos {
                    let _ = terminal.show_cursor();
                    let _ = terminal.set_cursor_position((cx, cy));
                } else {
                    let _ = terminal.hide_cursor();
                }
                dirty = false;
            }
            let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
            draw_res?;
        }

        let mut should_quit = false;
        tokio::select! {
            _ = tick.tick() => {
                let before = ui.toasts.len();
                ui.toasts.tick(std::time::Instant::now());
                if ui.toasts.len() != before {
                    dirty = true;
                }
            }
            maybe_core = core_rx.recv() => {
                match maybe_core {
                    Some(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                    }
                    None => should_quit = true,
                }
            }
            maybe_ui = ui_rx.recv() => {
                if let Some(msg) = maybe_ui {
                    app.handle_ui_msg(&mut ui, msg);
                    dirty = true;
                }
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        if matches!(
                            handle_terminal_event(ev, &mut app, &mut ui, &mut dirty),
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                        }
                    }
                    Some(Err(_)) | None => should_quit = true,
                }
            }
        }
        if should_quit {
            break;
        }

        // Coalesce burst events (scroll wheel, fast typing, sync batches) before redrawing.
        let mut drained = 0usize;
        while drained < MAX_COALESCED_EVENTS
            && !ui.editor_requested
            && ui.onenote_external_edit.is_none()
        {
            let mut progressed = false;
            while drained < MAX_COALESCED_EVENTS {
                match core_rx.try_recv() {
                    Ok(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        should_quit = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            if should_quit {
                break;
            }
            while drained < MAX_COALESCED_EVENTS {
                match ui_rx.try_recv() {
                    Ok(msg) => {
                        app.handle_ui_msg(&mut ui, msg);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(_) => break,
                }
            }
            while drained < MAX_COALESCED_EVENTS
                && !ui.editor_requested
                && ui.onenote_external_edit.is_none()
            {
                match events.next().now_or_never() {
                    Some(Some(Ok(ev))) => {
                        drained += 1;
                        progressed = true;
                        if matches!(
                            handle_terminal_event(ev, &mut app, &mut ui, &mut dirty),
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                            break;
                        }
                    }
                    Some(Some(Err(_)) | None) => {
                        should_quit = true;
                        break;
                    }
                    None => break,
                }
            }
            if should_quit || !progressed {
                break;
            }
        }
        if should_quit {
            break;
        }
    }

    app.stash_draft(&ui);
    let _ = sync_ctrl.request(SyncCommand::Shutdown);
    drop(guard);
    Ok(())
}
