//! TUI entry shell: terminal raw mode, mouse capture, keyboard enhancement, event loop, external editor.

use std::borrow::Cow;
use std::io::{self, BufWriter, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
#[cfg(not(windows))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch};

use crate::auth::AuthManager;
use crate::config::{AppConfig, ImageProtocolPreference, Persona};
use crate::graph::GraphClient;
use crate::img_preview::{
    self, ActivePixelGraphic, TerminalGraphicsCaps, TerminalImageProtocol,
};
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, FocusTarget, SyncCommand, SyncController};
use crate::ui::{
    self, action,
    action::Action,
    app::{AppState, UiMsg, UiState},
    composer, event,
    overlays::Overlay,
};

static KITTY_USED_GLOBAL: AtomicBool = AtomicBool::new(false);

/// VT DECSET sequences for X10 + button-motion + SGR mouse reporting.
/// `crossterm 0.28` skips ANSI mouse codes on Windows, so writing these explicitly
/// ensures ConPTY / Windows Terminal / xterm.js report mouse wheel events cleanly.
pub(crate) const VT_ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(crate) const VT_DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
pub(crate) const VT_ENABLE_FOCUS: &str = "\x1b[?1004h";
pub(crate) const VT_DISABLE_FOCUS: &str = "\x1b[?1004l";

pub(crate) fn write_vt_mouse_enable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_ENABLE_MOUSE.as_bytes())?;
    w.flush()
}

pub(crate) fn write_vt_mouse_disable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_DISABLE_MOUSE.as_bytes())?;
    w.flush()
}

pub(crate) fn write_vt_focus_enable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_ENABLE_FOCUS.as_bytes())?;
    w.flush()
}

pub(crate) fn write_vt_focus_disable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_DISABLE_FOCUS.as_bytes())?;
    w.flush()
}

#[cfg(windows)]
mod win_console {
    use std::ptr::null_mut;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;

    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    pub const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

    /// Raw-mode + ConPTY mouse capture console input mode (`0x0098`):
    /// enables `ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS`,
    /// explicitly clearing `ENABLE_QUICK_EDIT_MODE`, `ENABLE_VIRTUAL_TERMINAL_INPUT`,
    /// `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT`, and `ENABLE_PROCESSED_INPUT`.
    pub const TUI_MOUSE_CAPTURE_MODE: u32 =
        ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
    }

    fn with_conin_handle<T>(f: impl FnOnce(isize) -> Option<T>) -> Option<T> {
        const CONIN_WIDE: [u16; 7] = [
            b'C' as u16,
            b'O' as u16,
            b'N' as u16,
            b'I' as u16,
            b'N' as u16,
            b'$' as u16,
            0,
        ];
        let conin = unsafe {
            CreateFileW(
                CONIN_WIDE.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if conin != 0 && conin != INVALID_HANDLE_VALUE {
            let res = f(conin);
            unsafe {
                CloseHandle(conin);
            }
            return res;
        }
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if std_in != 0 && std_in != INVALID_HANDLE_VALUE {
            f(std_in)
        } else {
            None
        }
    }

    pub fn snapshot_console_mode() -> Option<u32> {
        with_conin_handle(|h| {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                Some(mode)
            } else {
                None
            }
        })
    }

    pub fn enforce_windows_mouse_capture() {
        let _ = with_conin_handle(|h| {
            unsafe {
                SetConsoleMode(h, TUI_MOUSE_CAPTURE_MODE);
            }
            Some(())
        });
    }

    pub fn restore_console_mode(orig_mode: Option<u32>) {
        if let Some(mode) = orig_mode {
            let _ = with_conin_handle(|h| {
                unsafe {
                    SetConsoleMode(h, mode | ENABLE_EXTENDED_FLAGS);
                }
                Some(())
            });
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PixelReconcilePlan {
    erase_and_redraw: bool,
    delete_kitty: bool,
    erase_rects: Vec<Rect>,
    emit_desired: bool,
}

#[derive(Debug, Default)]
struct PixelLayerTracker {
    active: Vec<ActivePixelGraphic>,
    last_term_size: (u16, u16),
}

impl PixelLayerTracker {
    fn plan_transition(
        &self,
        desired: &[ActivePixelGraphic],
        term_size: (u16, u16),
        kitty_used: bool,
    ) -> PixelReconcilePlan {
        let term_resized = self.last_term_size != term_size;
        if self.active == desired && !term_resized {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: false,
            };
        }

        let had_active = !self.active.is_empty();
        let delete_kitty = had_active
            && (kitty_used
                || self
                    .active
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty));
        let erase_rects = self
            .active
            .iter()
            .map(|prev| {
                Rect::new(
                    prev.rect.x,
                    prev.rect.y,
                    prev.rect.width.max(prev.cols),
                    prev.rect.height.max(prev.rows),
                )
            })
            .collect();

        PixelReconcilePlan {
            erase_and_redraw: had_active,
            delete_kitty,
            erase_rects,
            emit_desired: !desired.is_empty(),
        }
    }

    fn commit(&mut self, desired: Vec<ActivePixelGraphic>, term_size: (u16, u16)) {
        self.active = desired;
        self.last_term_size = term_size;
    }

    fn clear(&mut self) {
        self.active.clear();
    }
}

/// Overlay Z-order guard: blocks inline pixel graphics whenever any modal overlay is active,
/// allows `ImagePreview` modal graphics only when `ImagePreview` is topmost,
/// allows `HtmlPreview` inline graphics (`html:...`) only when `HtmlPreview` is topmost,
/// and blocks everything when another modal (e.g., `ConfirmDialog`) sits on top.
fn pixel_graphics_allowed_for_overlays(overlays: &[Overlay], cache_key: &str) -> bool {
    if overlays.is_empty() {
        !cache_key.starts_with("modal:") && !cache_key.starts_with("html:")
    } else if matches!(overlays.last(), Some(Overlay::ImagePreview(_))) {
        !cache_key.starts_with("inline:") && !cache_key.starts_with("html:")
    } else if matches!(overlays.last(), Some(Overlay::HtmlPreview(_))) {
        cache_key.starts_with("html:")
    } else {
        false
    }
}

fn parse_pixel_key(key: &str) -> (&str, Option<(u16, u16, u16, u16)>) {
    if let Some((base, crop_str)) = key.rsplit_once("#crop:") {
        let mut parts = crop_str.split(',');
        if let (Some(sx), Some(sy), Some(vw), Some(vh), None) = (
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next(),
        ) {
            return (base, Some((sx, sy, vw, vh)));
        }
    }
    (key, None)
}

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
    pixel_tracker: PixelLayerTracker,
    #[cfg(windows)]
    orig_conin_mode: Option<u32>,
}

type TuiTerminal = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

impl TerminalGuard {
    fn enter(
        image_protocol: ImageProtocolPreference,
    ) -> Result<(Self, TuiTerminal, TerminalGraphicsCaps)> {
        #[cfg(windows)]
        let orig_conin_mode = win_console::snapshot_console_mode();

        enable_raw_mode()?;
        // Probe graphics & cell pixel dimensions while raw mode is active, BEFORE entering
        // alternate screen / mouse capture so Windows VT probe never clobbers ConPTY mouse mode.
        let graphics_caps = img_preview::probe_terminal_graphics_caps(image_protocol);

        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        #[cfg(windows)]
        {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableFocusChange
            )?;
            write_vt_mouse_enable(&mut stdout)?;
            let _ = write_vt_focus_enable(&mut stdout);
        }

        let keyboard_enhanced = if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )
            .is_ok()
        } else {
            false
        };

        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
            let _ = write_vt_focus_enable(&mut stdout);
        }

        let backend = CrosstermBackend::new(BufWriter::with_capacity(64 * 1024, stdout));
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
                pixel_tracker: PixelLayerTracker::default(),
                #[cfg(windows)]
                orig_conin_mode,
            },
            terminal,
            graphics_caps,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used || KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_focus_disable(&mut stdout);
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(
                stdout,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(self.orig_conin_mode);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool, orig_conin_mode: Option<u32>) {
    #[cfg(not(windows))]
    let _ = orig_conin_mode;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_focus_disable(&mut stdout);
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(
                stdout,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(orig_conin_mode);
        prev(info);
    }));
}

const MAX_COALESCED_EVENTS: usize = 64;
const ACTIVITY_THROTTLE: Duration = Duration::from_secs(5);
const FOCUS_SYNC_DEBOUNCE: Duration = Duration::from_secs(2);
const PASTE_BURST_POLL: Duration = Duration::from_millis(2);
const MAX_PASTE_BURST_EVENTS: usize = 4096;
const EVENT_READER_POLL: Duration = Duration::from_millis(25);

enum ReaderMsg {
    Event(CrosstermEvent),
    Eof,
}

/// Lock-free terminal event reader backed by a dedicated OS thread and Tokio unbounded channel.
///
/// Avoids `crossterm::event::EventStream::next().now_or_never()`, which on `crossterm 0.28`
/// poisons `EventStream`'s background waker with `noop_waker` (causing up to 200ms timer-tick
/// input stalls) and blocks the main thread on `INTERNAL_EVENT_READER` on Windows until the next
/// console event arrives.
struct TerminalEventReader {
    rx: mpsc::UnboundedReceiver<ReaderMsg>,
    tx: mpsc::UnboundedSender<ReaderMsg>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TerminalEventReader {
    fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut reader = Self {
            rx,
            tx,
            stop,
            handle: None,
        };
        reader.resume();
        reader
    }

    #[cfg(test)]
    fn from_channel(
        tx: mpsc::UnboundedSender<ReaderMsg>,
        rx: mpsc::UnboundedReceiver<ReaderMsg>,
    ) -> Self {
        Self {
            rx,
            tx,
            stop: Arc::new(AtomicBool::new(true)),
            handle: None,
        }
    }

    fn resume(&mut self) {
        if self.handle.is_some() {
            return;
        }
        self.stop.store(false, Ordering::Release);
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        self.handle = std::thread::Builder::new()
            .name("tui-event-reader".to_string())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match crossterm::event::poll(EVENT_READER_POLL) {
                        Ok(true) => match crossterm::event::read() {
                            Ok(ev) => {
                                if tx.send(ReaderMsg::Event(ev)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(ReaderMsg::Eof);
                                break;
                            }
                        },
                        Ok(false) => continue,
                        Err(_) => {
                            let _ = tx.send(ReaderMsg::Eof);
                            break;
                        }
                    }
                }
            })
            .ok();
    }

    fn pause(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    async fn recv(&mut self) -> Option<CrosstermEvent> {
        match self.rx.recv().await {
            Some(ReaderMsg::Event(ev)) => Some(ev),
            Some(ReaderMsg::Eof) | None => None,
        }
    }

    fn try_recv(&mut self) -> std::result::Result<CrosstermEvent, mpsc::error::TryRecvError> {
        match self.rx.try_recv() {
            Ok(ReaderMsg::Event(ev)) => Ok(ev),
            Ok(ReaderMsg::Eof) => Err(mpsc::error::TryRecvError::Disconnected),
            Err(err) => Err(err),
        }
    }
}

impl Drop for TerminalEventReader {
    fn drop(&mut self) {
        self.pause();
    }
}

#[derive(Debug, Default)]
struct PasteBurstDetector {
    keys: Vec<KeyEvent>,
}

impl PasteBurstDetector {
    fn is_candidate_key(key: &KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Enter => key.modifiers == KeyModifiers::NONE,
            KeyCode::Char(ch) => {
                !ch.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            }
            _ => false,
        }
    }

    fn push(&mut self, key: KeyEvent) {
        self.keys.push(key);
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn into_paste_or_keys(mut self) -> std::result::Result<String, Vec<KeyEvent>> {
        let has_enter = self
            .keys
            .iter()
            .any(|k| k.code == KeyCode::Enter && k.modifiers == KeyModifiers::NONE);
        let has_char = self
            .keys
            .iter()
            .any(|k| matches!(k.code, KeyCode::Char(_)));
        if self.keys.len() >= 2 && has_enter && has_char {
            let mut out = String::with_capacity(self.keys.len());
            for k in self.keys.drain(..) {
                match k.code {
                    KeyCode::Char(ch) => out.push(ch),
                    KeyCode::Enter => out.push('\n'),
                    _ => {}
                }
            }
            Ok(out)
        } else {
            Err(self.keys)
        }
    }
}

enum LoopStep {
    Continue,
    Quit,
}

fn should_flush_frame_after_event(
    ev: &CrosstermEvent,
    prev_overlay_depth: usize,
    prev_tab: usize,
    prev_fullscreen: bool,
    prev_narrow: ui::app::NarrowPane,
    ui: &UiState,
) -> bool {
    if matches!(ev, CrosstermEvent::Resize(_, _)) {
        return true;
    }
    if matches!(
        ev,
        CrosstermEvent::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::Down(_)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
            )
    ) {
        return true;
    }
    ui.overlays.len() != prev_overlay_depth
        || ui.active_nav_tab != prev_tab
        || ui.medium_reader_fullscreen != prev_fullscreen
        || ui.narrow_pane != prev_narrow
}

async fn process_terminal_event_with_burst(
    first_ev: CrosstermEvent,
    events: &mut TerminalEventReader,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
    last_activity_sent: &mut Option<Instant>,
    last_focus_sync: &mut Option<Instant>,
) -> LoopStep {
    let CrosstermEvent::Key(first_key) = first_ev else {
        return handle_terminal_event(
            first_ev,
            app,
            ui,
            dirty,
            last_activity_sent,
            last_focus_sync,
        );
    };
    if !ui.accepts_text_input() || !PasteBurstDetector::is_candidate_key(&first_key) {
        return handle_terminal_event(
            CrosstermEvent::Key(first_key),
            app,
            ui,
            dirty,
            last_activity_sent,
            last_focus_sync,
        );
    }

    let mut detector = PasteBurstDetector::default();
    detector.push(first_key);
    let mut trailing_event: Option<CrosstermEvent> = None;
    let mut stream_ended = false;

    while detector.len() < MAX_PASTE_BURST_EVENTS {
        let has_enter = detector
            .keys
            .iter()
            .any(|k| k.code == KeyCode::Enter && k.modifiers == KeyModifiers::NONE);
        let next_ev = match events.try_recv() {
            Ok(ev) => Some(ev),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                stream_ended = true;
                break;
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                if !has_enter {
                    break;
                }
                match tokio::time::timeout(PASTE_BURST_POLL, events.recv()).await {
                    Ok(Some(ev)) => Some(ev),
                    Ok(None) => {
                        stream_ended = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
        };
        match next_ev {
            Some(CrosstermEvent::Key(k)) if k.kind == KeyEventKind::Release => {
                continue;
            }
            Some(CrosstermEvent::Key(k)) if PasteBurstDetector::is_candidate_key(&k) => {
                detector.push(k);
            }
            Some(other) => {
                trailing_event = Some(other);
                break;
            }
            None => break,
        }
    }

    match detector.into_paste_or_keys() {
        Ok(coalesced) => {
            notify_activity_throttled(app, last_activity_sent);
            *dirty = true;
            if action::dispatch(Action::Paste(coalesced), app, ui) {
                return LoopStep::Quit;
            }
        }
        Err(keys) => {
            for k in keys {
                if matches!(
                    handle_terminal_event(
                        CrosstermEvent::Key(k),
                        app,
                        ui,
                        dirty,
                        last_activity_sent,
                        last_focus_sync
                    ),
                    LoopStep::Quit
                ) {
                    return LoopStep::Quit;
                }
            }
        }
    }

    if let Some(extra) = trailing_event {
        if matches!(
            handle_terminal_event(
                extra,
                app,
                ui,
                dirty,
                last_activity_sent,
                last_focus_sync
            ),
            LoopStep::Quit
        ) {
            return LoopStep::Quit;
        }
    }

    if stream_ended {
        LoopStep::Quit
    } else {
        LoopStep::Continue
    }
}

fn notify_activity_throttled(app: &AppState, last_activity_sent: &mut Option<Instant>) {
    if last_activity_sent.is_none_or(|t| t.elapsed() >= ACTIVITY_THROTTLE) {
        app.sync_ctrl.request(SyncCommand::NotifyUserActivity);
        *last_activity_sent = Some(Instant::now());
    }
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<Stdout>>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
) -> Result<()> {
    let _ = queue!(terminal.backend_mut(), BeginSynchronizedUpdate);

    let draw_res: Result<()> = (|| {
        let mut cursor_pos = None;
        let mut cursor_set_in_draw = false;
        terminal.draw(|frame| {
            cursor_pos = ui::draw(frame, app, ui);
            if ui.pending_pixel_graphics.is_empty() {
                if let Some((cx, cy)) = cursor_pos {
                    frame.set_cursor_position((cx, cy));
                    cursor_set_in_draw = true;
                }
            }
        })?;

        let term_size = (ui.term_width, ui.term_height);
        let mut desired: Vec<ActivePixelGraphic> = Vec::new();
        let mut desired_payloads: Vec<Option<String>> = Vec::new();

        for pg in ui.pending_pixel_graphics.clone() {
            if !pixel_graphics_allowed_for_overlays(&ui.overlays, &pg.cache_key) {
                continue;
            }
            // Never allow any pixel graphic to touch the bottom-most status row.
            let safe_max_h = ui.term_height.saturating_sub(1).saturating_sub(pg.rect.y);
            let safe_max_w = ui.term_width.saturating_sub(pg.rect.x);
            if let Some(payload) = pg.payload {
                let draw_cols = pg.rect.width;
                let draw_rows = pg.rect.height;
                if draw_cols > 0
                    && draw_rows > 0
                    && draw_cols <= safe_max_w
                    && draw_rows <= safe_max_h
                {
                    let placement_rect = Rect::new(pg.rect.x, pg.rect.y, draw_cols, draw_rows);
                    desired.push(ActivePixelGraphic {
                        cache_key: pg.cache_key,
                        rect: placement_rect,
                        protocol: pg.protocol,
                        cols: draw_cols,
                        rows: draw_rows,
                        payload_len: payload.len(),
                    });
                    desired_payloads.push(Some(payload));
                }
            } else {
                // Defer expensive `viewport_pixel_payload` encoding until after `plan_transition`
                // confirms pixels must actually be emitted on this frame.
                let (base_key, crop) = parse_pixel_key(&pg.cache_key);
                if let Some(rendered) = app.image_cache.get(base_key) {
                    if let Some(payload_len) = rendered.pixel_payload.as_deref().map(str::len) {
                        let dims = match crop {
                            Some((sx, sy, vw, vh)) => rendered
                                .viewport_cell_dims(sx, sy, vw, vh)
                                .or_else(|| {
                                    (rendered.cols <= vw && rendered.rows <= vh)
                                        .then_some((rendered.cols, rendered.rows))
                                }),
                            None => Some((rendered.cols, rendered.rows)),
                        };
                        if let Some((draw_cols, draw_rows)) = dims {
                            if draw_cols > 0
                                && draw_rows > 0
                                && draw_cols <= safe_max_w
                                && draw_rows <= safe_max_h
                            {
                                let placement_rect =
                                    Rect::new(pg.rect.x, pg.rect.y, draw_cols, draw_rows);
                                desired.push(ActivePixelGraphic {
                                    cache_key: pg.cache_key,
                                    rect: placement_rect,
                                    protocol: pg.protocol,
                                    cols: draw_cols,
                                    rows: draw_rows,
                                    payload_len,
                                });
                                desired_payloads.push(None);
                            }
                        }
                    }
                }
            }
        }

        fn resolve_pixel_payload<'a>(
            graphic: &ActivePixelGraphic,
            prebuilt: Option<String>,
            app: &'a AppState,
        ) -> Option<Cow<'a, str>> {
            if let Some(p) = prebuilt {
                return Some(Cow::Owned(p));
            }
            let (base_key, crop) = parse_pixel_key(&graphic.cache_key);
            app.image_cache.get(base_key).and_then(|rendered| {
                if let Some((sx, sy, vw, vh)) = crop {
                    rendered
                        .viewport_pixel_payload(sx, sy, vw, vh)
                        .map(|(p, _, _)| Cow::Owned(p))
                        .or_else(|| {
                            if rendered.cols <= vw && rendered.rows <= vh {
                                rendered.pixel_payload.as_deref().map(Cow::Borrowed)
                            } else {
                                None
                            }
                        })
                } else {
                    rendered.pixel_payload.as_deref().map(Cow::Borrowed)
                }
            })
        }

        // Fast-path: avoid full `terminal.clear()` flicker when only the `#crop:...` viewport
        // changed on the exact same rect, base image, and terminal size for Sixel.
        let sixel_crop_only_update = guard.pixel_tracker.last_term_size == term_size
            && guard.pixel_tracker.active.len() == 1
            && desired.len() == 1
            && guard.pixel_tracker.active[0].protocol == TerminalImageProtocol::Sixel
            && desired[0].protocol == TerminalImageProtocol::Sixel
            && guard.pixel_tracker.active[0].rect == desired[0].rect
            && guard.pixel_tracker.active[0].cache_key != desired[0].cache_key
            && parse_pixel_key(&guard.pixel_tracker.active[0].cache_key).0
                == parse_pixel_key(&desired[0].cache_key).0;

        let mut pixels_touched = false;
        if sixel_crop_only_update {
            let prebuilt = desired_payloads.into_iter().next().flatten();
            if let Some(payload) = resolve_pixel_payload(&desired[0], prebuilt, app) {
                let graphic = &desired[0];
                crossterm::queue!(
                    terminal.backend_mut(),
                    crossterm::cursor::MoveTo(graphic.rect.x, graphic.rect.y)
                )?;
                terminal.backend_mut().write_all(payload.as_bytes())?;
                terminal.backend_mut().flush()?;
                ui.active_pixel_graphics =
                    vec![(graphic.cache_key.clone(), graphic.rect, graphic.protocol)];
                guard.pixel_tracker.commit(desired, term_size);
                pixels_touched = true;
            }
        } else {
            let plan = guard
                .pixel_tracker
                .plan_transition(&desired, term_size, guard.kitty_used);

            if plan.erase_and_redraw {
                pixels_touched = true;
                if plan.delete_kitty {
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_all_sequence().as_bytes())?;
                }
                for rect in &plan.erase_rects {
                    let erase = img_preview::erase_pixel_rect_sequence(
                        rect.x,
                        rect.y,
                        rect.width,
                        rect.height,
                        ui.term_width,
                        ui.term_height,
                    );
                    if !erase.is_empty() {
                        terminal.backend_mut().write_all(erase.as_bytes())?;
                    }
                }
                terminal.backend_mut().flush()?;
                terminal.clear()?;
                ui.active_pixel_graphics.clear();
                terminal.draw(|frame| {
                    cursor_pos = ui::draw(frame, app, ui);
                })?;
            }

            if plan.emit_desired {
                pixels_touched = true;
                let any_kitty = desired
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty);
                if any_kitty {
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_all_sequence().as_bytes())?;
                    guard.kitty_used = true;
                    KITTY_USED_GLOBAL.store(true, Ordering::Relaxed);
                }
                let mut committed: Vec<ActivePixelGraphic> = Vec::with_capacity(desired.len());
                for (idx, (graphic, prebuilt)) in
                    desired.into_iter().zip(desired_payloads).enumerate()
                {
                    let Some(payload) = resolve_pixel_payload(&graphic, prebuilt, app) else {
                        continue;
                    };
                    let emitted: Cow<'_, str> =
                        if graphic.protocol == TerminalImageProtocol::Kitty && idx > 0 {
                            let from = format!(",i={},", img_preview::KITTY_PREVIEW_IMAGE_ID);
                            let to = format!(
                                ",i={},",
                                img_preview::KITTY_PREVIEW_IMAGE_ID.saturating_add(idx as u32)
                            );
                            Cow::Owned(payload.replacen(&from, &to, 1))
                        } else {
                            payload
                        };
                    crossterm::queue!(
                        terminal.backend_mut(),
                        crossterm::cursor::MoveTo(graphic.rect.x, graphic.rect.y)
                    )?;
                    terminal.backend_mut().write_all(emitted.as_bytes())?;
                    committed.push(graphic);
                }
                terminal.backend_mut().flush()?;
                ui.active_pixel_graphics = committed
                    .iter()
                    .map(|g| (g.cache_key.clone(), g.rect, g.protocol))
                    .collect();
                guard.pixel_tracker.commit(committed, term_size);
            } else {
                if plan.delete_kitty {
                    guard.kitty_used = false;
                    ui.active_pixel_graphics.clear();
                } else if desired.is_empty() {
                    ui.active_pixel_graphics.clear();
                }
                guard.pixel_tracker.commit(desired, term_size);
            }
        }

        if pixels_touched || !cursor_set_in_draw {
            if let Some((cx, cy)) = cursor_pos {
                let _ = terminal.show_cursor();
                let _ = terminal.set_cursor_position((cx, cy));
            } else {
                let _ = terminal.hide_cursor();
            }
        }
        Ok(())
    })();

    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_res
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
    last_activity_sent: &mut Option<Instant>,
    last_focus_sync: &mut Option<Instant>,
) -> LoopStep {
    match ev {
        CrosstermEvent::Key(key) => {
            let act = event::key_to_action_for_tab(
                key,
                ui.focus(),
                ui.active_nav_tab,
                &ui.keymap,
                ui.accepts_text_input(),
            )
            .or_else(|| {
                (key.kind == crossterm::event::KeyEventKind::Press
                    && key.code == KeyCode::BackTab)
                    .then_some(Action::PrevFocus)
            });
            if let Some(act) = act {
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
            #[cfg(not(windows))]
            {
                let _ = img_preview::refresh_cell_pixel_size(&mut ui.graphics_caps);
                if matches!(
                    ui.overlays.last(),
                    Some(ui::overlays::Overlay::ImagePreview(_))
                ) {
                    let _ = action::dispatch(Action::LoadPreviewImage, app, ui);
                }
            }
            #[cfg(windows)]
            {
                ui.cell_px_refresh_pending = true;
            }
        }
        CrosstermEvent::FocusGained => {
            let now = Instant::now();
            app.sync_ctrl.notify_activity();
            *last_activity_sent = Some(now);
            if last_focus_sync.is_none_or(|t| now.saturating_duration_since(t) >= FOCUS_SYNC_DEBOUNCE)
            {
                *last_focus_sync = Some(now);
                app.request_sync_for_active_view(ui.active_nav_tab);
                if let Some(Overlay::MailCompose(c)) = ui.overlays.last() {
                    if c.field <= 2 {
                        app.request_contacts_sync();
                    }
                }
            }
        }
        _ => {}
    }
    LoopStep::Continue
}

struct SyncSupervisorArgs {
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
    ui_tx: mpsc::UnboundedSender<UiMsg>,
    proxy_focus_rx: watch::Receiver<FocusTarget>,
    proxy_nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
}

fn spawn_sync_supervisor(args: SyncSupervisorArgs) {
    let SyncSupervisorArgs {
        config,
        auth,
        graph,
        storage,
        core_tx,
        ui_tx,
        mut proxy_focus_rx,
        mut proxy_nudge_rx,
    } = args;
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
            if initial_focus != FocusTarget::default() {
                conn.sync_ctrl.set_focus(initial_focus);
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
                        conn.sync_ctrl.set_focus(focus);
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
        if current_focus != FocusTarget::default() {
            local_ctrl.set_focus(current_focus);
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
                    local_ctrl.set_focus(focus);
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
    // Enter raw mode, probe graphics caps BEFORE alternate screen / mouse capture,
    // and enforce Windows CONIN$ mouse capture.
    let (mut guard, mut terminal, graphics_caps) = TerminalGuard::enter(config.image_protocol)?;
    #[cfg(windows)]
    let orig_conin_mode = guard.orig_conin_mode;
    #[cfg(not(windows))]
    let orig_conin_mode = None;
    install_panic_hook(guard.keyboard_enhanced, orig_conin_mode);

    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let (proxy_focus_tx, proxy_focus_rx) = watch::channel(FocusTarget::default());
    let (proxy_nudge_tx, proxy_nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(proxy_focus_tx, proxy_nudge_tx);

    // Drafts written before per-account draft files existed go to the first account that opens the TUI.
    crate::config::adopt_legacy_drafts(graph.my_user_id());

    let mut ui = UiState::from_config(&config);
    if let Ok(sz) = terminal.size() {
        ui.term_width = sz.width;
        ui.term_height = sz.height;
    }
    ui.graphics_caps = graphics_caps;
    ui.kitty_keyboard_supported = guard.keyboard_enhanced;
    let mut app = AppState::new(
        config.clone(),
        auth.clone(),
        graph.clone(),
        storage.clone(),
        sync_ctrl.clone(),
        ui_tx.clone(),
    );
    app.load_cached_startup(&mut ui);
    draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;

    spawn_sync_supervisor(SyncSupervisorArgs {
        config,
        auth,
        graph,
        storage,
        core_tx,
        ui_tx,
        proxy_focus_rx,
        proxy_nudge_rx,
    });

    let mut events = TerminalEventReader::start();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_activity_sent: Option<Instant> = None;
    let mut last_focus_sync: Option<Instant> = None;

    loop {
        if ui.editor_requested {
            ui.editor_requested = false;
            events.pause();
            if guard.kitty_used
                || ui
                    .active_pixel_graphics
                    .iter()
                    .any(|(_, _, p)| *p == TerminalImageProtocol::Kitty)
            {
                let _ = terminal
                    .backend_mut()
                    .write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
                let _ = terminal
                    .backend_mut()
                    .write_all(img_preview::kitty_delete_all_sequence().as_bytes());
                let _ = terminal.backend_mut().flush();
            }
            for prev in &guard.pixel_tracker.active {
                let erase = img_preview::erase_pixel_rect_sequence(
                    prev.rect.x,
                    prev.rect.y,
                    prev.rect.width.max(prev.cols),
                    prev.rect.height.max(prev.rows),
                    ui.term_width,
                    ui.term_height,
                );
                if !erase.is_empty() {
                    let _ = terminal.backend_mut().write_all(erase.as_bytes());
                }
            }
            let _ = terminal.backend_mut().flush();
            guard.pixel_tracker.clear();
            ui.active_pixel_graphics.clear();
            guard.restore();
            let initial_body = match ui.overlays.last() {
                Some(Overlay::MailCompose(c)) => c.body.text().to_string(),
                _ => String::new(),
            };
            let result = composer::external_edit(&initial_body);
            let mut stdout = io::stdout();
            enable_raw_mode()?;
            #[cfg(not(windows))]
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste,
                EnableFocusChange
            )?;
            #[cfg(windows)]
            {
                execute!(
                    stdout,
                    EnterAlternateScreen,
                    EnableMouseCapture,
                    EnableFocusChange
                )?;
                write_vt_mouse_enable(&mut stdout)?;
                let _ = write_vt_focus_enable(&mut stdout);
            }
            if guard.keyboard_enhanced {
                let _ = execute!(
                    stdout,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    )
                );
            }
            #[cfg(windows)]
            {
                win_console::enforce_windows_mouse_capture();
                let _ = write_vt_mouse_enable(&mut stdout);
                let _ = write_vt_focus_enable(&mut stdout);
            }
            terminal.clear()?;
            events.resume();
            dirty = true;

            match result {
                Ok(edited) => {
                    if let Some(ui::overlays::Overlay::MailCompose(ref mut c)) =
                        ui.overlays.last_mut()
                    {
                        c.body.load_text(&edited);
                    }
                    ui.info("Updated email body from external editor");
                }
                Err(err) => {
                    ui.error(format!("External editor error: {err}"));
                }
            }
        }

        #[cfg(windows)]
        if ui.cell_px_refresh_pending {
            ui.cell_px_refresh_pending = false;
            events.pause();
            let _ = img_preview::refresh_cell_pixel_size(&mut ui.graphics_caps);
            win_console::enforce_windows_mouse_capture();
            let mut stdout = io::stdout();
            let _ = write_vt_mouse_enable(&mut stdout);
            let _ = write_vt_focus_enable(&mut stdout);
            events.resume();
            dirty = true;
            if matches!(
                ui.overlays.last(),
                Some(ui::overlays::Overlay::ImagePreview(_))
            ) {
                let _ = action::dispatch(Action::LoadPreviewImage, &mut app, &mut ui);
            }
        }

        if dirty {
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;
            dirty = false;
        }

        let mut should_quit = false;
        let mut flush_frame = false;
        tokio::select! {
            _ = tick.tick() => {
                let before = ui.toasts.len();
                ui.toasts.tick(std::time::Instant::now());
                if ui.toasts.len() != before {
                    dirty = true;
                }
                if ui.poll_retrying || app.syncing || ui.tasks.first_running().is_some() {
                    ui.spinner_tick = ui.spinner_tick.wrapping_add(1);
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
            maybe_ev = events.recv() => {
                match maybe_ev {
                    Some(ev) => {
                        let prev_overlays = ui.overlays.len();
                        let prev_tab = ui.active_nav_tab;
                        let prev_fullscreen = ui.medium_reader_fullscreen;
                        let prev_narrow = ui.narrow_pane;
                        let ev_needs_flush = matches!(ev, CrosstermEvent::Resize(_, _))
                            || matches!(
                                &ev,
                                CrosstermEvent::Mouse(m)
                                    if matches!(
                                        m.kind,
                                        MouseEventKind::Down(_)
                                            | MouseEventKind::ScrollUp
                                            | MouseEventKind::ScrollDown
                                    )
                            );
                        if matches!(
                            process_terminal_event_with_burst(
                                ev,
                                &mut events,
                                &mut app,
                                &mut ui,
                                &mut dirty,
                                &mut last_activity_sent,
                                &mut last_focus_sync
                            )
                            .await,
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                        }
                        if ev_needs_flush
                            || ui.overlays.len() != prev_overlays
                            || ui.active_nav_tab != prev_tab
                            || ui.medium_reader_fullscreen != prev_fullscreen
                            || ui.narrow_pane != prev_narrow
                        {
                            flush_frame = true;
                        }
                    }
                    None => should_quit = true,
                }
            }
        }
        if should_quit {
            break;
        }

        // Coalesce burst events (fast typing, sync batches) before redrawing,
        // but flush immediately on clicks, scrolls, resizes, or overlay/layout changes
        // so hitmaps and viewport rects are never stale for the next event.
        let mut drained = 0usize;
        while drained < MAX_COALESCED_EVENTS && !ui.editor_requested && !flush_frame {
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
            while drained < MAX_COALESCED_EVENTS && !ui.editor_requested && !flush_frame {
                match events.try_recv() {
                    Ok(ev) => {
                        drained += 1;
                        progressed = true;
                        let prev_overlays = ui.overlays.len();
                        let prev_tab = ui.active_nav_tab;
                        let prev_fullscreen = ui.medium_reader_fullscreen;
                        let prev_narrow = ui.narrow_pane;
                        let ev_clone_flush = should_flush_frame_after_event(
                            &ev,
                            prev_overlays,
                            prev_tab,
                            prev_fullscreen,
                            prev_narrow,
                            &ui,
                        );
                        if matches!(
                            process_terminal_event_with_burst(
                                ev,
                                &mut events,
                                &mut app,
                                &mut ui,
                                &mut dirty,
                                &mut last_activity_sent,
                                &mut last_focus_sync
                            )
                            .await,
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                            break;
                        }
                        if ev_clone_flush
                            || ui.overlays.len() != prev_overlays
                            || ui.active_nav_tab != prev_tab
                            || ui.medium_reader_fullscreen != prev_fullscreen
                            || ui.narrow_pane != prev_narrow
                        {
                            flush_frame = true;
                            break;
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        should_quit = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            if should_quit || !progressed || flush_frame {
                break;
            }
        }
        if should_quit {
            break;
        }
    }

    if let Some(ui::overlays::Overlay::MailCompose(c)) = ui.overlays.last() {
        app.stash_compose_draft(c)?;
    }
    sync_ctrl.request(SyncCommand::Shutdown);
    events.pause();
    drop(guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn paste_burst_detector_coalesces_multiline_burst() {
        let mut d = PasteBurstDetector::default();
        for ch in ['h', 'i'] {
            let k = press(KeyCode::Char(ch), KeyModifiers::NONE);
            assert!(PasteBurstDetector::is_candidate_key(&k));
            d.push(k);
        }
        d.push(press(KeyCode::Enter, KeyModifiers::NONE));
        d.push(press(KeyCode::Char('b'), KeyModifiers::SHIFT));
        assert_eq!(d.into_paste_or_keys(), Ok("hi\nb".to_string()));
    }

    #[test]
    fn paste_burst_detector_preserves_single_enter_and_plain_typing() {
        let mut single_enter = PasteBurstDetector::default();
        single_enter.push(press(KeyCode::Enter, KeyModifiers::NONE));
        assert!(single_enter.into_paste_or_keys().is_err());

        let mut plain_typing = PasteBurstDetector::default();
        plain_typing.push(press(KeyCode::Char('a'), KeyModifiers::NONE));
        plain_typing.push(press(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(plain_typing.into_paste_or_keys().is_err());

        let mut only_enters = PasteBurstDetector::default();
        only_enters.push(press(KeyCode::Enter, KeyModifiers::NONE));
        only_enters.push(press(KeyCode::Enter, KeyModifiers::NONE));
        assert!(only_enters.into_paste_or_keys().is_err());

        assert!(!PasteBurstDetector::is_candidate_key(&press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!PasteBurstDetector::is_candidate_key(&press(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
    }

    #[test]
    fn test_parse_pixel_key_extracts_base_and_crop() {
        assert_eq!(
            parse_pixel_key("msg-1:att-1:92x100:sixel#crop:2,10,80,25"),
            ("msg-1:att-1:92x100:sixel", Some((2, 10, 80, 25)))
        );
        assert_eq!(
            parse_pixel_key("msg-1:att-1:92x100:sixel"),
            ("msg-1:att-1:92x100:sixel", None)
        );
        assert_eq!(
            parse_pixel_key("msg-1:att-1:92x100:sixel#crop:bad"),
            ("msg-1:att-1:92x100:sixel#crop:bad", None)
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_mouse_capture_mode_bits() {
        // 0x0098 = ENABLE_WINDOW_INPUT (0x0008) | ENABLE_MOUSE_INPUT (0x0010) | ENABLE_EXTENDED_FLAGS (0x0080)
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE, 0x0098);
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0040, 0); // QuickEdit cleared
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0200, 0); // VTInput cleared
    }

    #[test]
    fn test_vt_mouse_enable_and_disable_ansi_sequences() {
        let mut enable_buf = Vec::new();
        write_vt_mouse_enable(&mut enable_buf).unwrap();
        assert_eq!(
            String::from_utf8(enable_buf).unwrap(),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );

        let mut disable_buf = Vec::new();
        write_vt_mouse_disable(&mut disable_buf).unwrap();
        assert_eq!(
            String::from_utf8(disable_buf).unwrap(),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );

        let mut focus_en = Vec::new();
        write_vt_focus_enable(&mut focus_en).unwrap();
        assert_eq!(String::from_utf8(focus_en).unwrap(), "\x1b[?1004h");

        let mut focus_dis = Vec::new();
        write_vt_focus_disable(&mut focus_dis).unwrap();
        assert_eq!(String::from_utf8(focus_dis).unwrap(), "\x1b[?1004l");
    }

    #[test]
    fn test_focus_gained_triggers_active_view_sync_and_debounces() {
        use crate::ui::nav_pane::TAB_CALENDAR;

        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _) = mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        ui.active_nav_tab = TAB_CALENDAR;

        let mut dirty = false;
        let mut last_activity = None;
        let mut last_focus = None;

        // First FocusGained wakes activity and immediately dispatches SyncCalendar
        handle_terminal_event(
            CrosstermEvent::FocusGained,
            &mut app,
            &mut ui,
            &mut dirty,
            &mut last_activity,
            &mut last_focus,
        );
        let mut cmds = Vec::new();
        while let Ok(c) = nudge_rx.try_recv() {
            cmds.push(c);
        }
        assert!(cmds.iter().any(|c| matches!(c, SyncCommand::NotifyUserActivity)));
        assert!(cmds.iter().any(|c| matches!(c, SyncCommand::SyncCalendar)));

        // Immediate second FocusGained within debounce window skips duplicate SyncCalendar
        handle_terminal_event(
            CrosstermEvent::FocusGained,
            &mut app,
            &mut ui,
            &mut dirty,
            &mut last_activity,
            &mut last_focus,
        );
        let mut cmds2 = Vec::new();
        while let Ok(c) = nudge_rx.try_recv() {
            cmds2.push(c);
        }
        assert!(!cmds2.iter().any(|c| matches!(c, SyncCommand::SyncCalendar)));
    }

    fn sample_graphic(
        key: &str,
        rect: Rect,
        protocol: TerminalImageProtocol,
        payload_len: usize,
    ) -> ActivePixelGraphic {
        ActivePixelGraphic {
            cache_key: key.to_string(),
            rect,
            protocol,
            cols: rect.width,
            rows: rect.height,
            payload_len,
        }
    }

    #[test]
    fn test_pixel_tracker_image_to_non_image_triggers_erase_and_redraw() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "msg-1:att-1:80x24:sixel#crop:0,0,80,24",
            Rect::new(4, 3, 60, 20),
            TerminalImageProtocol::Sixel,
            4096,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(&[], (120, 36), false);
        assert!(plan.erase_and_redraw);
        assert!(!plan.delete_kitty);
        assert_eq!(plan.erase_rects, vec![img_a.rect]);
        assert!(!plan.emit_desired);
    }

    #[test]
    fn test_pixel_tracker_image_a_to_image_b_erases_and_emits_b() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "msg-1:att-1:80x24:kitty",
            Rect::new(4, 3, 60, 20),
            TerminalImageProtocol::Kitty,
            4096,
        );
        let img_b = sample_graphic(
            "msg-2:att-2:80x24:kitty",
            Rect::new(4, 3, 50, 18),
            TerminalImageProtocol::Kitty,
            3072,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(&[img_b], (120, 36), true);
        assert!(plan.erase_and_redraw);
        assert!(plan.delete_kitty);
        assert_eq!(plan.erase_rects, vec![img_a.rect]);
        assert!(plan.emit_desired);
    }

    #[test]
    fn test_pixel_tracker_unchanged_spinner_tick_skips_erase_and_reemit() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "msg-1:att-1:80x24:sixel",
            Rect::new(4, 3, 60, 20),
            TerminalImageProtocol::Sixel,
            4096,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(std::slice::from_ref(&img_a), (120, 36), false);
        assert!(!plan.erase_and_redraw);
        assert!(!plan.delete_kitty);
        assert!(plan.erase_rects.is_empty());
        assert!(!plan.emit_desired);

        // Terminal resize with the same rect still forces erase + redraw + re-emit
        let resize_plan = tracker.plan_transition(&[img_a], (130, 36), false);
        assert!(resize_plan.erase_and_redraw);
        assert!(resize_plan.emit_desired);
    }

    #[test]
    fn test_overlay_z_order_guard_suppresses_pixels_under_non_image_modals() {
        use crate::models::{MailAttachment, MailMessage};
        use crate::ui::overlays::image_preview::ImagePreviewOverlay;

        let msg = MailMessage {
            id: "m1".into(),
            internet_message_id: None,
            conversation_id: None,
            folder_id: Some("inbox".into()),
            subject: "Photos".into(),
            from: None,
            to_recipients: vec![],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            reply_to: vec![],
            received_at: "2026-11-20T00:00:00Z".into(),
            sent_at: None,
            last_modified_at: None,
            is_read: false,
            is_draft: false,
            flagged: false,
            importance: "normal".into(),
            inference_classification: None,
            has_attachments: true,
            body_loaded: true,
            preview: String::new(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![MailAttachment {
                id: "att1".into(),
                name: "photo.png".into(),
                content_type: "image/png".into(),
                size: 1024,
                is_inline: false,
                content_id: None,
                content_bytes_b64: None,
            }],
            meeting_request: None,
            web_link: None,
            ..Default::default()
        };
        let modal = ImagePreviewOverlay::from_mail_message(&msg, None).unwrap();

        // ImagePreview on top: modal allowed, inline blocked
        let only_img = vec![Overlay::ImagePreview(modal.clone())];
        assert!(pixel_graphics_allowed_for_overlays(
            &only_img,
            "m1:att1:80x24:sixel#crop:0,0,80,24"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &only_img,
            "inline:m1:att1"
        ));

        // CommandPalette active: blocked
        let cmd_palette = vec![Overlay::command_palette()];
        assert!(!pixel_graphics_allowed_for_overlays(
            &cmd_palette,
            "m1:att1:80x24:sixel"
        ));

        // ConfirmDialog stacked over ImagePreview: blocked so Sixel/Kitty gets erased cleanly
        let stacked = vec![
            Overlay::ImagePreview(modal),
            Overlay::confirm("Quit Outlook?", None),
        ];
        assert!(!pixel_graphics_allowed_for_overlays(
            &stacked,
            "m1:att1:80x24:sixel"
        ));

        // HtmlPreview on top: html: graphics allowed, inline: blocked; ConfirmDialog over HtmlPreview blocks html:
        let html_only = vec![Overlay::html_preview(
            crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(
                Some("Chart".into()),
                "<p>Hi</p>".into(),
            ),
        )];
        assert!(pixel_graphics_allowed_for_overlays(
            &html_only,
            "html:3:80x8:sixel:crop:0,0,80,8"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &html_only,
            "inline:m1:att1"
        ));
        let html_stacked = vec![
            Overlay::html_preview(
                crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(
                    Some("Chart".into()),
                    "<p>Hi</p>".into(),
                ),
            ),
            Overlay::confirm("Quit Outlook?", None),
        ];
        assert!(!pixel_graphics_allowed_for_overlays(
            &html_stacked,
            "html:3:80x8:sixel:crop:0,0,80,8"
        ));
    }

    #[test]
    fn test_coalesce_loop_flushes_on_mouse_down_scroll_resize_and_overlay_change() {
        use crossterm::event::{MouseButton, MouseEvent};

        let mut ui = UiState::new();
        let plain_key = CrosstermEvent::Key(press(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(!should_flush_frame_after_event(
            &plain_key,
            0,
            ui.active_nav_tab,
            ui.medium_reader_fullscreen,
            ui.narrow_pane,
            &ui,
        ));

        let click = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(should_flush_frame_after_event(
            &click,
            0,
            ui.active_nav_tab,
            ui.medium_reader_fullscreen,
            ui.narrow_pane,
            &ui,
        ));

        let scroll = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(should_flush_frame_after_event(
            &scroll,
            0,
            ui.active_nav_tab,
            ui.medium_reader_fullscreen,
            ui.narrow_pane,
            &ui,
        ));

        let resize = CrosstermEvent::Resize(100, 30);
        assert!(should_flush_frame_after_event(
            &resize,
            0,
            ui.active_nav_tab,
            ui.medium_reader_fullscreen,
            ui.narrow_pane,
            &ui,
        ));

        // Overlay opened during key dispatch also forces flush
        ui.push_overlay(Overlay::settings());
        assert!(should_flush_frame_after_event(
            &plain_key,
            0,
            ui.active_nav_tab,
            ui.medium_reader_fullscreen,
            ui.narrow_pane,
            &ui,
        ));
    }

    #[tokio::test]
    async fn terminal_event_reader_try_recv_is_non_blocking_and_wakes_immediately() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut reader = TerminalEventReader::from_channel(tx.clone(), rx);

        // Empty queue returns immediately without blocking.
        let t0 = Instant::now();
        assert!(matches!(
            reader.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(t0.elapsed() < Duration::from_millis(5));

        // Async recv wakes immediately on single keystroke without waiting for any tick.
        let tx_bg = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = tx_bg.send(ReaderMsg::Event(CrosstermEvent::Key(press(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            ))));
        });
        let t1 = Instant::now();
        let ev = tokio::time::timeout(Duration::from_millis(100), reader.recv())
            .await
            .expect("recv must wake immediately");
        assert!(matches!(
            ev,
            Some(CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Char('a'),
                ..
            }))
        ));
        assert!(t1.elapsed() < Duration::from_millis(80));

        // Subsequent try_recv does not poison future async recv wakers.
        assert!(matches!(
            reader.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let _ = tx.send(ReaderMsg::Event(CrosstermEvent::Key(press(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        ))));
        let ev2 = tokio::time::timeout(Duration::from_millis(50), reader.recv())
            .await
            .expect("second recv must wake immediately after try_recv");
        assert!(matches!(
            ev2,
            Some(CrosstermEvent::Key(KeyEvent {
                code: KeyCode::Char('b'),
                ..
            }))
        ));

        // Explicit EOF reports clean termination even while sender handle exists.
        let _ = tx.send(ReaderMsg::Eof);
        assert!(reader.recv().await.is_none());
    }
}
