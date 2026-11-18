//! TUI entry shell: terminal raw mode, mouse capture, keyboard enhancement, event loop, external editor.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

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
use tokio::sync::{mpsc, watch};

use crate::auth::AuthManager;
use crate::config::{AppConfig, Persona};
use crate::graph::GraphClient;
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, FocusTarget, SyncCommand, SyncController};
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
const ACTIVITY_THROTTLE: Duration = Duration::from_secs(5);

enum LoopStep {
    Continue,
    Quit,
}

fn notify_activity_throttled(app: &AppState, last_activity_sent: &mut Option<Instant>) {
    if last_activity_sent.is_none_or(|t| t.elapsed() >= ACTIVITY_THROTTLE) {
        app.sync_ctrl.request(SyncCommand::NotifyUserActivity);
        *last_activity_sent = Some(Instant::now());
    }
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    ui: &mut UiState,
) -> Result<()> {
    let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
    let mut cursor_pos = None;
    let draw_res = terminal
        .draw(|frame| {
            cursor_pos = ui::draw(frame, app, ui);
        })
        .map(|_| ());
    if draw_res.is_ok() {
        if let Some((cx, cy)) = cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }
    }
    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    Ok(draw_res?)
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
    last_activity_sent: &mut Option<Instant>,
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
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return LoopStep::Quit;
                }
            }
        }
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(
                    Action::MouseScroll {
                        delta: -3,
                        column: mouse.column,
                        row: mouse.row,
                    },
                    app,
                    ui,
                ) {
                    return LoopStep::Quit;
                }
            }
            MouseEventKind::ScrollDown => {
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(
                    Action::MouseScroll {
                        delta: 3,
                        column: mouse.column,
                        row: mouse.row,
                    },
                    app,
                    ui,
                ) {
                    return LoopStep::Quit;
                }
            }
            _ => {
                if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                    notify_activity_throttled(app, last_activity_sent);
                    *dirty = true;
                    if action::dispatch(act, app, ui) {
                        return LoopStep::Quit;
                    }
                }
            }
        },
        CrosstermEvent::Paste(text) => {
            notify_activity_throttled(app, last_activity_sent);
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

fn spawn_sync_supervisor(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
    ui_tx: mpsc::UnboundedSender<UiMsg>,
    mut proxy_focus_rx: watch::Receiver<FocusTarget>,
    mut proxy_nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
) {
    tokio::spawn(async move {
        let expected_user_id = auth
            .current_tokens()
            .await
            .and_then(|t| t.user_id)
            .filter(|id| !id.is_empty());

        if core_tx.is_closed() {
            return;
        }

        if config.auto_daemon {
            let _ = crate::daemon::ensure_daemon_started(&config).await;
        }

        if core_tx.is_closed() {
            return;
        }

        let mut fell_back_from_daemon = false;

        if let Some(mut conn) =
            crate::daemon::try_connect_tui(&config, expected_user_id.as_deref()).await
        {
            let _ = ui_tx.send(UiMsg::DaemonAttached {
                pid: conn.status.pid,
                sync_busy: conn.status.sync_busy,
            });
            let initial_focus = proxy_focus_rx.borrow_and_update().clone();
            if initial_focus.target.is_some() || initial_focus.thread_root_id.is_some() {
                conn.sync_ctrl
                    .set_focus(initial_focus.target, initial_focus.thread_root_id);
            }

            loop {
                tokio::select! {
                    ev = conn.events_rx.recv() => {
                        match ev {
                            Some(core_ev) => {
                                if core_tx.send(core_ev).is_err() {
                                    return;
                                }
                            }
                            None => {
                                fell_back_from_daemon = true;
                                break;
                            }
                        }
                    }
                    cmd = proxy_nudge_rx.recv() => {
                        match cmd {
                            None | Some(SyncCommand::Shutdown) => return,
                            Some(cmd) => conn.sync_ctrl.request(cmd),
                        }
                    }
                    changed = proxy_focus_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let focus = proxy_focus_rx.borrow_and_update().clone();
                        conn.sync_ctrl.set_focus(focus.target, focus.thread_root_id);
                    }
                }
            }
        }

        if core_tx.is_closed() {
            return;
        }

        let local_ctrl = sync::spawn_sync_engine(config, graph, storage, core_tx);
        if fell_back_from_daemon {
            let _ = ui_tx.send(UiMsg::DaemonDetached);
        } else {
            let _ = ui_tx.send(UiMsg::StartLocalBootstrap);
        }

        let current_focus = proxy_focus_rx.borrow_and_update().clone();
        if current_focus.target.is_some() || current_focus.thread_root_id.is_some() {
            local_ctrl.set_focus(current_focus.target, current_focus.thread_root_id);
        }

        loop {
            tokio::select! {
                cmd = proxy_nudge_rx.recv() => {
                    match cmd {
                        None | Some(SyncCommand::Shutdown) => {
                            local_ctrl.request(SyncCommand::Shutdown);
                            break;
                        }
                        Some(cmd) => local_ctrl.request(cmd),
                    }
                }
                changed = proxy_focus_rx.changed() => {
                    if changed.is_err() {
                        local_ctrl.request(SyncCommand::Shutdown);
                        break;
                    }
                    let focus = proxy_focus_rx.borrow_and_update().clone();
                    local_ctrl.set_focus(focus.target, focus.thread_root_id);
                }
            }
        }
    });
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
    _persona: Persona,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    // Enter alternate screen and raw mode first so the terminal responds in <5ms.
    let (guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let (proxy_focus_tx, proxy_focus_rx) = watch::channel(FocusTarget::default());
    let (proxy_nudge_tx, proxy_nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(proxy_focus_tx, proxy_nudge_tx, false);

    let mut ui = UiState::from_config(&config);
    let mut app = AppState::new(
        config.clone(),
        auth.clone(),
        graph.clone(),
        storage.clone(),
        sync_ctrl.clone(),
        ui_tx.clone(),
    );
    app.load_cached_teams();
    draw_frame(&mut terminal, &mut app, &mut ui)?;

    spawn_sync_supervisor(
        config,
        auth,
        graph,
        storage,
        core_tx,
        ui_tx,
        proxy_focus_rx,
        proxy_nudge_rx,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut dirty = true;
    let mut last_activity_sent: Option<Instant> = None;

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

        if dirty {
            draw_frame(&mut terminal, &mut app, &mut ui)?;
            dirty = false;
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
                            handle_terminal_event(ev, &mut app, &mut ui, &mut dirty, &mut last_activity_sent),
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
        while drained < MAX_COALESCED_EVENTS && !ui.editor_requested {
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
            while drained < MAX_COALESCED_EVENTS && !ui.editor_requested {
                match events.next().now_or_never() {
                    Some(Some(Ok(ev))) => {
                        drained += 1;
                        progressed = true;
                        if matches!(
                            handle_terminal_event(ev, &mut app, &mut ui, &mut dirty, &mut last_activity_sent),
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
