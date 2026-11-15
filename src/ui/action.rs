//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use std::path::PathBuf;

use chrono::Utc;
use ratatui::layout::Position;

use crate::config::Persona;
use crate::models::{
    ConversationTarget, InlineImageUpload, OutgoingContact, OutgoingEvent,
};
use crate::sync::SyncCommand;

use super::app::{AppState, Focus, NarrowPane, UiState};
use super::composer::{self, ReplyPreview};
use super::hitmap::{ButtonId, HitTarget, MessageScope};
use super::nav_pane::{
    self, TAB_CALENDAR, TAB_CONTACTS, TAB_COUNT, TAB_MAIL, TAB_NOTES, TAB_TODO,
};
use super::overlays::context_menu::{ContextMenu, ContextTarget};
use super::overlays::image_preview::ImagePreviewOverlay;
use super::overlays::mail_compose::{MailComposeMode, MailComposeOverlay};
use super::overlays::onenote_form::OnenoteFormOverlay;
use super::overlays::search::{SearchOverlay, SearchScope};
use super::overlays::{Overlay, OverlayKind, OverlayOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Noop,

    // Focus & layout
    Focus(Focus),
    NextFocus,
    PrevFocus,
    ToggleNavRail,
    ToggleAutoThreadPane,
    ToggleNarrowPane,
    ToggleNotify,
    ShowDownloadDir,

    // Navigation pane
    SelectNavTab(usize),
    SelectNavRow(usize),
    NavFirst,
    NavLast,

    // Message selection & scrolling
    SelectMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    MoveUp,
    MoveDown,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    Activate,

    // Composer editing & clipboard
    OpenComposer,
    ToggleSubject,
    FocusSubject,
    SendMessage,
    InsertNewline,
    Backspace,
    DeleteForward,
    Input(char),
    Paste(String),
    ClearComposer,
    ClearPendingImages,
    Tab,
    ComposerEscape,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,
    CursorDocStart,
    CursorDocEnd,
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectHome,
    SelectEnd,
    SelectDocStart,
    SelectDocEnd,
    SelectAll,
    Copy,
    Cut,
    PasteClipboard,
    Undo,
    Redo,
    AcceptMention,
    OpenExternalEditor,

    // Threads & replies
    OpenThreadForSelected,
    OpenThread {
        msg_index: usize,
    },
    CloseThreadDrawer,
    ToggleComposerTarget,
    ReplyToSelected,
    ReplyTo {
        scope: MessageScope,
        msg_index: usize,
    },
    CancelReply,

    // Copy & delete
    CopyMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    CopySelectedMessage,
    CopyFocused,
    CopyText(String),
    RequestDeleteMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    RequestDeleteSelectedMessage,
    DeleteMessage {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    },

    // Reactions
    OpenReactionsForSelected,
    OpenReactions {
        scope: MessageScope,
        msg_index: usize,
    },
    ToggleReaction {
        scope: MessageScope,
        msg_index: usize,
        reaction_type: String,
    },
    ToggleReactionByIndex {
        scope: MessageScope,
        msg_index: usize,
        reaction_index: usize,
    },

    // Attachments & files
    DownloadAttachment {
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    },
    DownloadLatest,
    OpenSendFile,
    SendAttachmentPath {
        path: String,
        caption: String,
    },
    AttachmentNext,
    AttachmentPrev,

    // Search, sync & history
    Search(String),
    OpenSearch,
    JumpToSearchHit(usize),
    RefreshAll,
    SyncHistory,
    LoadOlder,
    MarkActiveRead,

    // People, chats & presence
    SearchUsers(String),
    CreateChat {
        peers: Vec<String>,
        topic: Option<String>,
    },
    SetPresence {
        availability: String,
        activity: String,
    },

    // Overlays & mouse
    OpenOverlay(OverlayKind),
    CloseOverlay,
    OpenContextMenuAt {
        column: u16,
        row: u16,
    },
    ContextMenuCurrent,
    OpenConfirm {
        message: String,
        on_confirm: Option<Box<Action>>,
    },
    OverlaySelect(usize),
    OverlaySubmit,
    Click {
        column: u16,
        row: u16,
    },
    ShowToast(String),

    // Persona, Inline Images & Updater
    SwitchPersona(Persona),
    ToggleInlineImages,
    OpenImagePreview,
    LoadPreviewImage,
    DownloadPreviewImage,
    CheckForUpdates,
    ApplySelfUpdate,

    // Outlook Mail
    OpenMailComposeNew,
    OpenMailReply {
        reply_all: bool,
    },
    OpenMailForward,
    SubmitMailCompose {
        mode: MailComposeMode,
        to: Vec<String>,
        cc: Vec<String>,
        subject: String,
        body_markdown: String,
        attachments: Vec<PathBuf>,
    },
    MailToggleRead,
    MailToggleFlag,
    MailArchive,
    MailDeleteRequest,
    MailDeleteConfirm,
    MailDownloadAttachment,

    // Outlook Calendar
    CreateCalendarEvent(OutgoingEvent),
    JoinCalendarMeeting,
    RespondCalendarEvent(String),
    DeleteCalendarEventRequest,
    DeleteCalendarEventConfirm,

    // Outlook Contacts
    CreateContact(OutgoingContact),
    DeleteContactRequest,
    DeleteContactConfirm,
    ContactSendMail,
    ContactStartChat,

    // Microsoft To Do
    TodoToggleSelected,
    TodoCycleList(isize),
    TodoToggleShowCompleted,
    TodoFocusInput,
    TodoDeleteRequest,
    TodoDeleteConfirm,

    // Scoped Search
    SearchMail(String),
    SearchContacts(String),
    JumpToMailSearchHit(usize),
    JumpToContactSearchHit(usize),

    // Microsoft OneNote
    SearchNotes(String),
    JumpToNoteSearchHit(usize),
    OnenoteOpenCreateNotebook,
    OnenoteOpenCreateSection,
    OnenoteOpenCreatePage { subpage: bool },
    OnenoteCreateNotebook {
        display_name: String,
    },
    OnenoteCreateSection {
        notebook_key: String,
        display_name: String,
    },
    OnenoteCreatePage {
        section_key: String,
        title: String,
        markdown_body: String,
        inline_images: Vec<InlineImageUpload>,
        level: Option<i32>,
    },
    OnenoteAppendMarkdown {
        page_key: String,
        markdown: String,
        inline_images: Vec<InlineImageUpload>,
    },
    OnenoteEditBlockSubmit {
        page_key: String,
        element_id: String,
        markdown: String,
    },
    OnenoteDeletePageConfirm,
    OnenoteDownloadAttachment,
}

/// Dispatches an `Action`, mutating `app` and `ui`.
/// Returns `true` when the application should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if matches!(action, Action::Quit) {
        app.stash_draft(ui);
        return true;
    }

    if let Action::Click { column, row } = action {
        return handle_click(column, row, app, ui);
    }

    if let Action::OpenContextMenuAt { column, row } = action {
        open_context_menu_at(column, row, app, ui);
        return false;
    }

    // If an overlay is open, let it handle actions first.
    if let Some(mut top) = ui.overlays.pop() {
        let outcome = top.handle(&action, app, ui);
        match outcome {
            OverlayOutcome::Handled => {
                ui.overlays.push(top);
                return false;
            }
            OverlayOutcome::Close => {
                return false;
            }
            OverlayOutcome::Dispatch(next) => {
                ui.overlays.push(top);
                return dispatch(next, app, ui);
            }
            OverlayOutcome::CloseAndDispatch(next) => {
                return dispatch(next, app, ui);
            }
            OverlayOutcome::NotHandled => {
                ui.overlays.push(top);
            }
        }
    }

    // Quick To Do input bar interception when focused on TAB_TODO center pane.
    if matches!(ui.focus(), Focus::Chat)
        && ui.active_nav_tab == TAB_TODO
        && ui.todo_input_active
    {
        match action {
            Action::Input(ch) => {
                ui.todo_input.push(ch);
                return false;
            }
            Action::Paste(text) => {
                ui.todo_input.push_str(&text.replace(['\r', '\n'], " "));
                return false;
            }
            Action::Backspace => {
                ui.todo_input.pop();
                return false;
            }
            Action::SendMessage | Action::Activate => {
                let title = std::mem::take(&mut ui.todo_input);
                ui.todo_input_active = false;
                app.create_todo_task(ui, title);
                return false;
            }
            Action::CloseOverlay | Action::ComposerEscape => {
                ui.todo_input_active = false;
                return false;
            }
            _ => {}
        }
    }

    // Mention popup interception while typing in the composer.
    if matches!(ui.focus(), Focus::Composer) && ui.composer.mention_active() {
        match action {
            Action::MoveUp | Action::CursorUp => {
                ui.composer.mention_prev();
                return false;
            }
            Action::MoveDown | Action::CursorDown => {
                ui.composer.mention_next();
                return false;
            }
            Action::AcceptMention | Action::Tab | Action::Activate => {
                ui.composer.accept_mention();
                return false;
            }
            Action::ComposerEscape | Action::CloseOverlay => {
                ui.composer.close_mentions();
                return false;
            }
            _ => {}
        }
    }

    match action {
        Action::Quit => unreachable!(),
        Action::Noop => {}

        // Focus & layout
        Action::Focus(focus) => {
            ui.set_focus(focus);
        }
        Action::NextFocus => cycle_focus(app, ui, true),
        Action::PrevFocus => cycle_focus(app, ui, false),
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
            app.config.show_nav_rail = ui.show_nav_rail;
            let _ = app.config.save();
        }
        Action::ToggleAutoThreadPane => {
            ui.auto_thread_pane = !ui.auto_thread_pane;
            app.config.auto_thread_pane = ui.auto_thread_pane;
            let _ = app.config.save();
            ui.info(if ui.auto_thread_pane {
                "넓은 화면 자동 스레드 패널 켜짐"
            } else {
                "넓은 화면 자동 스레드 패널 꺼짐"
            });
        }
        Action::ToggleNarrowPane => {
            ui.narrow_pane = match ui.narrow_pane {
                NarrowPane::List => NarrowPane::Chat,
                NarrowPane::Chat => {
                    if ui.thread_drawer_open && app.active_thread.is_some() {
                        NarrowPane::Thread
                    } else {
                        NarrowPane::List
                    }
                }
                NarrowPane::Thread => NarrowPane::Chat,
            };
            match ui.narrow_pane {
                NarrowPane::List => ui.set_focus(Focus::Nav),
                NarrowPane::Chat => ui.set_focus(Focus::Chat),
                NarrowPane::Thread => ui.set_focus(Focus::Thread),
            }
        }
        Action::ToggleNotify => {
            ui.notify = !ui.notify;
            app.config.notify = ui.notify;
            let _ = app.config.save();
            ui.info(if ui.notify {
                "데스크톱 알림 켜짐"
            } else {
                "데스크톱 알림 꺼짐"
            });
        }
        Action::ShowDownloadDir => {
            ui.info(format!("다운로드 폴더: {}", app.download_dir.display()));
        }

        // Navigation pane
        Action::SelectNavTab(tab) => {
            ui.active_nav_tab = tab.min(TAB_COUNT - 1);
            ui.nav_selected = 0;
            ui.todo_input_active = false;
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::Nav);
            if ui.active_nav_tab >= TAB_MAIL {
                ui.persona = Persona::Outlook;
            } else {
                ui.persona = Persona::Teams;
            }
        }
        Action::SelectNavRow(idx) => {
            app.select_nav_row(ui, idx);
        }
        Action::NavFirst => {
            let len = nav_pane::build_nav_rows(app, ui).len();
            if len > 0 {
                ui.nav_selected = 0;
            }
        }
        Action::NavLast => {
            let len = nav_pane::build_nav_rows(app, ui).len();
            if len > 0 {
                ui.nav_selected = len - 1;
            }
        }

        // Message selection & scrolling
        Action::SelectMessage { scope, msg_index } => match scope {
            MessageScope::Chat => {
                if msg_index < app.messages.len() {
                    ui.chat_scroll.selected_idx = Some(msg_index);
                    ui.set_focus(Focus::Chat);
                }
            }
            MessageScope::ThreadRoot | MessageScope::ThreadReply => {
                ui.thread_scroll.selected_idx = Some(match scope {
                    MessageScope::ThreadRoot => 0,
                    MessageScope::ThreadReply => msg_index + 1,
                    MessageScope::Chat => unreachable!(),
                });
                ui.set_focus(Focus::Thread);
            }
        },
        Action::MoveUp => move_selection(app, ui, -1),
        Action::MoveDown => move_selection(app, ui, 1),
        Action::ScrollUp => scroll_focused(ui, -3),
        Action::ScrollDown => scroll_focused(ui, 3),
        Action::ScrollPageUp => scroll_focused(ui, -12),
        Action::ScrollPageDown => scroll_focused(ui, 12),
        Action::ScrollTop => match ui.focus() {
            Focus::Nav => {
                let _ = dispatch(Action::NavFirst, app, ui);
            }
            Focus::Thread => ui.thread_scroll.top(),
            _ => ui.chat_scroll.top(),
        },
        Action::ScrollBottom => match ui.focus() {
            Focus::Nav => {
                let _ = dispatch(Action::NavLast, app, ui);
            }
            Focus::Thread => ui.thread_scroll.bottom(),
            _ => ui.chat_scroll.bottom(),
        },
        Action::Activate => handle_activate(app, ui),

        // Composer
        Action::OpenComposer => {
            ui.set_focus(Focus::Composer);
        }
        Action::ToggleSubject => {
            let is_channel = app
                .active_target
                .as_ref()
                .is_some_and(ConversationTarget::is_channel);
            if !is_channel || ui.composer_target_thread {
                ui.info("제목(Subject)은 채널 루트 게시물에서만 사용할 수 있습니다");
            } else {
                ui.show_subject = !ui.show_subject;
                ui.composer.subject_focused = ui.show_subject;
                if ui.show_subject {
                    ui.set_focus(Focus::Composer);
                }
            }
        }
        Action::FocusSubject => {
            ui.show_subject = true;
            ui.composer.subject_focused = !ui.composer.subject_focused;
            ui.set_focus(Focus::Composer);
        }
        Action::SendMessage => {
            app.send_composer(ui);
        }
        Action::InsertNewline => {
            if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.composer.insert_char('\n');
                ui.composer.reference_mentions(&app.members);
            }
        }
        Action::Backspace => {
            ui.composer.backspace();
            ui.composer.reference_mentions(&app.members);
        }
        Action::DeleteForward => {
            ui.composer.delete_forward();
            ui.composer.reference_mentions(&app.members);
        }
        Action::Input(ch) => {
            if !matches!(ui.focus(), Focus::Composer) {
                ui.set_focus(Focus::Composer);
            }
            ui.composer.insert_char(ch);
            ui.composer.reference_mentions(&app.members);
        }
        Action::Paste(text) => {
            if !matches!(ui.focus(), Focus::Composer) {
                ui.set_focus(Focus::Composer);
            }
            ui.composer.insert_str(&text);
            ui.composer.reference_mentions(&app.members);
        }
        Action::ClearComposer => {
            app.clear_current_draft(ui);
        }
        Action::ClearPendingImages => {
            ui.composer.clear_pending_images();
            ui.info("첨부된 인라인 이미지를 모두 비웠습니다");
        }
        Action::Tab => {
            if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.composer.insert_str("    ");
            }
        }
        Action::ComposerEscape => {
            if ui.composer.mention_active() {
                ui.composer.close_mentions();
            } else if ui.composer.reply().is_some() {
                ui.composer.clear_reply();
            } else if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.toggle_composer_focus(false);
            }
        }
        Action::CursorLeft => ui.composer.move_left(false),
        Action::CursorRight => ui.composer.move_right(false),
        Action::CursorUp => ui.composer.move_up(false),
        Action::CursorDown => ui.composer.move_down(false),
        Action::CursorHome => ui.composer.move_home(false),
        Action::CursorEnd => ui.composer.move_end(false),
        Action::CursorDocStart => ui.composer.move_doc_start(false),
        Action::CursorDocEnd => ui.composer.move_doc_end(false),
        Action::SelectLeft => ui.composer.move_left(true),
        Action::SelectRight => ui.composer.move_right(true),
        Action::SelectUp => ui.composer.move_up(true),
        Action::SelectDown => ui.composer.move_down(true),
        Action::SelectHome => ui.composer.move_home(true),
        Action::SelectEnd => ui.composer.move_end(true),
        Action::SelectDocStart => ui.composer.move_doc_start(true),
        Action::SelectDocEnd => ui.composer.move_doc_end(true),
        Action::SelectAll => ui.composer.select_all(),
        Action::Copy => {
            if matches!(ui.focus(), Focus::Composer) {
                let text = ui
                    .composer
                    .selected_text()
                    .unwrap_or_else(|| ui.composer.text())
                    .to_string();
                copy_to_clipboard(&text, ui);
            } else {
                let _ = dispatch(Action::CopySelectedMessage, app, ui);
            }
        }
        Action::Cut => {
            if let Some(sel) = ui.composer.selected_text().map(str::to_string) {
                ui.composer.delete_selection();
                copy_to_clipboard(&sel, ui);
            }
        }
        Action::PasteClipboard => {
            paste_clipboard(app, ui);
        }
        Action::Undo => {
            ui.composer.undo();
        }
        Action::Redo => {
            ui.composer.redo();
        }
        Action::AcceptMention => {
            ui.composer.accept_mention();
        }
        Action::OpenExternalEditor => {
            ui.set_focus(Focus::Composer);
            ui.editor_requested = true;
        }

        // Threads & replies
        Action::OpenThreadForSelected => {
            if let Some(idx) = selected_chat_index(app, ui) {
                app.open_thread(ui, idx);
            }
        }
        Action::OpenThread { msg_index } => {
            app.open_thread(ui, msg_index);
        }
        Action::CloseThreadDrawer => {
            app.close_thread(ui);
        }
        Action::ToggleComposerTarget => {
            if app.active_thread.is_some() {
                app.stash_draft(ui);
                ui.composer_target_thread = !ui.composer_target_thread;
                app.restore_draft(ui);
            } else {
                ui.composer_target_thread = false;
                ui.info("열린 스레드가 없어 채널/채팅 본문으로 입력합니다");
            }
        }
        Action::ReplyToSelected => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(Action::ReplyTo { scope, msg_index }, app, ui);
            }
        }
        Action::ReplyTo { scope, msg_index } => {
            if matches!(scope, MessageScope::Chat)
                && app
                    .active_target
                    .as_ref()
                    .is_some_and(ConversationTarget::is_channel)
            {
                app.open_thread(ui, msg_index);
                ui.set_focus(Focus::Composer);
            } else if let Some(msg) = app.message_at(scope, msg_index) {
                ui.composer.set_reply(ReplyPreview {
                    msg_id: msg.msg_id.clone(),
                    author: msg.author_name.clone(),
                    text: msg.body_plain.chars().take(80).collect(),
                });
                ui.set_focus(Focus::Composer);
            }
        }
        Action::CancelReply => {
            ui.composer.clear_reply();
        }

        // Copy & delete
        Action::CopyMessage { scope, msg_index } => {
            if let Some(msg) = app.message_at(scope, msg_index) {
                let text = if let Some(sub) = &msg.subject {
                    format!("{sub}\n{}", msg.body_plain)
                } else {
                    msg.body_plain.clone()
                };
                copy_to_clipboard(&text, ui);
            }
        }
        Action::CopySelectedMessage | Action::CopyFocused => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(Action::CopyMessage { scope, msg_index }, app, ui);
            }
        }
        Action::CopyText(text) => {
            copy_to_clipboard(&text, ui);
        }
        Action::RequestDeleteMessage { scope, msg_index } => {
            if let (Some(conv_key), Some(msg)) =
                (app.active_conv_key.clone(), app.message_at(scope, msg_index))
            {
                if !msg.is_me {
                    ui.info("본인이 작성한 메시지만 삭제할 수 있습니다");
                    return false;
                }
                let parent_msg_id = match scope {
                    MessageScope::ThreadReply => {
                        app.active_thread.as_ref().map(|t| t.root.msg_id.clone())
                    }
                    _ => msg.parent_msg_id.clone(),
                };
                let preview: String = msg.body_plain.chars().take(40).collect();
                let confirm = Action::DeleteMessage {
                    conv_key,
                    msg_id: msg.msg_id.clone(),
                    parent_msg_id,
                };
                ui.push_overlay(Overlay::confirm(
                    format!("이 메시지를 삭제할까요?\n\"{preview}\""),
                    Some(confirm),
                ));
            }
        }
        Action::RequestDeleteSelectedMessage => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(
                    Action::RequestDeleteMessage { scope, msg_index },
                    app,
                    ui,
                );
            }
        }
        Action::DeleteMessage {
            conv_key,
            msg_id,
            parent_msg_id,
        } => {
            app.delete_message(ui, conv_key, msg_id, parent_msg_id);
        }

        // Reactions
        Action::OpenReactionsForSelected => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                ui.push_overlay(Overlay::reactions(scope, msg_index));
            }
        }
        Action::OpenReactions { scope, msg_index } => {
            ui.push_overlay(Overlay::reactions(scope, msg_index));
        }
        Action::ToggleReaction {
            scope,
            msg_index,
            reaction_type,
        } => {
            app.toggle_reaction(ui, scope, msg_index, reaction_type);
        }
        Action::ToggleReactionByIndex {
            scope,
            msg_index,
            reaction_index,
        } => {
            let reaction_type = app
                .message_at(scope, msg_index)
                .and_then(|m| m.reactions.get(reaction_index))
                .map(|r| r.reaction_type.clone());
            if let Some(rt) = reaction_type {
                app.toggle_reaction(ui, scope, msg_index, rt);
            }
        }

        // Attachments
        Action::DownloadAttachment {
            scope,
            msg_index,
            attach_index,
        } => {
            app.download_attachment(ui, scope, msg_index, attach_index);
        }
        Action::DownloadLatest => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                if app
                    .message_at(scope, msg_index)
                    .is_some_and(|m| !m.attachments.is_empty())
                {
                    app.download_attachment(ui, scope, msg_index, 0);
                    return false;
                }
            }
            if let Some((idx, _)) = app
                .messages
                .iter()
                .enumerate()
                .rev()
                .find(|(_, m)| !m.attachments.is_empty())
            {
                app.download_attachment(ui, MessageScope::Chat, idx, 0);
            } else {
                ui.info("다운로드할 첨부파일이 없습니다");
            }
        }
        Action::OpenSendFile => {
            open_overlay(OverlayKind::PathInput, app, ui);
        }
        Action::SendAttachmentPath { path, caption } => {
            app.send_file(ui, path, caption);
        }
        Action::AttachmentNext => {
            ui.attach_index = ui.attach_index.saturating_add(1);
        }
        Action::AttachmentPrev => {
            ui.attach_index = ui.attach_index.saturating_sub(1);
        }

        // Search, sync & history
        Action::Search(query) => {
            app.search_messages(ui, query);
        }
        Action::OpenSearch => {
            open_overlay(OverlayKind::Search, app, ui);
        }
        Action::JumpToSearchHit(hit_idx) => {
            if let Some(hit) = app.search_results.get(hit_idx).cloned() {
                if let Some(target) = ConversationTarget::from_conv_key(&hit.conv_key) {
                    app.select_conversation(ui, target);
                    if let Some(root) = hit.parent_msg_id {
                        app.open_thread_by_id(ui, root);
                    } else if let Some(idx) =
                        app.messages.iter().position(|m| m.msg_id == hit.msg_id)
                    {
                        ui.chat_scroll.selected_idx = Some(idx);
                    }
                }
            }
        }
        Action::RefreshAll => {
            app.sync_ctrl.request(SyncCommand::RefreshConversations);
            app.sync_ctrl.request(SyncCommand::RefreshOutlook);
            app.sync_ctrl.request(SyncCommand::SyncActiveNow);
            ui.info("Teams · Outlook 목록과 메시지를 새로고침합니다...");
        }
        Action::SyncHistory => {
            if let Some(ConversationTarget::Channel {
                team_id,
                channel_id,
            }) = app.active_target.clone()
            {
                app.sync_ctrl.request(SyncCommand::SyncChannelHistory {
                    team_id,
                    channel_id,
                });
                ui.info("전체 히스토리 동기화를 시작합니다...");
            } else {
                app.load_older(ui);
            }
        }
        Action::LoadOlder => {
            app.load_older(ui);
        }
        Action::MarkActiveRead => {
            if let Some(conv_key) = app.active_conv_key.clone() {
                if let Some(conv) = app.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
                    conv.unread_count = 0;
                }
                let now_ts = Utc::now().to_rfc3339();
                let _ = app.storage.mark_conversation_read(&conv_key, &now_ts);
                if let Some(ConversationTarget::Chat { chat_id }) = app.active_target.clone() {
                    let graph = app.graph.clone();
                    tokio::spawn(async move {
                        let _ = graph.mark_chat_read(&chat_id).await;
                    });
                }
                ui.info("읽음으로 표시했습니다");
            }
        }

        // People & presence
        Action::SearchUsers(query) => {
            app.search_users(ui, query);
        }
        Action::CreateChat { peers, topic } => {
            app.create_chat(ui, peers, topic);
        }
        Action::SetPresence {
            availability,
            activity,
        } => {
            app.set_presence(ui, availability, activity);
        }

        // Overlays
        Action::OpenOverlay(kind) => {
            open_overlay(kind, app, ui);
        }
        Action::CloseOverlay => {
            if ui.close_top_overlay().is_none()
                && ui.thread_drawer_open
                && matches!(ui.focus(), Focus::Thread)
            {
                app.close_thread(ui);
            }
        }
        Action::ContextMenuCurrent => {
            let ctx = match ui.focus() {
                Focus::Nav => ContextTarget::NavRow(ui.nav_selected),
                Focus::Chat => {
                    if let Some(idx) = selected_chat_index(app, ui) {
                        ContextTarget::Message {
                            scope: MessageScope::Chat,
                            msg_index: idx,
                        }
                    } else {
                        ContextTarget::Global
                    }
                }
                Focus::Thread => {
                    if let Some((scope, idx)) = selected_thread_scope_and_index(app, ui) {
                        ContextTarget::Message {
                            scope,
                            msg_index: idx,
                        }
                    } else {
                        ContextTarget::Global
                    }
                }
                Focus::Composer => ContextTarget::Composer,
                Focus::Overlay => ContextTarget::Global,
            };
            let menu = ContextMenu::build(ctx, (12, 6), app, ui);
            ui.push_overlay(Overlay::context_menu(menu));
        }
        Action::OpenConfirm {
            message,
            on_confirm,
        } => {
            ui.push_overlay(Overlay::confirm(message, on_confirm.map(|b| *b)));
        }
        Action::OverlaySelect(index) => {
            if let Some(top) = ui.overlays.last_mut() {
                top.select(index);
            }
        }
        Action::OverlaySubmit => {}
        Action::Click { .. } | Action::OpenContextMenuAt { .. } => unreachable!(),
        Action::ShowToast(msg) => {
            ui.info(msg);
        }

        // Persona, Inline Images & Updater
        Action::SwitchPersona(persona) => {
            ui.persona = persona;
            ui.todo_input_active = false;
            match persona {
                Persona::Teams => {
                    if ui.active_nav_tab >= TAB_MAIL {
                        ui.active_nav_tab = 0;
                        ui.nav_selected = 0;
                    }
                    ui.info("Teams 워크스페이스 모드로 전환했습니다");
                }
                Persona::Outlook => {
                    if ui.active_nav_tab < TAB_MAIL {
                        ui.active_nav_tab = TAB_MAIL;
                        ui.nav_selected = 0;
                    }
                    ui.info("Outlook 워크스페이스 모드로 전환했습니다");
                }
                Persona::OneNote => {
                    if ui.active_nav_tab != TAB_NOTES {
                        ui.active_nav_tab = TAB_NOTES;
                        ui.nav_selected = 0;
                    }
                    ui.info("OneNote 워크스페이스 모드로 전환했습니다");
                }
            }
            ui.set_focus(Focus::Nav);
        }
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            app.config.inline_images = ui.inline_images;
            let _ = app.config.save();
            ui.info(if ui.inline_images {
                "인라인 이미지 미리보기 켜짐"
            } else {
                "인라인 이미지 미리보기 꺼짐"
            });
        }
        Action::OpenImagePreview => {
            open_image_preview(app, ui);
        }
        Action::LoadPreviewImage => {
            let dims = (ui.term_width, ui.term_height);
            let target_and_dims = ui.overlays.last().and_then(|top| {
                if let Overlay::ImagePreview(preview) = top {
                    preview
                        .current()
                        .cloned()
                        .map(|t| (t, preview.target_dimensions(dims.0, dims.1)))
                } else {
                    None
                }
            });
            if let Some((target, (cols, rows))) = target_and_dims {
                app.request_preview_image(&target, cols, rows);
            }
        }
        Action::DownloadPreviewImage => {
            let current = ui.overlays.last().and_then(|top| {
                if let Overlay::ImagePreview(preview) = top {
                    preview.current().cloned()
                } else {
                    None
                }
            });
            if let Some(target) = current {
                match target {
                    super::overlays::image_preview::PreviewTarget::Teams { attachment, .. } => {
                        let graph = app.graph.clone();
                        let dir = app.download_dir.clone();
                        let tx = app.ui_tx.clone();
                        let label = format!("이미지 저장: {}", attachment.name.as_deref().unwrap_or(&attachment.id));
                        ui.tasks.begin(label.clone());
                        tokio::spawn(async move {
                            match graph.download_attachment(&attachment, &dir).await {
                                Ok(file) => {
                                    let _ = tx.send(super::app::UiMsg::TaskDone {
                                        label,
                                        error: None,
                                        toast: Some((
                                            format!("저장됨: {}", file.path.display()),
                                            super::toast::ToastKind::Success,
                                        )),
                                    });
                                }
                                Err(err) => {
                                    let _ = tx.send(super::app::UiMsg::TaskDone {
                                        label,
                                        error: Some(err.to_string()),
                                        toast: Some((format!("저장 실패: {err}"), super::toast::ToastKind::Error)),
                                    });
                                }
                            }
                        });
                    }
                    super::overlays::image_preview::PreviewTarget::Mail { mail_id, attachment } => {
                        app.download_mail_attachment(ui, mail_id, attachment.id, attachment.name);
                    }
                    super::overlays::image_preview::PreviewTarget::OneNote {
                        page_key,
                        resource_index,
                        resource,
                        ..
                    } => {
                        app.download_onenote_attachment(
                            ui,
                            &page_key,
                            resource_index,
                            resource.clone(),
                        );
                    }
                }
            }
        }
        Action::CheckForUpdates => {
            app.check_for_updates(ui, true, false);
        }
        Action::ApplySelfUpdate => {
            app.check_for_updates(ui, false, false);
        }

        // Outlook Mail
        Action::OpenMailComposeNew => {
            ui.push_overlay(Overlay::MailCompose(MailComposeOverlay::new()));
        }
        Action::OpenMailReply { reply_all } => {
            if let Some(mail) = app.active_mail() {
                ui.push_overlay(Overlay::MailCompose(MailComposeOverlay::reply(
                    mail, reply_all,
                )));
            } else {
                ui.info("답장할 메일을 먼저 선택하세요");
            }
        }
        Action::OpenMailForward => {
            if let Some(mail) = app.active_mail() {
                ui.push_overlay(Overlay::MailCompose(MailComposeOverlay::forward(mail)));
            } else {
                ui.info("전달할 메일을 먼저 선택하세요");
            }
        }
        Action::SubmitMailCompose {
            mode,
            to,
            cc,
            subject,
            body_markdown,
            attachments,
        } => {
            app.submit_mail_compose(ui, mode, to, cc, subject, body_markdown, attachments);
        }
        Action::MailToggleRead => {
            app.toggle_active_mail_read(ui);
        }
        Action::MailToggleFlag => {
            app.toggle_active_mail_flag(ui);
        }
        Action::MailArchive => {
            app.archive_active_mail(ui);
        }
        Action::MailDeleteRequest => {
            if let Some(mail) = app.active_mail() {
                let subj = if mail.subject.is_empty() {
                    "(제목 없음)"
                } else {
                    mail.subject.as_str()
                };
                let msg = format!("메일 '{subj}' 을(를) 삭제하시겠습니까?");
                ui.push_overlay(Overlay::confirm(msg, Some(Action::MailDeleteConfirm)));
            }
        }
        Action::MailDeleteConfirm => {
            app.delete_active_mail(ui);
        }
        Action::MailDownloadAttachment => {
            if let Some(mail) = app.active_mail().cloned() {
                if let Some(att) = mail
                    .attachments
                    .get(ui.mail_attach_index)
                    .or_else(|| mail.attachments.first())
                {
                    app.download_mail_attachment(ui, mail.id, att.id.clone(), att.name.clone());
                } else {
                    ui.info("다운로드할 메일 첨부파일이 없습니다");
                }
            }
        }

        // Outlook Calendar
        Action::CreateCalendarEvent(event) => {
            app.create_calendar_event(ui, event);
        }
        Action::JoinCalendarMeeting => {
            app.open_active_event_meeting(ui);
        }
        Action::RespondCalendarEvent(response) => {
            app.respond_active_event(ui, &response);
        }
        Action::DeleteCalendarEventRequest => {
            if let Some(ev) = app.active_event() {
                let msg = format!("일정 '{}' 을(를) 삭제하시겠습니까?", ev.subject);
                ui.push_overlay(Overlay::confirm(
                    msg,
                    Some(Action::DeleteCalendarEventConfirm),
                ));
            }
        }
        Action::DeleteCalendarEventConfirm => {
            app.delete_active_event(ui);
        }

        // Outlook Contacts
        Action::CreateContact(contact) => {
            app.create_contact(ui, contact);
        }
        Action::DeleteContactRequest => {
            if let Some(contact) = app.active_contact() {
                let msg = format!("연락처 '{}' 을(를) 삭제하시겠습니까?", contact.display_name);
                ui.push_overlay(Overlay::confirm(msg, Some(Action::DeleteContactConfirm)));
            }
        }
        Action::DeleteContactConfirm => {
            app.delete_active_contact(ui);
        }
        Action::ContactSendMail => {
            if let Some(contact) = app.active_contact() {
                let compose = if let Some(email) = contact.primary_email() {
                    MailComposeOverlay::new_to(email)
                } else {
                    MailComposeOverlay::new()
                };
                ui.push_overlay(Overlay::MailCompose(compose));
            } else {
                ui.info("연락처를 먼저 선택하세요");
            }
        }
        Action::ContactStartChat => {
            app.start_chat_with_contact(ui);
        }

        // Microsoft To Do
        Action::TodoToggleSelected => {
            app.toggle_active_todo_completed(ui);
        }
        Action::TodoCycleList(delta) => {
            app.cycle_todo_list(ui, delta);
        }
        Action::TodoToggleShowCompleted => {
            app.toggle_todo_show_completed(ui);
        }
        Action::TodoFocusInput => {
            ui.active_nav_tab = TAB_TODO;
            ui.narrow_pane = NarrowPane::Chat;
            ui.set_focus(Focus::Chat);
            ui.todo_input_active = true;
        }
        Action::TodoDeleteRequest => {
            if let Some(task) = app.active_todo_task() {
                let msg = format!("할 일 '{}' 을(를) 삭제하시겠습니까?", task.title);
                ui.push_overlay(Overlay::confirm(msg, Some(Action::TodoDeleteConfirm)));
            }
        }
        Action::TodoDeleteConfirm => {
            app.delete_active_todo(ui);
        }

        // Scoped Search
        Action::SearchMail(query) => {
            app.search_mail(query);
        }
        Action::SearchContacts(query) => {
            app.search_contacts(query);
        }
        Action::JumpToMailSearchHit(idx) => {
            if let Some(hit) = app.mail_search_results.get(idx).cloned() {
                ui.active_nav_tab = TAB_MAIL;
                ui.persona = Persona::Outlook;
                app.select_mail(ui, hit.id);
                ui.narrow_pane = NarrowPane::Chat;
                ui.set_focus(Focus::Chat);
            }
        }
        Action::JumpToContactSearchHit(idx) => {
            if let Some(contact) = app.contact_search_results.get(idx).cloned() {
                ui.active_nav_tab = TAB_CONTACTS;
                ui.persona = Persona::Outlook;
                if !app.contacts.iter().any(|c| c.id == contact.id) {
                    app.contacts.insert(0, contact.clone());
                }
                app.select_contact(ui, contact.id);
                ui.narrow_pane = NarrowPane::Chat;
                ui.set_focus(Focus::Chat);
            }
        }

        // Microsoft OneNote
        Action::SearchNotes(query) => {
            app.search_notes(query);
        }
        Action::JumpToNoteSearchHit(idx) => {
            app.jump_to_note_search_hit(ui, idx);
        }
        Action::OnenoteOpenCreateNotebook => {
            app.open_onenote_create_notebook(ui);
        }
        Action::OnenoteOpenCreateSection => {
            app.open_onenote_create_section(ui);
        }
        Action::OnenoteOpenCreatePage { subpage } => {
            app.open_onenote_create_page(ui, subpage);
        }
        Action::OnenoteCreateNotebook { display_name } => {
            app.create_onenote_notebook(ui, display_name);
        }
        Action::OnenoteCreateSection {
            notebook_key,
            display_name,
        } => {
            app.create_onenote_section(ui, notebook_key, display_name);
        }
        Action::OnenoteCreatePage {
            section_key,
            title,
            markdown_body,
            inline_images,
            level,
        } => {
            app.create_onenote_page(ui, section_key, title, markdown_body, inline_images, level);
        }
        Action::OnenoteAppendMarkdown {
            page_key,
            markdown,
            inline_images,
        } => {
            app.append_onenote_markdown(ui, page_key, markdown, inline_images);
        }
        Action::OnenoteEditBlockSubmit {
            page_key,
            element_id,
            markdown,
        } => {
            app.edit_onenote_block(ui, page_key, element_id, markdown);
        }
        Action::OnenoteDeletePageConfirm => {
            app.delete_active_onenote_page(ui);
        }
        Action::OnenoteDownloadAttachment => {
            ui.info("미리보기에서 저장하세요");
        }
    }

    false
}

fn open_overlay(kind: OverlayKind, app: &mut AppState, ui: &mut UiState) {
    if kind.is_singleton() {
        ui.close_overlays_of(kind);
    }
    let overlay = match kind {
        OverlayKind::ContextMenu => {
            Overlay::context_menu(ContextMenu::build(ContextTarget::Global, (10, 5), app, ui))
        }
        OverlayKind::CommandPalette => Overlay::command_palette(),
        OverlayKind::Settings => Overlay::settings(),
        OverlayKind::Search => {
            let scope = match ui.active_nav_tab {
                TAB_MAIL => SearchScope::Mail,
                TAB_CONTACTS => SearchScope::Contacts,
                _ => SearchScope::TeamsMessages,
            };
            Overlay::Search(SearchOverlay::with_scope(scope))
        }
        OverlayKind::Presence => Overlay::presence(),
        OverlayKind::Reactions => {
            let (scope, idx) =
                selected_message_scope_and_index(app, ui).unwrap_or((MessageScope::Chat, 0));
            Overlay::reactions(scope, idx)
        }
        OverlayKind::Attachments => Overlay::attachments(),
        OverlayKind::PathInput => Overlay::path_input(),
        OverlayKind::NewChat => Overlay::new_chat(),
        OverlayKind::RoomInfo => Overlay::room_info(),
        OverlayKind::MailCompose => Overlay::MailCompose(MailComposeOverlay::new()),
        OverlayKind::EventCreate => Overlay::event_create(),
        OverlayKind::ContactCreate => Overlay::contact_create(),
        OverlayKind::OnenoteForm => Overlay::OnenoteForm(OnenoteFormOverlay::create_notebook()),
        OverlayKind::ImagePreview => {
            open_image_preview(app, ui);
            return;
        }
        OverlayKind::Help => Overlay::help(),
        OverlayKind::Confirm => Overlay::confirm("확인하시겠습니까?", None),
    };
    ui.push_overlay(overlay);
}

fn open_image_preview(app: &mut AppState, ui: &mut UiState) {
    let preview_opt = if ui.active_nav_tab == TAB_MAIL {
        app.active_mail()
            .and_then(|mail| ImagePreviewOverlay::from_mail_message(mail, Some(ui.mail_attach_index)))
    } else {
        selected_message_scope_and_index(app, ui)
            .and_then(|(scope, idx)| app.message_at(scope, idx))
            .and_then(|msg| ImagePreviewOverlay::from_teams_message(msg, Some(ui.attach_index)))
            .or_else(|| {
                app.messages
                    .iter()
                    .rev()
                    .find_map(|msg| ImagePreviewOverlay::from_teams_message(msg, None))
            })
    };

    let Some(preview) = preview_opt else {
        ui.info("미리볼 수 있는 이미지 첨부가 없습니다");
        return;
    };

    ui.close_overlays_of(OverlayKind::ImagePreview);
    if let Some(target) = preview.current().cloned() {
        let (cols, rows) = preview.target_dimensions(ui.term_width, ui.term_height);
        app.request_preview_image(&target, cols, rows);
    }
    ui.push_overlay(Overlay::ImagePreview(preview));
}

fn cycle_focus(app: &AppState, ui: &mut UiState, forward: bool) {
    let mut order = vec![Focus::Nav, Focus::Chat];
    if ui.active_nav_tab < TAB_MAIL {
        if ui.thread_drawer_open && app.active_thread.is_some() {
            order.push(Focus::Thread);
        }
        order.push(Focus::Composer);
    }

    let pos = order.iter().position(|f| *f == ui.focus).unwrap_or(0);
    let next_pos = if forward {
        (pos + 1) % order.len()
    } else {
        (pos + order.len() - 1) % order.len()
    };
    let next = order[next_pos];
    ui.set_focus(next);
    match next {
        Focus::Nav => ui.narrow_pane = NarrowPane::List,
        Focus::Chat => ui.narrow_pane = NarrowPane::Chat,
        Focus::Thread => ui.narrow_pane = NarrowPane::Thread,
        Focus::Composer | Focus::Overlay => {}
    }
}

fn selected_chat_index(app: &AppState, ui: &UiState) -> Option<usize> {
    if app.messages.is_empty() {
        return None;
    }
    Some(
        ui.chat_scroll
            .selected_idx
            .unwrap_or(app.messages.len().saturating_sub(1))
            .min(app.messages.len() - 1),
    )
}

fn selected_thread_scope_and_index(app: &AppState, ui: &UiState) -> Option<(MessageScope, usize)> {
    let thread = app.active_thread.as_ref()?;
    let total = 1 + thread.replies.len();
    let sel = ui
        .thread_scroll
        .selected_idx
        .unwrap_or(total.saturating_sub(1))
        .min(total.saturating_sub(1));
    if sel == 0 {
        Some((MessageScope::ThreadRoot, 0))
    } else {
        Some((MessageScope::ThreadReply, sel - 1))
    }
}

fn selected_message_scope_and_index(
    app: &AppState,
    ui: &UiState,
) -> Option<(MessageScope, usize)> {
    match ui.focus() {
        Focus::Thread => selected_thread_scope_and_index(app, ui),
        _ => selected_chat_index(app, ui).map(|i| (MessageScope::Chat, i)),
    }
}

fn move_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    match ui.focus() {
        Focus::Nav => {
            let len = nav_pane::build_nav_rows(app, ui).len();
            if len == 0 {
                return;
            }
            let cur = ui.nav_selected.min(len - 1) as i32;
            let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
            ui.nav_selected = next;
        }
        Focus::Chat => {
            if ui.active_nav_tab >= TAB_MAIL {
                if delta < 0 {
                    ui.mail_scroll.scroll_up((-delta) as u16);
                } else if delta > 0 {
                    ui.mail_scroll.scroll_down(delta as u16);
                }
                return;
            }
            let len = app.messages.len();
            if len == 0 {
                return;
            }
            let cur = ui.chat_scroll.selected_idx.unwrap_or(len - 1) as i32;
            let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
            ui.chat_scroll.selected_idx = Some(next);
            if delta < 0 && next == 0 {
                ui.chat_scroll.scroll_up(2);
            } else if delta > 0 && next == len - 1 {
                ui.chat_scroll.bottom();
            }
        }
        Focus::Thread => {
            let len = app
                .active_thread
                .as_ref()
                .map(|t| 1 + t.replies.len())
                .unwrap_or(0);
            if len == 0 {
                return;
            }
            let cur = ui.thread_scroll.selected_idx.unwrap_or(len - 1) as i32;
            let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
            ui.thread_scroll.selected_idx = Some(next);
            if delta < 0 && next == 0 {
                ui.thread_scroll.scroll_up(2);
            } else if delta > 0 && next == len - 1 {
                ui.thread_scroll.bottom();
            }
        }
        Focus::Composer | Focus::Overlay => {}
    }
}

fn scroll_focused(ui: &mut UiState, delta: i32) {
    let target = match ui.focus() {
        Focus::Thread => &mut ui.thread_scroll,
        _ if ui.active_nav_tab >= TAB_MAIL => &mut ui.mail_scroll,
        _ => &mut ui.chat_scroll,
    };
    if delta < 0 {
        target.scroll_up((-delta) as u16);
    } else if delta > 0 {
        target.scroll_down(delta as u16);
    }
}

fn handle_activate(app: &mut AppState, ui: &mut UiState) {
    match ui.focus() {
        Focus::Nav => {
            let idx = ui.nav_selected;
            app.select_nav_row(ui, idx);
        }
        Focus::Chat => {
            match ui.active_nav_tab {
                TAB_MAIL => {
                    let _ = dispatch(Action::OpenMailReply { reply_all: false }, app, ui);
                }
                TAB_CALENDAR => {
                    app.open_active_event_meeting(ui);
                }
                TAB_CONTACTS => {
                    let _ = dispatch(Action::ContactSendMail, app, ui);
                }
                TAB_TODO => {
                    app.toggle_active_todo_completed(ui);
                }
                _ => {
                    if let Some(idx) = selected_chat_index(app, ui) {
                        if app
                            .active_target
                            .as_ref()
                            .is_some_and(ConversationTarget::is_channel)
                        {
                            app.open_thread(ui, idx);
                        } else {
                            let _ = dispatch(
                                Action::ReplyTo {
                                    scope: MessageScope::Chat,
                                    msg_index: idx,
                                },
                                app,
                                ui,
                            );
                        }
                    }
                }
            }
        }
        Focus::Thread => {
            ui.composer_target_thread = true;
            ui.set_focus(Focus::Composer);
        }
        Focus::Composer | Focus::Overlay => {}
    }
}

fn handle_click(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    let target = ui.hitmap.hit(row, column);

    if !ui.overlays.is_empty() {
        match target {
            Some(HitTarget::OverlayItem { index }) => {
                let is_ctx = ui
                    .overlays
                    .last()
                    .is_some_and(|o| o.kind() == OverlayKind::ContextMenu);
                dispatch(Action::OverlaySelect(index), app, ui);
                if is_ctx {
                    return dispatch(Action::Activate, app, ui);
                }
                return false;
            }
            None => {
                ui.close_top_overlay();
                return false;
            }
            _ => return false,
        }
    }

    if let Some(hit) = target {
        if handle_onenote_hit(hit, app, ui) {
            return false;
        }
        let act = action_for_target(hit);
        return dispatch(act, app, ui);
    }

    let pos = Position { x: column, y: row };
    if ui.thread_drawer_open && ui.thread_scroll.area.contains(pos) {
        ui.set_focus(Focus::Thread);
    } else if ui.chat_scroll.area.contains(pos) || ui.mail_scroll.area.contains(pos) {
        ui.set_focus(Focus::Chat);
    }
    false
}

fn action_for_target(target: HitTarget) -> Action {
    match target {
        HitTarget::NavTab(tab) => Action::SelectNavTab(tab),
        HitTarget::NavRow(row) => Action::SelectNavRow(row),
        HitTarget::Message { scope, msg_index } => Action::SelectMessage { scope, msg_index },
        HitTarget::Attachment {
            scope,
            msg_index,
            attach_index,
        } => Action::DownloadAttachment {
            scope,
            msg_index,
            attach_index,
        },
        HitTarget::Reaction {
            scope,
            msg_index,
            reaction_index,
        } => Action::ToggleReactionByIndex {
            scope,
            msg_index,
            reaction_index,
        },
        HitTarget::OpenThread { msg_index } => Action::OpenThread { msg_index },
        HitTarget::Button(btn) => match btn {
            ButtonId::Search => Action::OpenOverlay(OverlayKind::Search),
            ButtonId::SendFile => Action::OpenOverlay(OverlayKind::PathInput),
            ButtonId::Attachments => Action::OpenOverlay(OverlayKind::Attachments),
            ButtonId::RoomInfo => Action::OpenOverlay(OverlayKind::RoomInfo),
            ButtonId::Refresh => Action::RefreshAll,
            ButtonId::Settings => Action::OpenOverlay(OverlayKind::Settings),
            ButtonId::NewChat => Action::OpenOverlay(OverlayKind::NewChat),
            ButtonId::Presence => Action::OpenOverlay(OverlayKind::Presence),
            ButtonId::CloseThread => Action::CloseThreadDrawer,
            ButtonId::ComposerTargetToggle => Action::ToggleComposerTarget,
            ButtonId::SubjectToggle => Action::ToggleSubject,
            ButtonId::NarrowToggle => Action::ToggleNarrowPane,
            ButtonId::NewMail => Action::OpenMailComposeNew,
            ButtonId::MailReply => Action::OpenMailReply { reply_all: false },
            ButtonId::MailReplyAll => Action::OpenMailReply { reply_all: true },
            ButtonId::MailForward => Action::OpenMailForward,
            ButtonId::MailToggleRead => Action::MailToggleRead,
            ButtonId::MailToggleFlag => Action::MailToggleFlag,
            ButtonId::MailArchive => Action::MailArchive,
            ButtonId::MailDelete => Action::MailDeleteRequest,
            ButtonId::ImagePreview => Action::OpenImagePreview,
            ButtonId::NewEvent => Action::OpenOverlay(OverlayKind::EventCreate),
            ButtonId::JoinMeeting => Action::JoinCalendarMeeting,
            ButtonId::RsvpAccept => Action::RespondCalendarEvent("accept".to_string()),
            ButtonId::RsvpTentative => {
                Action::RespondCalendarEvent("tentativelyAccept".to_string())
            }
            ButtonId::RsvpDecline => Action::RespondCalendarEvent("decline".to_string()),
            ButtonId::DeleteEvent => Action::DeleteCalendarEventRequest,
            ButtonId::NewContact => Action::OpenOverlay(OverlayKind::ContactCreate),
            ButtonId::ContactMail => Action::ContactSendMail,
            ButtonId::ContactChat => Action::ContactStartChat,
            ButtonId::DeleteContact => Action::DeleteContactRequest,
            ButtonId::NewTodo | ButtonId::TodoInputFocus => Action::TodoFocusInput,
            ButtonId::TodoToggle => Action::TodoToggleSelected,
            ButtonId::DeleteTodo => Action::TodoDeleteRequest,
            // Microsoft OneNote toolbar
            ButtonId::OnenoteNewNotebook => Action::OnenoteOpenCreateNotebook,
            ButtonId::OnenoteNewSection => Action::OnenoteOpenCreateSection,
            ButtonId::OnenoteNewPage => Action::OnenoteOpenCreatePage { subpage: false },
            ButtonId::OnenoteNewSubpage => Action::OnenoteOpenCreatePage { subpage: true },
            // These need direct `AppState` access, so `handle_onenote_hit` consumes them first.
            ButtonId::OnenoteAppendBlock
            | ButtonId::OnenoteEditBlock
            | ButtonId::OnenoteEditFullPage
            | ButtonId::OnenoteToggleTodo
            | ButtonId::OnenoteOpenWeb
            | ButtonId::OnenoteRefresh
            | ButtonId::OnenoteDeletePage => Action::Noop,
        },
        HitTarget::SubjectInput => Action::FocusSubject,
        HitTarget::Composer => Action::OpenComposer,
        HitTarget::OverlayItem { index } => Action::OverlaySelect(index),
        // OneNote hit targets resolve against `AppState`, so they are handled in
        // `handle_onenote_hit` before this mapper runs.
        HitTarget::OnenoteSectionTab(_)
        | HitTarget::OnenotePage(_)
        | HitTarget::OnenoteBlock(_)
        | HitTarget::OnenoteTodoToggle(_)
        | HitTarget::OnenoteResource { .. } => Action::Noop,
    }
}

/// Handles OneNote hit targets and toolbar buttons that require direct `AppState` access.
///
/// Returns `true` when the target was consumed.
fn handle_onenote_hit(hit: HitTarget, app: &mut AppState, ui: &mut UiState) -> bool {
    match hit {
        HitTarget::OnenoteSectionTab(idx) => {
            match onenote_section_key_at(app, idx) {
                Some(section_key) => app.open_onenote_section(ui, section_key),
                None => ui.info("선택할 OneNote 섹션이 없습니다"),
            }
            true
        }
        HitTarget::OnenotePage(idx) => {
            match app.onenote_pages.get(idx).map(|page| page.page_key.clone()) {
                Some(page_key) => app.open_onenote_page(ui, page_key),
                None => ui.info("선택할 OneNote 페이지가 없습니다"),
            }
            true
        }
        HitTarget::OnenoteBlock(idx) => {
            app.select_onenote_block(ui, idx);
            true
        }
        HitTarget::OnenoteTodoToggle(idx) => {
            app.toggle_onenote_todo(ui, idx);
            true
        }
        HitTarget::OnenoteResource {
            resource_index,
            is_image,
        } => {
            if is_image {
                open_onenote_image_preview(app, ui, Some(resource_index));
            } else {
                ui.info("이미지가 아닌 첨부파일입니다 ('d'로 저장)");
            }
            true
        }
        HitTarget::Button(btn) => handle_onenote_button(btn, app, ui),
        _ => false,
    }
}

/// Handles the OneNote toolbar buttons that cannot be expressed as an `Action`.
fn handle_onenote_button(btn: ButtonId, app: &mut AppState, ui: &mut UiState) -> bool {
    match btn {
        ButtonId::OnenoteAppendBlock => open_onenote_append_form(app, ui),
        ButtonId::OnenoteEditBlock => open_onenote_edit_block_form(app, ui),
        ButtonId::OnenoteEditFullPage => open_onenote_edit_page_form(app, ui),
        ButtonId::OnenoteToggleTodo => {
            let idx = ui.onenote_selected_block;
            app.toggle_onenote_todo(ui, idx);
        }
        ButtonId::OnenoteOpenWeb => app.open_active_onenote_in_browser(ui),
        ButtonId::OnenoteRefresh => app.refresh_onenote(ui),
        ButtonId::OnenoteDeletePage => request_onenote_page_delete(app, ui),
        _ => return false,
    }
    true
}

/// Resolves a section ribbon index against the sections of the active notebook.
fn onenote_section_key_at(app: &AppState, idx: usize) -> Option<String> {
    let active_notebook_key = app.active_notebook_key.as_deref();
    app.onenote_sections
        .iter()
        .filter(|sec| match active_notebook_key {
            Some(key) => app
                .onenote_notebooks
                .iter()
                .find(|nb| nb.notebook_key == key)
                .is_none_or(|nb| nav_pane::section_belongs_to_notebook(sec, nb)),
            None => true,
        })
        .nth(idx)
        .map(|sec| sec.section_key.clone())
}

fn request_onenote_page_delete(app: &AppState, ui: &mut UiState) {
    let Some(detail) = app.active_page_detail.as_ref() else {
        ui.info("삭제할 OneNote 페이지를 먼저 선택하세요");
        return;
    };
    let msg = format!(
        "OneNote 페이지 '{}' 을(를) 삭제하시겠습니까?",
        detail.summary.title
    );
    ui.push_overlay(Overlay::confirm(
        msg,
        Some(Action::OnenoteDeletePageConfirm),
    ));
}

/// Opens the append-markdown form for the active page (`a`).
fn open_onenote_append_form(app: &AppState, ui: &mut UiState) {
    let Some(detail) = app.active_page_detail.as_ref() else {
        ui.info("먼저 OneNote 페이지를 선택하세요");
        return;
    };
    let form = OnenoteFormOverlay::append_markdown(
        detail.summary.page_key.clone(),
        detail.summary.title.clone(),
    );
    ui.push_overlay(Overlay::OnenoteForm(form));
}

/// Opens the surgical block edit form for the selected block (`e`).
fn open_onenote_edit_block_form(app: &AppState, ui: &mut UiState) {
    let Some(detail) = app.active_page_detail.as_ref() else {
        ui.info("먼저 OneNote 페이지를 선택하세요");
        return;
    };
    let block_index = ui.onenote_selected_block;
    let Some(block) = detail.blocks.get(block_index) else {
        ui.info("편집할 OneNote 블록을 선택하세요");
        return;
    };
    let Some(element_id) = block.element_id.clone() else {
        ui.info("이 블록은 편집할 수 없습니다 (element ID 없음)");
        return;
    };
    let initial_markdown =
        crate::parser::onenote::onenote_blocks_to_markdown(std::slice::from_ref(block));
    let form = OnenoteFormOverlay::edit_block(
        detail.summary.page_key.clone(),
        detail.summary.title.clone(),
        element_id,
        block_index,
        initial_markdown,
    );
    ui.push_overlay(Overlay::OnenoteForm(form));
}

/// Opens the whole-page markdown edit form (`E`), replacing the page body container.
fn open_onenote_edit_page_form(app: &AppState, ui: &mut UiState) {
    let Some(detail) = app.active_page_detail.as_ref() else {
        ui.info("먼저 OneNote 페이지를 선택하세요");
        return;
    };
    let Some(element_id) = detail.primary_div_id.clone() else {
        ui.info("페이지 본문 컨테이너를 찾을 수 없어 전체 편집을 열 수 없습니다");
        return;
    };
    let form = OnenoteFormOverlay::edit_block(
        detail.summary.page_key.clone(),
        detail.summary.title.clone(),
        element_id,
        0,
        detail.body_markdown.clone(),
    );
    ui.push_overlay(Overlay::OnenoteForm(form));
}

/// Opens the image preview overlay for the active page's resource at `resource_index`.
fn open_onenote_image_preview(
    app: &mut AppState,
    ui: &mut UiState,
    preferred_res_idx: Option<usize>,
) {
    let preview_opt = app
        .active_page_detail
        .as_ref()
        .and_then(|detail| ImagePreviewOverlay::from_onenote_page(detail, preferred_res_idx));

    let Some(preview) = preview_opt else {
        ui.info("미리볼 수 있는 OneNote 이미지가 없습니다");
        return;
    };

    ui.close_overlays_of(OverlayKind::ImagePreview);
    if let Some(target) = preview.current().cloned() {
        let (cols, rows) = preview.target_dimensions(ui.term_width, ui.term_height);
        app.request_preview_image(&target, cols, rows);
    }
    ui.push_overlay(Overlay::ImagePreview(preview));
}

fn open_context_menu_at(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) {
    let ctx = match ui.hitmap.hit(row, column) {
        Some(HitTarget::NavRow(idx)) => ContextTarget::NavRow(idx),
        Some(HitTarget::Message { scope, msg_index })
        | Some(HitTarget::Attachment {
            scope, msg_index, ..
        })
        | Some(HitTarget::Reaction {
            scope, msg_index, ..
        }) => ContextTarget::Message { scope, msg_index },
        Some(HitTarget::OpenThread { msg_index }) => ContextTarget::Message {
            scope: MessageScope::Chat,
            msg_index,
        },
        Some(HitTarget::Composer) | Some(HitTarget::SubjectInput) => ContextTarget::Composer,
        _ => ContextTarget::Global,
    };
    ui.close_overlays_of(OverlayKind::ContextMenu);
    let menu = ContextMenu::build(ctx, (column, row), app, ui);
    ui.push_overlay(Overlay::context_menu(menu));
}

fn copy_to_clipboard(text: &str, ui: &mut UiState) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => ui.info("클립보드에 복사했습니다"),
        Err(err) => ui.info(format!("클립보드 복사 실패: {err}")),
    }
}

fn paste_clipboard(app: &mut AppState, ui: &mut UiState) {
    let Ok(mut cb) = arboard::Clipboard::new() else {
        ui.info("클립보드를 열 수 없습니다");
        return;
    };

    if let Ok(text) = cb.get_text() {
        if !text.is_empty() {
            if !ui.overlays.is_empty() {
                dispatch(Action::Paste(text), app, ui);
                return;
            }
            if !matches!(ui.focus(), Focus::Composer) {
                ui.set_focus(Focus::Composer);
            }
            ui.composer.insert_str(&text);
            ui.composer.reference_mentions(&app.members);
            return;
        }
    }

    if let Ok(img) = cb.get_image() {
        let width = img.width as u32;
        let height = img.height as u32;
        match composer::encode_rgba_to_png(width, height, img.bytes.into_owned()) {
            Ok(png_bytes) => {
                let n = ui.composer.pending_images.len() + 1;
                ui.composer.stage_inline_image(
                    png_bytes,
                    "image/png",
                    Some(format!("clipboard-{n}.png")),
                );
                ui.set_focus(Focus::Composer);
                ui.info(format!("클립보드 이미지 첨부 ({width}x{height})"));
            }
            Err(err) => {
                ui.error(format!("이미지 인코딩 실패: {err}"));
            }
        }
    }
}
