//! Image caches, inline/preview image decoding, HTML attachment preview, and self-update checks.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};

use crate::img_preview::{
    self, FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
};
use crate::models::MailAttachment;
use crate::parser;

use super::super::overlays::html_preview::HtmlPreviewOverlay;
use super::super::overlays::image_preview::PreviewTarget;
use super::super::overlays::{Overlay, OverlayKind};
use super::super::toast::ToastKind;
use super::state::{MAX_RAW_IMAGE_CACHE_BYTES, MAX_RAW_IMAGE_CACHE_ENTRIES};
use super::{AppState, UiMsg, UiState};

impl AppState {
    pub fn image_cache_key(
        owner_id: &str,
        att_id: &str,
        cols: u16,
        rows: u16,
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{cols}x{rows}:{}x{}:{:02x}{:02x}{:02x}",
            cell_px.0, cell_px.1, bg_rgb.0, bg_rgb.1, bg_rgb.2
        )
    }

    pub fn preview_image_cache_key(
        owner_id: &str,
        att_id: &str,
        view_cols: u16,
        view_rows: u16,
        zoom_pct: u16,
        cell_px: (u16, u16),
        protocol: TerminalImageProtocol,
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{view_cols}x{view_rows}:{zoom_pct}%:{}x{}:{}",
            cell_px.0,
            cell_px.1,
            protocol.as_str()
        )
    }

    pub fn raw_image_cache_key(owner_id: &str, att_id: &str) -> String {
        format!("{owner_id}:{att_id}")
    }

    pub fn insert_raw_image_cache(&mut self, raw_key: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.image_raw_cache.remove(&raw_key) {
            self.image_raw_bytes = self.image_raw_bytes.saturating_sub(prev.len());
            self.image_raw_order.retain(|k| k != &raw_key);
        }
        self.image_raw_cache.insert(raw_key.clone(), bytes);
        self.image_raw_order.push_back(raw_key);
        self.image_raw_bytes = self.image_raw_bytes.saturating_add(entry_bytes);

        while self.image_raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
        while self.image_raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.image_raw_order.len() > 1 {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    pub(super) fn touch_raw_image_cache(&mut self, raw_key: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(raw_key).cloned()?;
        self.image_raw_order.retain(|k| k != raw_key);
        self.image_raw_order.push_back(raw_key.to_string());
        Some(bytes)
    }

    pub(super) fn rendered_image_bytes(rendered: &RenderedTerminalImage) -> usize {
        rendered.approx_heap_bytes().max(64)
    }

    pub fn insert_image_cache(&mut self, cache_key: String, rendered: RenderedTerminalImage) {
        const MAX_IMAGE_CACHE: usize = 32;
        self.image_pending.remove(&cache_key);
        self.clear_image_error(&cache_key);

        if let Some(prev) = self.image_cache.remove(&cache_key) {
            self.image_cache_bytes = self
                .image_cache_bytes
                .saturating_sub(Self::rendered_image_bytes(&prev));
            self.image_cache_order.retain(|k| k != &cache_key);
        }
        let entry_bytes = Self::rendered_image_bytes(&rendered);
        self.image_cache.insert(cache_key.clone(), rendered);
        self.image_cache_order.push_back(cache_key);
        self.image_cache_bytes = self.image_cache_bytes.saturating_add(entry_bytes);

        while self.image_cache_order.len() > MAX_IMAGE_CACHE {
            let Some(evicted) = self.image_cache_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_cache.remove(&evicted) {
                self.image_cache_bytes = self
                    .image_cache_bytes
                    .saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
        while self.image_cache_bytes > img_preview::MAX_IMAGE_CACHE_BYTES
            && self.image_cache_order.len() > 1
        {
            let Some(evicted) = self.image_cache_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_cache.remove(&evicted) {
                self.image_cache_bytes = self
                    .image_cache_bytes
                    .saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn insert_image_error(&mut self, cache_key: String, error: String) {
        const MAX_IMAGE_ERRORS: usize = 64;
        if self.image_errors.contains_key(&cache_key) {
            self.image_errors_order.retain(|k| k != &cache_key);
        }
        self.image_errors.insert(cache_key.clone(), error);
        self.image_errors_order.push_back(cache_key);
        while self.image_errors_order.len() > MAX_IMAGE_ERRORS {
            if let Some(evicted) = self.image_errors_order.pop_front() {
                self.image_errors.remove(&evicted);
            }
        }
    }

    pub fn clear_image_error(&mut self, cache_key: &str) {
        if self.image_errors.remove(cache_key).is_some() {
            self.image_errors_order.retain(|k| k != cache_key);
        }
    }

    /// Clears rendered terminal image caches (keeping fetched raw bytes intact, e.g. on theme change).
    pub fn clear_rendered_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn clear_image_caches(&mut self) {
        self.clear_rendered_image_cache();
        self.image_raw_cache.clear();
        self.image_raw_order.clear();
        self.image_raw_bytes = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_mail_image_decode(
        &mut self,
        key: String,
        msg_id: String,
        att: MailAttachment,
        fit: FitBox,
        bg: (u8, u8, u8),
        cell_px: (u16, u16),
        caps: Option<TerminalGraphicsCaps>,
    ) {
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.image_pending.insert(key.clone());
        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        let cached_raw = self.touch_raw_image_cache(&raw_key);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let from_preview = caps.is_some();
            let att_title = if att.name.trim().is_empty() {
                att_id.clone()
            } else {
                att.name.clone()
            };
            let fetched: anyhow::Result<(Arc<[u8]>, Option<String>, bool)> = match cached_raw {
                Some(bytes) => {
                    let mime = (!att.content_type.trim().is_empty())
                        .then(|| att.content_type.clone());
                    Ok((bytes, mime, false))
                }
                None => {
                    if let Some(b64) = att
                        .content_bytes_b64
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                        match B64_STANDARD.decode(cleaned) {
                            Ok(bytes) => {
                                let mime = (!att.content_type.trim().is_empty())
                                    .then(|| att.content_type.clone());
                                Ok((Arc::from(bytes), mime, true))
                            }
                            Err(e) => Err(anyhow::anyhow!("Invalid base64 image: {e}")),
                        }
                    } else if att.id.trim().len() >= 11
                        && att.id.trim()[..11].eq_ignore_ascii_case("data:image/")
                    {
                        match img_preview::decode_data_image_uri(att.id.trim()) {
                            Ok((bytes, mime)) => Ok((Arc::from(bytes), Some(mime), true)),
                            Err(e) => Err(e),
                        }
                    } else if att.id.starts_with("https://") || att.id.starts_with("http://") {
                        match graph.http().get(&att.id).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                let resp_ct = resp
                                    .headers()
                                    .get(reqwest::header::CONTENT_TYPE)
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("")
                                    .split(';')
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .to_ascii_lowercase();
                                match resp.bytes().await {
                                    Ok(b) => {
                                        let mime = (!resp_ct.is_empty()).then_some(resp_ct);
                                        Ok((Arc::from(b.to_vec()), mime, true))
                                    }
                                    Err(e) => Err(anyhow::anyhow!(e)),
                                }
                            }
                            Ok(resp) => Err(anyhow::anyhow!("HTTP {}", resp.status())),
                            Err(e) => Err(anyhow::anyhow!(e)),
                        }
                    } else {
                        match graph.fetch_mail_attachment_bytes(&msg_id, &att.id).await {
                            Ok((_, ct, bytes)) => {
                                let mime = (!ct.trim().is_empty())
                                    .then_some(ct)
                                    .or_else(|| {
                                        (!att.content_type.trim().is_empty())
                                            .then(|| att.content_type.clone())
                                    });
                                Ok((Arc::from(bytes), mime, true))
                            }
                            Err(err) => Err(err),
                        }
                    }
                }
            };

            match fetched {
                Ok((bytes, mime_hint, is_fresh)) => {
                    let kind = bitefoundry_html::sniff_content_kind(
                        mime_hint.as_deref(),
                        Some(&att.name),
                        Some(&bytes),
                    );
                    if kind == bitefoundry_html::ContentKind::Html {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = tx.send(UiMsg::ImageFetchReturnedHtml {
                            cache_key: key,
                            owner_id: msg_id,
                            att_id,
                            title: att_title,
                            html,
                            from_preview,
                        });
                        return;
                    }
                    if is_fresh {
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            raw_key,
                            bytes: Arc::clone(&bytes),
                        });
                    }
                    let decode_res = tokio::task::spawn_blocking(move || match caps {
                        Some(c) => img_preview::decode_and_render_for_protocol(&bytes, fit, bg, c),
                        None => img_preview::decode_and_render_halfblocks_with_cell_px(
                            &bytes, fit, bg, cell_px,
                        ),
                    })
                    .await;
                    match decode_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageDecoded {
                                cache_key: key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: msg_id,
                        att_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_mail_image(
        &mut self,
        msg_id: String,
        att: MailAttachment,
        cols: u16,
        rows: u16,
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows, cell_px, bg_rgb);
        self.spawn_mail_image_decode(
            key,
            msg_id,
            att,
            FitBox::fixed(cols, rows),
            bg_rgb,
            cell_px,
            None,
        );
    }

    pub fn request_preview_image(
        &mut self,
        target: &PreviewTarget,
        fit: FitBox,
        caps: TerminalGraphicsCaps,
        bg_rgb: (u8, u8, u8),
    ) {
        match target {
            PreviewTarget::Mail { msg_id, attachment } => {
                let key = Self::preview_image_cache_key(
                    msg_id,
                    &attachment.id,
                    fit.view_cols,
                    fit.view_rows,
                    fit.zoom_pct,
                    caps.cell_px,
                    caps.protocol,
                );
                self.spawn_mail_image_decode(
                    key,
                    msg_id.clone(),
                    attachment.clone(),
                    fit,
                    bg_rgb,
                    caps.cell_px,
                    Some(caps),
                );
            }
        }
    }

    pub fn open_mail_html_attachment(
        &mut self,
        ui: &mut UiState,
        msg_id: String,
        attach_index: usize,
    ) {
        let Some(msg) = self.resolve_mail_for_action(&msg_id) else {
            ui.info("Email not found");
            return;
        };
        let eff = parser::effective_mail_attachments(&msg);
        let Some(att) = eff.get(attach_index).cloned() else {
            ui.info("Attachment not found");
            return;
        };
        let title = if att.name.trim().is_empty() {
            if super::super::overlays::html_preview::is_markdown_mime_or_name(&att.content_type, None) {
                "attachment.md".to_string()
            } else {
                "attachment.html".to_string()
            }
        } else {
            att.name.clone()
        };

        if let Some(b64) = att
            .content_bytes_b64
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            match B64_STANDARD.decode(cleaned) {
                Ok(bytes) => {
                    let kind = super::super::overlays::html_preview::classify_markup_content(
                        Some(&att.content_type),
                        Some(&title),
                        &bytes,
                    );
                    let raw = String::from_utf8_lossy(&bytes).into_owned();
                    let mut overlay = if kind == bitefoundry_html::ContentKind::Markdown {
                        HtmlPreviewOverlay::new_markdown(Some(title), raw)
                    } else {
                        HtmlPreviewOverlay::new(Some(title), raw)
                    };
                    overlay.show_link_urls = ui.show_link_urls;
                    ui.close_overlays_of(OverlayKind::HtmlPreview);
                    ui.push_overlay(Overlay::html_preview(overlay));
                    return;
                }
                Err(err) => {
                    ui.error(format!("Invalid HTML attachment encoding: {err}"));
                    return;
                }
            }
        }

        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        if let Some(bytes) = self.touch_raw_image_cache(&raw_key) {
            let kind = super::super::overlays::html_preview::classify_markup_content(
                Some(&att.content_type),
                Some(&title),
                &bytes,
            );
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            let mut overlay = if kind == bitefoundry_html::ContentKind::Markdown {
                HtmlPreviewOverlay::new_markdown(Some(title), raw)
            } else {
                HtmlPreviewOverlay::new(Some(title), raw)
            };
            overlay.show_link_urls = ui.show_link_urls;
            ui.close_overlays_of(OverlayKind::HtmlPreview);
            ui.push_overlay(Overlay::html_preview(overlay));
            return;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot fetch HTML attachment without async runtime");
            return;
        }

        let task_label = format!("Load {title}");
        ui.tasks.begin(task_label.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let att_content_type = att.content_type.clone();
        tokio::spawn(async move {
            let res: anyhow::Result<(Option<String>, Vec<u8>)> =
                if att.id.starts_with("https://") || att.id.starts_with("http://") {
                    match graph.http().get(&att.id).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            let mime = resp
                                .headers()
                                .get(reqwest::header::CONTENT_TYPE)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                                .or_else(|| {
                                    (!att_content_type.trim().is_empty())
                                        .then(|| att_content_type.clone())
                                });
                            resp.bytes()
                                .await
                                .map(|b| (mime, b.to_vec()))
                                .map_err(Into::into)
                        }
                        Ok(resp) => Err(anyhow::anyhow!("HTTP {}", resp.status())),
                        Err(err) => Err(err.into()),
                    }
                } else {
                    graph
                        .fetch_mail_attachment_bytes(&msg_id, &att.id)
                        .await
                        .map(|(_, ct, b)| {
                            let mime = if ct.trim().is_empty() {
                                (!att_content_type.trim().is_empty())
                                    .then(|| att_content_type.clone())
                            } else {
                                Some(ct)
                            };
                            (mime, b)
                        })
                };

            match res {
                Ok((content_type, bytes)) => {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::HtmlAttachmentLoaded {
                        title,
                        content_type,
                        html,
                    });
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: None,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("Failed to load HTML attachment: {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    /// Serves an HTML document on the lazy localhost Axum server (`0 MB` idle) and returns its URL.
    pub async fn serve_html_document(&self, title: &str, html: &str) -> anyhow::Result<String> {
        self.html_server.serve_html(title, html).await
    }

    /// Serves `html` via `NativeHtmlServer::serve_html` and opens the localhost URL in the default browser.
    pub fn serve_html_in_browser(&self, ui: &mut UiState, title: String, html: String) {
        if tokio::runtime::Handle::try_current().is_err() {
            ui.error("Cannot start native HTML server without async runtime");
            return;
        }
        let task_label = "Serve HTML in browser".to_string();
        ui.tasks.begin(task_label.clone());
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match server.serve_html(&title, &html).await {
                Ok(url) => match open::that(&url) {
                    Ok(()) => {
                        let _ = tx.send(UiMsg::TaskDone {
                            label: task_label,
                            error: None,
                            toast: Some((
                                format!("Opened in browser: {url}"),
                                ToastKind::Success,
                            )),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskDone {
                            label: task_label,
                            error: Some(err.to_string()),
                            toast: Some((
                                format!("Served at {url} (open failed: {err})"),
                                ToastKind::Error,
                            )),
                        });
                    }
                },
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((
                            format!("Failed to serve HTML: {err}"),
                            ToastKind::Error,
                        )),
                    });
                }
            }
        });
    }

    pub fn check_for_updates(&mut self, ui: &mut UiState, check_only: bool, force: bool) {
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        let task_label = if check_only {
            "Check updates".to_string()
        } else {
            "Install update".to_string()
        };
        ui.tasks.begin(task_label.clone());

        tokio::spawn(async move {
            match crate::updater::check_or_apply_update(&repo, check_only, force).await {
                Ok(res) => {
                    let toast_msg = if res.updated {
                        format!(
                            "Updated: {} -> {} (restart recommended)",
                            res.current_version, res.latest_version
                        )
                    } else if res.update_available {
                        format!(
                            "Update available: {} (current {})",
                            res.latest_version, res.current_version
                        )
                    } else {
                        format!("Up to date ({})", res.current_version)
                    };
                    let _ = tx.send(UiMsg::UpdateChecked(res));
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: None,
                        toast: Some((toast_msg, ToastKind::Success)),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskDone {
                        label: task_label,
                        error: Some(err.to_string()),
                        toast: Some((format!("Update check failed: {err}"), ToastKind::Error)),
                    });
                }
            }
        });
    }
}
