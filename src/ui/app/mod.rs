//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

mod cache;
mod events;
mod messages;
mod navigation;
mod requests_cal_todo;
mod requests_mail;
mod requests_media;
mod state;

#[cfg(test)]
mod tests;

pub use state::{
    AppState, CalendarRangeFilter, Focus, MailListRow, NarrowPane, PendingPixelGraphic,
    ScrollState, UiMsg, UiState,
};
