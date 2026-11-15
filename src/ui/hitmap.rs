//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

/// Toolbar & header button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Search,
    SendFile,
    Attachments,
    RoomInfo,
    Refresh,
    Settings,
    NewChat,
    Presence,
    CloseThread,
    ComposerTargetToggle,
    SubjectToggle,
    NarrowToggle,
    // Outlook & Image Preview toolbar buttons
    NewMail,
    MailReply,
    MailReplyAll,
    MailForward,
    MailToggleRead,
    MailToggleFlag,
    MailArchive,
    MailDelete,
    ImagePreview,
    NewEvent,
    JoinMeeting,
    RsvpAccept,
    RsvpTentative,
    RsvpDecline,
    DeleteEvent,
    NewContact,
    ContactMail,
    ContactChat,
    DeleteContact,
    NewTodo,
    TodoToggle,
    DeleteTodo,
    TodoInputFocus,
    // Microsoft OneNote toolbar & footer buttons
    OnenoteNewNotebook,
    OnenoteNewSection,
    OnenoteNewPage,
    OnenoteNewSubpage,
    OnenoteAppendBlock,
    OnenoteEditBlock,
    OnenoteEditFullPage,
    OnenoteToggleTodo,
    OnenoteOpenWeb,
    OnenoteRefresh,
    OnenoteDeletePage,
}

/// Identifies which list a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageScope {
    Chat,
    ThreadRoot,
    ThreadReply,
}

/// Clickable targets on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// Navigation tab (`0` = 채팅, `1` = 팀/채널, `2` = 활동, `3` = 메일, `4` = 캘린더, `5` = 연락처, `6` = 할 일, `7` = 노트).
    NavTab(usize),
    /// A row in the active navigation pane list.
    NavRow(usize),
    /// Any rendered line belonging to message `msg_index`.
    Message { scope: MessageScope, msg_index: usize },
    /// Clickable attachment or inline image chip.
    Attachment {
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    },
    /// Clickable reaction badge on a message.
    Reaction {
        scope: MessageScope,
        msg_index: usize,
        reaction_index: usize,
    },
    /// Channel post thread indicator (`[💬 답글 N개 — Enter/t 로 스레드 열기]`).
    OpenThread { msg_index: usize },
    /// Header / toolbar / rail button.
    Button(ButtonId),
    /// Subject field inside composer.
    SubjectInput,
    /// Composer body area.
    Composer,
    /// Item inside the top overlay.
    OverlayItem { index: usize },
    /// Horizontal section tab in the OneNote ribbon bar (`TAB_NOTES`).
    OnenoteSectionTab(usize),
    /// Page item in the OneNote page list sub-column (`TAB_NOTES`).
    OnenotePage(usize),
    /// Block row in the OneNote rich note canvas (`TAB_NOTES`).
    OnenoteBlock(usize),
    /// Interactive `☐` / `☑` checkbox prefix on a OneNote block (`TAB_NOTES`).
    OnenoteTodoToggle(usize),
    /// Inline image or `<object>` file attachment chip in OneNote (`TAB_NOTES`).
    OnenoteResource {
        resource_index: usize,
        is_image: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Registers a clickable rectangle. Zero-sized areas are ignored.
    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    /// Registers a single-row clickable span.
    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    /// Resolves `(row, col)` to the top-most target.
    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let position = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }

    pub fn regions(&self) -> impl Iterator<Item = &(Rect, HitTarget)> {
        self.regions.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_inside_and_outside() {
        let mut map = HitMap::new();
        map.add(Rect::new(2, 3, 5, 2), HitTarget::NavRow(7));

        assert_eq!(map.hit(3, 2), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(4, 6), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(2, 2), None);
        assert_eq!(map.hit(3, 7), None);
    }

    #[test]
    fn later_region_wins_overlap() {
        let mut map = HitMap::new();
        map.add(
            Rect::new(0, 0, 20, 4),
            HitTarget::Message {
                scope: MessageScope::Chat,
                msg_index: 1,
            },
        );
        map.add_line(
            2,
            2,
            8,
            HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 1,
                attach_index: 0,
            },
        );
        map.add_line(
            12,
            2,
            5,
            HitTarget::Reaction {
                scope: MessageScope::Chat,
                msg_index: 1,
                reaction_index: 0,
            },
        );

        assert_eq!(
            map.hit(2, 3),
            Some(HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 1,
                attach_index: 0,
            })
        );
        assert_eq!(
            map.hit(2, 14),
            Some(HitTarget::Reaction {
                scope: MessageScope::Chat,
                msg_index: 1,
                reaction_index: 0,
            })
        );
        assert_eq!(
            map.hit(0, 0),
            Some(HitTarget::Message {
                scope: MessageScope::Chat,
                msg_index: 1,
            })
        );
    }

    #[test]
    fn thread_indicator_and_zero_sized_regions() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 0, 5), HitTarget::Composer);
        map.add(Rect::new(0, 0, 5, 0), HitTarget::Composer);
        assert!(map.is_empty());

        map.add_line(4, 10, 24, HitTarget::OpenThread { msg_index: 3 });
        assert_eq!(map.hit(10, 8), Some(HitTarget::OpenThread { msg_index: 3 }));
        assert_eq!(map.hit(10, 28), None);
    }

    #[test]
    fn onenote_todo_checkbox_wins_over_block_line() {
        let mut map = HitMap::new();
        map.add_line(10, 5, 40, HitTarget::OnenoteBlock(2));
        map.add_line(12, 5, 3, HitTarget::OnenoteTodoToggle(2));

        assert_eq!(map.hit(5, 13), Some(HitTarget::OnenoteTodoToggle(2)));
        assert_eq!(map.hit(5, 20), Some(HitTarget::OnenoteBlock(2)));
    }
}
