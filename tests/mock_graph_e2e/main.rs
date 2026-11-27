//! End-to-end integration tests for `outlook-cli` against an in-process mock
//! Microsoft Graph v1.0, Entra OAuth 2.0, and GitHub Releases server.

mod core_e2e;
mod pagination_and_calendar;
mod plan_contracts;
mod regressions_folders_media;
mod regressions_mail_sync;
mod server;
