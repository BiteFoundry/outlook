//! Terminal event (key / mouse) -> `Action` conversion.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};

use crate::ui::{action::Action, app::Focus, hitmap::HitMap, keymap::Keymap};

/// Maps a crossterm `KeyEvent` to an `Action` respecting text input focus.
pub fn key_to_action(
    key: KeyEvent,
    focus: Focus,
    keymap: &Keymap,
    text_input: bool,
) -> Option<Action> {
    key_to_action_for_tab(key, focus, 0, keymap, text_input)
}

/// Tab-aware variant of `key_to_action`.
pub fn key_to_action_for_tab(
    key: KeyEvent,
    focus: Focus,
    active_nav_tab: usize,
    keymap: &Keymap,
    text_input: bool,
) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    if text_input {
        if let Some(action) = keymap.resolve_scoped(focus, key) {
            return Some(action);
        }
        if is_plain_text(key) {
            if let KeyCode::Char(ch) = key.code {
                return Some(Action::Input(ch));
            }
        }
    }

    if let Some(action) = keymap.resolve_for_tab(focus, active_nav_tab, key) {
        return Some(action);
    }

    if focus == Focus::Overlay && is_plain_text(key) {
        if let KeyCode::Char(ch) = key.code {
            return Some(Action::Input(ch));
        }
    }

    None
}

fn is_plain_text(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(ch) if !ch.is_control())
}

pub fn mouse_to_action(mouse: MouseEvent, _hitmap: &HitMap) -> Option<Action> {
    use crossterm::event::{MouseButton, MouseEventKind};

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Action::Click {
            column: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::Down(MouseButton::Right) => Some(Action::OpenContextMenuAt {
            column: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::ScrollUp => Some(Action::MouseScroll {
            delta: -3,
            column: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::ScrollDown => Some(Action::MouseScroll {
            delta: 3,
            column: mouse.column,
            row: mouse.row,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn overlay_text_input_beats_browse_hotkeys() {
        let keymap = Keymap::default_outlook();
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('c'), KeyModifiers::NONE),
                Focus::Overlay,
                &keymap,
                true,
            ),
            Some(Action::Input('c'))
        );
        assert_eq!(
            key_to_action(
                press(KeyCode::Enter, KeyModifiers::ALT),
                Focus::Overlay,
                &keymap,
                true,
            ),
            Some(Action::OverlaySubmit)
        );
    }

    #[test]
    fn list_focus_maps_triage_and_tab_keys() {
        let keymap = Keymap::default_outlook();
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('r'), KeyModifiers::NONE),
                Focus::List,
                &keymap,
                false,
            ),
            Some(Action::MailReply { reply_all: false })
        );
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('q'), KeyModifiers::NONE),
                Focus::List,
                &keymap,
                false,
            ),
            None
        );
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('q'), KeyModifiers::CONTROL),
                Focus::List,
                &keymap,
                false,
            ),
            Some(Action::QuitConfirm)
        );
    }
}
