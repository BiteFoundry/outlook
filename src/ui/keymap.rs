//! Data-driven keymap for Microsoft Outlook TUI.
//!
//! Focus-specific bindings take precedence over global bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    models::EventRsvpAction,
    ui::{action::Action, app::Focus, overlays::OverlayKind},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Key {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub focus: Option<Focus>,
    pub key: Key,
    pub action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub fn default_outlook() -> Self {
        use KeyCode::*;
        use KeyModifiers as M;

        let mut map = Keymap::default();
        let any = None;

        // --- Overlay (highest priority) ---
        map.bind(Some(Focus::Overlay), Esc, M::NONE, Action::CloseOverlay);
        map.bind(Some(Focus::Overlay), Up, M::NONE, Action::MoveUp);
        map.bind(Some(Focus::Overlay), Down, M::NONE, Action::MoveDown);
        map.bind(Some(Focus::Overlay), Left, M::NONE, Action::CursorLeft);
        map.bind(Some(Focus::Overlay), Right, M::NONE, Action::CursorRight);
        map.bind(Some(Focus::Overlay), Home, M::NONE, Action::CursorHome);
        map.bind(Some(Focus::Overlay), End, M::NONE, Action::CursorEnd);
        map.bind(Some(Focus::Overlay), Tab, M::NONE, Action::Tab);
        map.bind(Some(Focus::Overlay), BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(Some(Focus::Overlay), Enter, M::NONE, Action::Activate);
        map.bind(Some(Focus::Overlay), Enter, M::CONTROL, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Enter, M::ALT, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Enter, M::SHIFT, Action::InsertNewline);
        map.bind(Some(Focus::Overlay), Char('j'), M::CONTROL, Action::InsertNewline);
        map.bind(Some(Focus::Overlay), Char('s'), M::CONTROL, Action::SaveDraft);
        map.bind(Some(Focus::Overlay), Char('a'), M::CONTROL, Action::OpenAttachPicker);
        map.bind(Some(Focus::Overlay), Char('f'), M::CONTROL, Action::OpenFilePicker);
        map.bind(Some(Focus::Overlay), Char('e'), M::CONTROL, Action::OpenExternalEditor);
        map.bind(Some(Focus::Overlay), Char('v'), M::CONTROL, Action::PasteClipboard);
        map.bind(Some(Focus::Overlay), Char('z'), M::CONTROL, Action::Undo);
        map.bind(Some(Focus::Overlay), Char('y'), M::CONTROL, Action::Redo);
        map.bind(Some(Focus::Overlay), Char('w'), M::CONTROL, Action::DeleteWordBackward);
        map.bind(Some(Focus::Overlay), Backspace, M::NONE, Action::Backspace);
        map.bind(Some(Focus::Overlay), Delete, M::NONE, Action::DeleteForward);
        map.bind(
            Some(Focus::Overlay),
            Char('X'),
            M::CONTROL | M::SHIFT,
            Action::ClearPendingImages,
        );

        // --- Browse modes (Nav / List / Reader) ---
        for focus in [Focus::Nav, Focus::List, Focus::Reader] {
            map.bind(Some(focus), Char('1'), M::NONE, Action::SelectNavTab(0));
            map.bind(Some(focus), Char('2'), M::NONE, Action::SelectNavTab(1));
            map.bind(Some(focus), Char('3'), M::NONE, Action::SelectNavTab(2));
        }

        // Mail & shared triage bindings in List & Reader
        for focus in [Focus::List, Focus::Reader] {
            map.bind(Some(focus), Char(' '), M::NONE, Action::SpaceAction);
            map.bind(Some(focus), Char('c'), M::NONE, Action::OpenMailCompose);
            map.bind(
                Some(focus),
                Char('r'),
                M::NONE,
                Action::MailReply { reply_all: false },
            );
            map.bind(
                Some(focus),
                Char('R'),
                M::SHIFT,
                Action::MailReply { reply_all: true },
            );
            map.bind(Some(focus), Char('f'), M::NONE, Action::MailForward);
            map.bind(Some(focus), Char('e'), M::NONE, Action::MailArchive);
            map.bind(Some(focus), Delete, M::NONE, Action::DeleteItem);
            map.bind(Some(focus), Char('#'), M::NONE, Action::DeleteItem);
            map.bind(Some(focus), Char('#'), M::SHIFT, Action::DeleteItem);
            if focus == Focus::List {
                map.bind(Some(focus), Char('u'), M::NONE, Action::MailToggleRead);
            } else {
                map.bind(Some(focus), Char('u'), M::NONE, Action::OpenLinkExtractor);
            }
            map.bind(Some(focus), Char('h'), M::NONE, Action::ToggleShowLinkUrls);
            map.bind(Some(focus), Char('g'), M::NONE, Action::MailToggleFlag);
            map.bind(Some(focus), Char('m'), M::NONE, Action::OpenFolderPicker);
            map.bind(Some(focus), Char('d'), M::NONE, Action::DownloadLatestAttachment);
            map.bind(
                Some(focus),
                Char('A'),
                M::SHIFT,
                Action::OpenOverlay(OverlayKind::Attachments),
            );
            map.bind(Some(focus), Char('z'), M::NONE, Action::ToggleQuotedHistory);
            map.bind(Some(focus), Char('t'), M::NONE, Action::ToggleGroupThreads);
            map.bind(Some(focus), Char('F'), M::SHIFT, Action::ToggleFocusedOnly);
            map.bind(Some(focus), Char('U'), M::SHIFT, Action::ToggleUnreadFilter);
            map.bind(Some(focus), Char('o'), M::NONE, Action::OpenActiveLink);
            map.bind(
                Some(focus),
                Char('a'),
                M::NONE,
                Action::CalendarRsvp(EventRsvpAction::Accept),
            );
            map.bind(
                Some(focus),
                Char('y'),
                M::NONE,
                Action::CalendarRsvp(EventRsvpAction::TentativelyAccept),
            );
            map.bind(
                Some(focus),
                Char('x'),
                M::NONE,
                Action::CalendarRsvp(EventRsvpAction::Decline),
            );
            map.bind(Some(focus), Char('n'), M::NONE, Action::ContextNew);
        }

        // Nav also allows 'n' for New List / New Event, 'c' for Compose
        map.bind(Some(Focus::Nav), Char('n'), M::NONE, Action::ContextNew);
        map.bind(Some(Focus::Nav), Char('c'), M::NONE, Action::OpenMailCompose);
        map.bind(Some(Focus::Nav), Char('l'), M::NONE, Action::Activate);
        map.bind(Some(Focus::Nav), Right, M::NONE, Action::Activate);

        // --- Global shortcuts ---
        map.bind(any, Char('c'), M::CONTROL, Action::Quit);
        map.bind(any, Char('q'), M::CONTROL, Action::QuitConfirm);
        map.bind(any, Esc, M::NONE, Action::CloseOverlay);
        map.bind(any, Tab, M::NONE, Action::NextFocus);
        map.bind(any, BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(any, Enter, M::NONE, Action::Activate);
        map.bind(any, Char('k'), M::NONE, Action::MoveUp);
        map.bind(any, Char('j'), M::NONE, Action::MoveDown);
        map.bind(any, Up, M::NONE, Action::MoveUp);
        map.bind(any, Down, M::NONE, Action::MoveDown);
        map.bind(any, Home, M::NONE, Action::ScrollTop);
        map.bind(any, End, M::NONE, Action::ScrollBottom);
        map.bind(any, PageUp, M::NONE, Action::ScrollPageUp);
        map.bind(any, PageDown, M::NONE, Action::ScrollPageDown);
        map.bind(any, Char('d'), M::CONTROL, Action::ToggleDensity);
        map.bind(any, Char('r'), M::CONTROL, Action::RefreshAll);
        map.bind(any, F(5), M::NONE, Action::RefreshAll);
        map.bind(any, Char('v'), M::NONE, Action::OpenImagePreview);
        map.bind(any, Char('v'), M::ALT, Action::OpenImagePreview);

        // Tabs global (`Alt+1..3` and `Ctrl+1..3`)
        map.bind(any, Char('1'), M::ALT, Action::SelectNavTab(0));
        map.bind(any, Char('2'), M::ALT, Action::SelectNavTab(1));
        map.bind(any, Char('3'), M::ALT, Action::SelectNavTab(2));
        map.bind(any, Char('1'), M::CONTROL, Action::SelectNavTab(0));
        map.bind(any, Char('2'), M::CONTROL, Action::SelectNavTab(1));
        map.bind(any, Char('3'), M::CONTROL, Action::SelectNavTab(2));

        // Overlays & View toggles
        map.bind(
            any,
            Char('?'),
            M::NONE,
            Action::OpenOverlay(OverlayKind::Help),
        );
        map.bind(any, Char('/'), M::NONE, Action::OpenSearch);
        map.bind(any, Char('f'), M::CONTROL, Action::OpenSearch);
        map.bind(
            any,
            Char('k'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::CommandPalette),
        );
        map.bind(
            any,
            Char(','),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::Settings),
        );
        map.bind(any, Char('b'), M::CONTROL, Action::ToggleNavRail);

        map
    }

    pub fn bind(
        &mut self,
        focus: Option<Focus>,
        code: KeyCode,
        modifiers: KeyModifiers,
        action: Action,
    ) {
        self.bindings.push(Binding {
            focus,
            key: Key::new(code, modifiers),
            action,
        });
    }

    pub fn resolve(&self, focus: Focus, key: KeyEvent) -> Option<Action> {
        self.resolve_with(focus, key, true)
    }

    pub fn resolve_for_tab(
        &self,
        focus: Focus,
        active_nav_tab: usize,
        key: KeyEvent,
    ) -> Option<Action> {
        if key.kind == KeyEventKind::Press
            && focus == Focus::List
            && active_nav_tab != 0
            && key.code == KeyCode::Char('u')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Some(Action::OpenLinkExtractor);
        }
        self.resolve_with(focus, key, true)
    }

    pub fn resolve_scoped(&self, focus: Focus, key: KeyEvent) -> Option<Action> {
        self.resolve_with(focus, key, false)
    }

    fn resolve_with(&self, focus: Focus, key: KeyEvent, include_global: bool) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.bindings
            .iter()
            .find(|binding| {
                (binding.focus == Some(focus) || (include_global && binding.focus.is_none()))
                    && key_matches(binding.key, key)
            })
            .map(|binding| binding.action.clone())
    }
}

fn key_matches(expected: Key, actual: KeyEvent) -> bool {
    match expected.code {
        KeyCode::Char(exp_ch) => {
            let KeyCode::Char(act_ch) = actual.code else {
                return false;
            };
            let mod_ok = actual.modifiers.contains(KeyModifiers::CONTROL)
                == expected.modifiers.contains(KeyModifiers::CONTROL)
                && actual.modifiers.contains(KeyModifiers::ALT)
                    == expected.modifiers.contains(KeyModifiers::ALT);
            if !mod_ok {
                return false;
            }
            if expected
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                actual.modifiers.contains(KeyModifiers::SHIFT)
                    && act_ch.eq_ignore_ascii_case(&exp_ch)
            } else {
                act_ch == exp_ch
            }
        }
        _ => expected.code == actual.code && actual.modifiers == expected.modifiers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn outlook_mail_triage_and_tab_shortcuts_map_correctly() {
        let map = Keymap::default_outlook();
        for focus in [Focus::Nav, Focus::List, Focus::Reader] {
            assert_eq!(
                map.resolve(focus, key(KeyCode::Char('1'), KeyModifiers::NONE)),
                Some(Action::SelectNavTab(0))
            );
            assert_eq!(
                map.resolve(focus, key(KeyCode::Char('2'), KeyModifiers::NONE)),
                Some(Action::SelectNavTab(1))
            );
            assert_eq!(
                map.resolve(focus, key(KeyCode::Char('3'), KeyModifiers::NONE)),
                Some(Action::SelectNavTab(2))
            );
        }

        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some(Action::OpenMailCompose)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::MailReply { reply_all: false })
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            Some(Action::MailReply { reply_all: true })
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(Action::MailArchive)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(Action::MailToggleRead)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(Action::MailToggleFlag)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(Action::OpenFolderPicker)
        );
        assert_eq!(
            map.resolve(Focus::Reader, key(KeyCode::Char('z'), KeyModifiers::NONE)),
            Some(Action::ToggleQuotedHistory)
        );
        assert_eq!(
            map.resolve(Focus::Overlay, key(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Action::OverlaySubmit)
        );
        assert_eq!(
            map.resolve(Focus::Overlay, key(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(Action::OverlaySubmit)
        );
        assert_eq!(
            map.resolve(Focus::Reader, key(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(Action::OpenLinkExtractor)
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::List,
                1,
                key(KeyCode::Char('u'), KeyModifiers::NONE)
            ),
            Some(Action::OpenLinkExtractor)
        );
        assert_eq!(
            map.resolve(Focus::Reader, key(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(Action::ToggleShowLinkUrls)
        );
        assert_eq!(
            map.resolve(Focus::Overlay, key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(Action::OpenFilePicker)
        );
    }
}
