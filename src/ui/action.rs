//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use chrono::Utc;
use ratatui::layout::Position;

use crate::models::ConversationTarget;
use crate::sync::SyncCommand;

use super::app::{AppState, Focus, NarrowPane, UiState};
use super::composer::{self, ReplyPreview};
use super::hitmap::{ButtonId, HitTarget, MessageScope};
use super::nav_pane;
use super::overlays::context_menu::{ContextMenu, ContextTarget};
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
    Click {
        column: u16,
        row: u16,
    },
    ShowToast(String),
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
            ui.active_nav_tab = tab.min(2);
            ui.nav_selected = 0;
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::Nav);
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
            app.sync_ctrl.request(SyncCommand::SyncActiveNow);
            ui.info("대화 목록과 메시지를 새로고침합니다...");
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
        Action::Click { .. } | Action::OpenContextMenuAt { .. } => unreachable!(),
        Action::ShowToast(msg) => {
            ui.info(msg);
        }
    }

    false
}

fn open_overlay(kind: OverlayKind, app: &AppState, ui: &mut UiState) {
    if kind.is_singleton() {
        ui.close_overlays_of(kind);
    }
    let overlay = match kind {
        OverlayKind::ContextMenu => {
            Overlay::context_menu(ContextMenu::build(ContextTarget::Global, (10, 5), app, ui))
        }
        OverlayKind::CommandPalette => Overlay::command_palette(),
        OverlayKind::Settings => Overlay::settings(),
        OverlayKind::Search => Overlay::search(),
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
        OverlayKind::Help => Overlay::help(),
        OverlayKind::Confirm => Overlay::confirm("확인하시겠습니까?", None),
    };
    ui.push_overlay(overlay);
}

fn cycle_focus(app: &AppState, ui: &mut UiState, forward: bool) {
    let mut order = vec![Focus::Nav, Focus::Chat];
    if ui.thread_drawer_open && app.active_thread.is_some() {
        order.push(Focus::Thread);
    }
    order.push(Focus::Composer);

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
        let act = action_for_target(hit);
        return dispatch(act, app, ui);
    }

    let pos = Position { x: column, y: row };
    if ui.thread_drawer_open && ui.thread_scroll.area.contains(pos) {
        ui.set_focus(Focus::Thread);
    } else if ui.chat_scroll.area.contains(pos) {
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
        },
        HitTarget::SubjectInput => Action::FocusSubject,
        HitTarget::Composer => Action::OpenComposer,
        HitTarget::OverlayItem { index } => Action::OverlaySelect(index),
    }
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
