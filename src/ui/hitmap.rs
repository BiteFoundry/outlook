//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

use crate::models::EventRsvpAction;

/// Toolbar, header, and modal button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Search,
    CommandPalette,
    DensityToggle,
    Help,
    FilterToggle,
    FocusedToggle,
    ThreadGroupToggle,
    Settings,
    Refresh,
    // Mail toolbar actions
    NewMail,
    MailReply,
    MailReplyAll,
    MailForward,
    MailToggleRead,
    MailToggleFlag,
    MailArchive,
    MailDelete,
    MailMove,
    Attachments,
    ImagePreview,
    HtmlPreview,
    ExpandQuoted,
    // HTML preview modal buttons
    HtmlToggleRaw,
    HtmlOpenBrowser,
    // Calendar toolbar actions
    NewEvent,
    JoinMeeting,
    RsvpAccept,
    RsvpTentative,
    RsvpDecline,
    DeleteEvent,
    // Tasks toolbar actions
    NewTodoList,
    NewTodoTask,
    DeleteTodoTask,
    // Narrow responsive bar pills
    NarrowFolders,
    NarrowList,
    NarrowRead,
    // Compose overlay buttons
    ComposeSend,
    ComposeAttach,
    ComposeSaveDraft,
    ComposeCancel,
    // Confirm dialog
    ConfirmOk,
    ConfirmCancel,
}

/// Clickable targets on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// Primary navigation tab (`0` = Mail, `1` = Calendar, `2` = Tasks).
    NavTab(usize),
    /// A row in the left sidebar (`nav_pane`).
    NavRow(usize),
    /// A day cell inside Tab 2's mini month calendar grid.
    MiniCalDay { year: i32, month: u32, day: u32 },
    /// A row in the middle pane list (Mail message/thread, Calendar event, or To Do task).
    ListItem(usize),
    /// Checkbox on a To Do task row in Tab 3.
    TodoCheckbox(usize),
    /// Collapsible thread message header in Reading Pane when thread-grouped.
    ThreadItem(usize),
    /// Clickable attachment or inline image chip in Reading Pane (`thread_pos` = message index in active thread).
    Attachment {
        thread_pos: usize,
        attach_index: usize,
    },
    /// Toggle quoted reply chain in Reading Pane (`thread_pos` = message index in active thread).
    QuotedToggle(usize),
    /// Meeting invite RSVP button in Mail Reading Pane (`Accept`, `Tentative`, `Decline`).
    MeetingRsvp(EventRsvpAction),
    /// Clickable `[Join Online]` link in Mail meeting banner or Calendar detail.
    JoinMeetingUrl,
    /// Clickable `[Open in Browser]` link in Mail or Calendar detail.
    WebLinkUrl,
    /// Clickable banner to load older mail messages.
    LoadOlderMail,
    /// Header / toolbar / status / modal button.
    Button(ButtonId),
    /// Field inside `MailComposeOverlay` (`0`=To, `1`=Cc, `2`=Bcc, `3`=Subject, `4`=Body).
    ComposeField(usize),
    /// Autocomplete dropdown suggestion row inside `MailComposeOverlay`.
    ComposeContactSuggestion(usize),
    /// Modal / overlay outer frame (prevents clicks inside modal padding from closing the modal).
    OverlayBody,
    /// Clickable HTML document body inside `HtmlPreviewOverlay`.
    HtmlPreviewBody,
    /// Item inside the top overlay.
    OverlayItem { index: usize },
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
        map.add(Rect::new(0, 0, 20, 4), HitTarget::ListItem(1));
        map.add_line(
            2,
            2,
            8,
            HitTarget::Attachment {
                thread_pos: 0,
                attach_index: 0,
            },
        );
        map.add_line(12, 2, 7, HitTarget::QuotedToggle(0));

        assert_eq!(
            map.hit(2, 3),
            Some(HitTarget::Attachment {
                thread_pos: 0,
                attach_index: 0,
            })
        );
        assert_eq!(map.hit(2, 14), Some(HitTarget::QuotedToggle(0)));
        assert_eq!(map.hit(0, 0), Some(HitTarget::ListItem(1)));
    }

    #[test]
    fn zero_sized_regions_ignored_and_rsvp_target_works() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 0, 5), HitTarget::LoadOlderMail);
        map.add(Rect::new(0, 0, 5, 0), HitTarget::LoadOlderMail);
        assert!(map.is_empty());

        map.add_line(4, 10, 12, HitTarget::MeetingRsvp(EventRsvpAction::Accept));
        assert_eq!(
            map.hit(10, 8),
            Some(HitTarget::MeetingRsvp(EventRsvpAction::Accept))
        );
        assert_eq!(map.hit(10, 16), None);
    }
}
