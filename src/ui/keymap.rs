//! Data-driven keymap for Microsoft Teams TUI.
//!
//! Focus-specific bindings take precedence over global bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    config::Persona,
    ui::{
        action::Action,
        app::Focus,
        nav_pane::{TAB_CALENDAR, TAB_CONTACTS, TAB_MAIL, TAB_NOTES, TAB_TODO},
        overlays::OverlayKind,
    },
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
    pub fn default_teams() -> Self {
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
        map.bind(Some(Focus::Overlay), Tab, M::NONE, Action::Tab);
        map.bind(Some(Focus::Overlay), BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(Some(Focus::Overlay), Enter, M::NONE, Action::Activate);
        map.bind(Some(Focus::Overlay), Enter, M::CONTROL, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Char('s'), M::CONTROL, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Enter, M::SHIFT, Action::InsertNewline);
        map.bind(Some(Focus::Overlay), Enter, M::ALT, Action::InsertNewline);
        map.bind(Some(Focus::Overlay), Char('j'), M::CONTROL, Action::InsertNewline);
        map.bind(Some(Focus::Overlay), Char('v'), M::CONTROL, Action::PasteClipboard);
        map.bind(Some(Focus::Overlay), Backspace, M::NONE, Action::Backspace);
        map.bind(
            Some(Focus::Overlay),
            Char('y'),
            M::CONTROL,
            Action::CopyFocused,
        );

        // --- Composer ---
        map.bind(Some(Focus::Composer), Enter, M::NONE, Action::SendMessage);
        map.bind(
            Some(Focus::Composer),
            Enter,
            M::SHIFT,
            Action::InsertNewline,
        );
        map.bind(Some(Focus::Composer), Enter, M::ALT, Action::InsertNewline);
        map.bind(
            Some(Focus::Composer),
            Char('j'),
            M::CONTROL,
            Action::InsertNewline,
        );
        map.bind(Some(Focus::Composer), Backspace, M::NONE, Action::Backspace);
        map.bind(Some(Focus::Composer), Delete, M::NONE, Action::DeleteForward);
        map.bind(Some(Focus::Composer), Tab, M::NONE, Action::Tab);
        map.bind(Some(Focus::Composer), Esc, M::NONE, Action::ComposerEscape);
        map.bind(
            Some(Focus::Composer),
            Char('s'),
            M::CONTROL,
            Action::ToggleSubject,
        );

        // Cursor movement
        map.bind(Some(Focus::Composer), Left, M::NONE, Action::CursorLeft);
        map.bind(Some(Focus::Composer), Right, M::NONE, Action::CursorRight);
        map.bind(Some(Focus::Composer), Up, M::NONE, Action::CursorUp);
        map.bind(Some(Focus::Composer), Down, M::NONE, Action::CursorDown);
        map.bind(Some(Focus::Composer), Home, M::NONE, Action::CursorHome);
        map.bind(Some(Focus::Composer), End, M::NONE, Action::CursorEnd);
        map.bind(
            Some(Focus::Composer),
            Home,
            M::CONTROL,
            Action::CursorDocStart,
        );
        map.bind(
            Some(Focus::Composer),
            End,
            M::CONTROL,
            Action::CursorDocEnd,
        );

        // Selection
        map.bind(Some(Focus::Composer), Left, M::SHIFT, Action::SelectLeft);
        map.bind(Some(Focus::Composer), Right, M::SHIFT, Action::SelectRight);
        map.bind(Some(Focus::Composer), Up, M::SHIFT, Action::SelectUp);
        map.bind(Some(Focus::Composer), Down, M::SHIFT, Action::SelectDown);
        map.bind(Some(Focus::Composer), Home, M::SHIFT, Action::SelectHome);
        map.bind(Some(Focus::Composer), End, M::SHIFT, Action::SelectEnd);
        map.bind(
            Some(Focus::Composer),
            Home,
            M::SHIFT | M::CONTROL,
            Action::SelectDocStart,
        );
        map.bind(
            Some(Focus::Composer),
            End,
            M::SHIFT | M::CONTROL,
            Action::SelectDocEnd,
        );

        // Clipboard / undo / external editor
        map.bind(Some(Focus::Composer), Char('a'), M::CONTROL, Action::SelectAll);
        map.bind(Some(Focus::Composer), Char('c'), M::CONTROL, Action::Copy);
        map.bind(Some(Focus::Composer), Char('x'), M::CONTROL, Action::Cut);
        map.bind(
            Some(Focus::Composer),
            Char('v'),
            M::CONTROL,
            Action::PasteClipboard,
        );
        map.bind(Some(Focus::Composer), Char('z'), M::CONTROL, Action::Undo);
        map.bind(Some(Focus::Composer), Char('y'), M::CONTROL, Action::Redo);
        map.bind(
            Some(Focus::Composer),
            Char('e'),
            M::CONTROL,
            Action::OpenExternalEditor,
        );

        // --- Chat & Thread focus message actions ---
        for focus in [Focus::Chat, Focus::Thread] {
            map.bind(Some(focus), Char('y'), M::NONE, Action::CopySelectedMessage);
            map.bind(
                Some(focus),
                Delete,
                M::NONE,
                Action::RequestDeleteSelectedMessage,
            );
            map.bind(
                Some(focus),
                Char('+'),
                M::NONE,
                Action::OpenReactionsForSelected,
            );
            map.bind(
                Some(focus),
                Char('='),
                M::NONE,
                Action::OpenReactionsForSelected,
            );
            map.bind(
                Some(focus),
                Char('e'),
                M::NONE,
                Action::OpenReactionsForSelected,
            );
            map.bind(Some(focus), Char('r'), M::NONE, Action::ReplyToSelected);
        }
        map.bind(
            Some(Focus::Chat),
            Char('t'),
            M::NONE,
            Action::OpenThreadForSelected,
        );

        // --- Global shortcuts ---
        map.bind(any, Char('c'), M::CONTROL, Action::Quit);
        map.bind(any, Char('q'), M::NONE, Action::Quit);
        map.bind(any, Esc, M::NONE, Action::CloseOverlay);
        map.bind(any, Tab, M::NONE, Action::NextFocus);
        map.bind(any, BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(any, Enter, M::NONE, Action::Activate);
        map.bind(any, Char('i'), M::NONE, Action::OpenComposer);
        map.bind(any, Char('k'), M::NONE, Action::MoveUp);
        map.bind(any, Char('j'), M::NONE, Action::MoveDown);
        map.bind(any, Up, M::NONE, Action::MoveUp);
        map.bind(any, Down, M::NONE, Action::MoveDown);
        map.bind(any, Home, M::NONE, Action::NavFirst);
        map.bind(any, End, M::NONE, Action::NavLast);
        map.bind(any, Left, M::NONE, Action::ToggleNarrowPane);
        map.bind(any, Right, M::NONE, Action::ToggleNarrowPane);
        map.bind(any, PageUp, M::NONE, Action::LoadOlder);
        map.bind(any, Char('u'), M::NONE, Action::LoadOlder);
        map.bind(any, Char('s'), M::NONE, Action::SyncHistory);
        map.bind(any, Char('R'), M::NONE, Action::RefreshAll);
        map.bind(any, F(5), M::NONE, Action::RefreshAll);
        map.bind(any, Char('d'), M::NONE, Action::DownloadLatest);
        map.bind(any, Char('m'), M::NONE, Action::ContextMenuCurrent);

        // Nav tabs
        map.bind(any, Char('1'), M::CONTROL, Action::SelectNavTab(0));
        map.bind(any, Char('2'), M::CONTROL, Action::SelectNavTab(1));
        map.bind(any, Char('3'), M::CONTROL, Action::SelectNavTab(2));
        map.bind(any, Char('4'), M::CONTROL, Action::SelectNavTab(3));
        map.bind(any, Char('5'), M::CONTROL, Action::SelectNavTab(4));
        map.bind(any, Char('6'), M::CONTROL, Action::SelectNavTab(5));
        map.bind(any, Char('7'), M::CONTROL, Action::SelectNavTab(6));
        map.bind(any, Char('8'), M::CONTROL, Action::SelectNavTab(7));
        map.bind(any, Char('t'), M::ALT, Action::SwitchPersona(Persona::Teams));
        map.bind(any, Char('o'), M::ALT, Action::SwitchPersona(Persona::Outlook));
        map.bind(any, Char('n'), M::ALT, Action::SwitchPersona(Persona::OneNote));
        map.bind(any, Char('v'), M::NONE, Action::OpenImagePreview);

        // Overlays & Teams actions
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
            Char('p'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::Presence),
        );
        map.bind(any, Char('o'), M::CONTROL, Action::OpenSendFile);
        map.bind(
            any,
            Char('n'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::NewChat),
        );
        map.bind(
            any,
            Char('t'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::Attachments),
        );
        map.bind(
            any,
            Char('u'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::RoomInfo),
        );
        map.bind(
            any,
            Char(','),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::Settings),
        );
        map.bind(any, Char('b'), M::CONTROL, Action::ToggleNavRail);
        map.bind(any, Char('s'), M::CONTROL, Action::ToggleSubject);
        map.bind(any, Up, M::CONTROL, Action::ScrollUp);
        map.bind(any, Down, M::CONTROL, Action::ScrollDown);
        map.bind(any, PageUp, M::SHIFT, Action::ScrollPageUp);
        map.bind(any, PageDown, M::SHIFT, Action::ScrollPageDown);
        map.bind(any, Char('['), M::NONE, Action::AttachmentPrev);
        map.bind(any, Char(']'), M::NONE, Action::AttachmentNext);

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
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if matches!(focus, Focus::Nav | Focus::Chat) {
            if let Some(tab_act) = resolve_outlook_tab_key(active_nav_tab, key) {
                return Some(tab_act);
            }
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

fn resolve_outlook_tab_key(active_nav_tab: usize, key: KeyEvent) -> Option<Action> {
    use KeyCode::*;
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match active_nav_tab {
        TAB_MAIL => match key.code {
            Char('c') => Some(Action::OpenMailComposeNew),
            Char('r') => Some(Action::OpenMailReply { reply_all: false }),
            Char('R') => Some(Action::OpenMailReply { reply_all: true }),
            Char('f') => Some(Action::OpenMailForward),
            Char('u') => Some(Action::MailToggleRead),
            Char('g') => Some(Action::MailToggleFlag),
            Char('a') => Some(Action::MailArchive),
            Char('d') => Some(Action::MailDownloadAttachment),
            Delete => Some(Action::MailDeleteRequest),
            _ => None,
        },
        TAB_CALENDAR => match key.code {
            Char('j') | Char('o') => Some(Action::JoinCalendarMeeting),
            Char('n') => Some(Action::OpenOverlay(OverlayKind::EventCreate)),
            Char('y') => Some(Action::RespondCalendarEvent("accept".to_string())),
            Char('t') => Some(Action::RespondCalendarEvent(
                "tentativelyAccept".to_string(),
            )),
            Char('x') => Some(Action::RespondCalendarEvent("decline".to_string())),
            Delete => Some(Action::DeleteCalendarEventRequest),
            _ => None,
        },
        TAB_CONTACTS => match key.code {
            Char('m') => Some(Action::ContactSendMail),
            Char('c') => Some(Action::ContactStartChat),
            Char('n') => Some(Action::OpenOverlay(OverlayKind::ContactCreate)),
            Delete => Some(Action::DeleteContactRequest),
            _ => None,
        },
        TAB_TODO => match key.code {
            Char(' ') | Char('x') => Some(Action::TodoToggleSelected),
            Char('n') => Some(Action::TodoFocusInput),
            Char('c') => Some(Action::TodoToggleShowCompleted),
            Char('[') => Some(Action::TodoCycleList(-1)),
            Char(']') => Some(Action::TodoCycleList(1)),
            Delete => Some(Action::TodoDeleteRequest),
            _ => None,
        },
        TAB_NOTES => match key.code {
            Char(' ') | Char('x') => Some(Action::OnenoteToggleTodo),
            Char('n') => Some(Action::OnenoteOpenCreatePage { subpage: false }),
            Char('N') => Some(Action::OnenoteOpenCreatePage { subpage: true }),
            Char('S') => Some(Action::OnenoteOpenCreateSection),
            Char('B') => Some(Action::OnenoteOpenCreateNotebook),
            Char('a') => Some(Action::OnenoteOpenAppendBlock),
            Char('e') => Some(Action::OnenoteOpenEditBlock),
            Char('E') => Some(Action::OnenoteOpenEditFullPage),
            Char('[') => Some(Action::OnenoteCycleSection(-1)),
            Char(']') => Some(Action::OnenoteCycleSection(1)),
            Char('J') => Some(Action::OnenoteCyclePage(1)),
            Char('K') => Some(Action::OnenoteCyclePage(-1)),
            Char('o') => Some(Action::OnenoteOpenWeb),
            Char('r') => Some(Action::OnenoteRefresh),
            Char('d') | Delete => Some(Action::OnenoteDeletePageRequest),
            Char('D') => Some(Action::OnenoteDownloadAttachment),
            _ => None,
        },
        _ => None,
    }
}

fn key_matches(expected: Key, actual: KeyEvent) -> bool {
    if expected.code != actual.code {
        return false;
    }
    match expected.code {
        KeyCode::Char(_) => {
            actual.modifiers.contains(KeyModifiers::CONTROL)
                == expected.modifiers.contains(KeyModifiers::CONTROL)
                && actual.modifiers.contains(KeyModifiers::ALT)
                    == expected.modifiers.contains(KeyModifiers::ALT)
        }
        _ => actual.modifiers == expected.modifiers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn composer_enter_sends_and_shift_enter_inserts_newline() {
        let map = Keymap::default_teams();
        assert_eq!(
            map.resolve(Focus::Composer, key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::SendMessage)
        );
        assert_eq!(
            map.resolve(Focus::Composer, key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn teams_shortcuts_map_correctly() {
        let map = Keymap::default_teams();
        assert_eq!(
            map.resolve(Focus::Chat, key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Action::OpenOverlay(OverlayKind::Presence))
        );
        assert_eq!(
            map.resolve(Focus::Chat, key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(Action::OpenOverlay(OverlayKind::NewChat))
        );
        assert_eq!(
            map.resolve(Focus::Chat, key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(Action::OpenSendFile)
        );
        assert_eq!(
            map.resolve(Focus::Chat, key(KeyCode::Char('t'), KeyModifiers::NONE)),
            Some(Action::OpenThreadForSelected)
        );
        assert_eq!(
            map.resolve(Focus::Chat, key(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(Action::OpenReactionsForSelected)
        );
    }

    #[test]
    fn onenote_tab_and_persona_shortcuts_map_correctly() {
        let map = Keymap::default_teams();
        assert_eq!(
            map.resolve(Focus::Nav, key(KeyCode::Char('8'), KeyModifiers::CONTROL)),
            Some(Action::SelectNavTab(7))
        );
        assert_eq!(
            map.resolve(Focus::Nav, key(KeyCode::Char('n'), KeyModifiers::ALT)),
            Some(Action::SwitchPersona(Persona::OneNote))
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::Chat,
                TAB_NOTES,
                key(KeyCode::Char(' '), KeyModifiers::NONE)
            ),
            Some(Action::OnenoteToggleTodo)
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::Chat,
                TAB_NOTES,
                key(KeyCode::Char('n'), KeyModifiers::NONE)
            ),
            Some(Action::OnenoteOpenCreatePage { subpage: false })
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::Chat,
                TAB_NOTES,
                key(KeyCode::Char('N'), KeyModifiers::SHIFT)
            ),
            Some(Action::OnenoteOpenCreatePage { subpage: true })
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::Chat,
                TAB_NOTES,
                key(KeyCode::Char('e'), KeyModifiers::NONE)
            ),
            Some(Action::OnenoteOpenEditBlock)
        );
        assert_eq!(
            map.resolve_for_tab(
                Focus::Chat,
                TAB_NOTES,
                key(KeyCode::Char('E'), KeyModifiers::SHIFT)
            ),
            Some(Action::OnenoteOpenEditFullPage)
        );
    }
}
