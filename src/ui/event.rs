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

    if text_input || focus == Focus::Overlay {
        if let Some(action) = keymap.resolve_scoped(focus, key) {
            return Some(action);
        }
        if is_plain_text(key) {
            if let KeyCode::Char(ch) = key.code {
                return Some(Action::Input(ch));
            }
        }
    }

    keymap.resolve_for_tab(focus, active_nav_tab, key)
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
    fn composer_text_beats_global_quit() {
        let keymap = Keymap::default_teams();
        let action = key_to_action(
            press(KeyCode::Char('q'), KeyModifiers::NONE),
            Focus::Composer,
            &keymap,
            true,
        );
        assert_eq!(action, Some(Action::Input('q')));
    }

    #[test]
    fn composer_ctrl_s_toggles_subject() {
        let keymap = Keymap::default_teams();
        let action = key_to_action(
            press(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Focus::Composer,
            &keymap,
            true,
        );
        assert_eq!(action, Some(Action::ToggleSubject));
    }

    #[test]
    fn chat_q_quits_and_t_opens_thread() {
        let keymap = Keymap::default_teams();
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('q'), KeyModifiers::NONE),
                Focus::Chat,
                &keymap,
                false,
            ),
            Some(Action::Quit)
        );
        assert_eq!(
            key_to_action(
                press(KeyCode::Char('t'), KeyModifiers::NONE),
                Focus::Chat,
                &keymap,
                false,
            ),
            Some(Action::OpenThreadForSelected)
        );
    }

    #[test]
    fn teams_tabs_image_and_history_keys() {
        let keymap = Keymap::default_teams();
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('1'), KeyModifiers::CONTROL),
                Focus::Nav,
                0,
                &keymap,
                false,
            ),
            Some(Action::SelectNavTab(0))
        );
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('2'), KeyModifiers::CONTROL),
                Focus::Nav,
                0,
                &keymap,
                false,
            ),
            Some(Action::SelectNavTab(1))
        );
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('3'), KeyModifiers::CONTROL),
                Focus::Nav,
                0,
                &keymap,
                false,
            ),
            Some(Action::SelectNavTab(2))
        );
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('v'), KeyModifiers::NONE),
                Focus::Chat,
                0,
                &keymap,
                false,
            ),
            Some(Action::OpenImagePreview)
        );
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('o'), KeyModifiers::NONE),
                Focus::Chat,
                0,
                &keymap,
                false,
            ),
            Some(Action::LoadOlder)
        );
        assert_eq!(
            key_to_action_for_tab(
                press(KeyCode::Char('H'), KeyModifiers::SHIFT),
                Focus::Chat,
                0,
                &keymap,
                false,
            ),
            Some(Action::SyncHistory)
        );
    }
}
