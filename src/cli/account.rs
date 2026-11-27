//! Authentication, profile (`whoami`), diagnostics (`doctor`), and TUI launch commands.

use std::path::Path;

use anyhow::{bail, Result};
use serde_json::json;

use crate::{
    auth::{diagnose_access_token_scopes, AuthManager, LoginOptions, StoredTokens},
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    storage::Storage,
    tui,
};

use super::{
    format::{
        default_calendar_window, format_unix_ts, truncate_single_line, wrap_graph_permission_error,
    },
    runtime::{error_with_info, open_runtime},
};

pub(super) async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    if auth.current_tokens().await.is_none() {
        println!("No stored sign-in credentials found. Starting Microsoft account sign-in.");
        println!(
            "(To use a custom Azure App: {} login --client-id <APP_ID> --tenant-id <TENANT_ID>)",
            persona.bin_name()
        );
        cmd_login(cfg, LoginOptions::default(), false).await?;
        println!();
    }

    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    tui::run(cfg.clone(), auth, graph, storage).await
}

pub(super) async fn cmd_login(cfg: &mut AppConfig, opts: LoginOptions, json_out: bool) -> Result<()> {
    let mut config_changed = false;
    if let Some(ref cid) = opts.client_id {
        let trimmed = cid.trim();
        if !trimmed.is_empty() && cfg.client_id != trimmed {
            cfg.client_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if let Some(ref tid) = opts.tenant_id {
        let trimmed = tid.trim();
        if !trimmed.is_empty() && cfg.tenant_id != trimmed {
            cfg.tenant_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if config_changed {
        cfg.save()?;
    }

    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.login(&opts).await?;
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let me = graph.get_me().await?;
    auth.update_identity(me.id.clone(), me.display_name.clone(), me.email.clone())
        .await?;

    let updated = auth.current_tokens().await.unwrap_or(tokens);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "user": me,
                "client_id": updated.client_id,
                "tenant_id": updated.tenant_id,
                "auth_mode": updated.auth_mode,
                "expires_at": updated.expires_at,
            }))?
        );
    } else {
        println!(
            "Signed in as: {} ({}) [id={}]",
            me.display_name,
            me.email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "Auth mode: {} · tenant={} · expires: {}",
            updated.auth_mode,
            updated.tenant_id,
            format_unix_ts(updated.expires_at)
        );
    }
    Ok(())
}

pub(super) async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Signed out and removed stored outlook-cli tokens.");
    }
    Ok(())
}

pub(crate) fn auth_source_label(tokens: Option<&StoredTokens>, cfg: &AppConfig) -> &'static str {
    let Some(tok) = tokens else {
        return "not signed in";
    };
    let mode = tok.auth_mode.to_ascii_lowercase();
    if mode == "shared_teams_token_file" {
        return "teams-cli (shared token_file)";
    }
    if mode.contains("teams") || mode.contains("shared") {
        return "teams-cli (shared session)";
    }
    if mode == "env_token" {
        return "outlook-cli (OUTLOOK_ACCESS_TOKEN)";
    }
    if mode == "token_file" {
        if cfg.resolved_token_file().is_some() {
            return "outlook-cli (OUTLOOK_TOKEN_FILE)";
        }
        return "teams-cli (TEAMS_TOKEN_FILE)";
    }
    let outlook_cred = config::credential_fallback_path();
    if outlook_cred.exists() {
        return "outlook-cli (credentials.json)";
    }
    "outlook-cli (OS keyring)"
}

pub(super) async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let mut tokens = match auth.current_tokens().await {
        Some(t) => t,
        None => {
            if let Some(ref tf) = cfg.resolved_token_file() {
                if !tf.exists() {
                    bail!(
                        "Sign-in required. Run `{} login` or set `OUTLOOK_TOKEN_FILE` / `OUTLOOK_ACCESS_TOKEN`. (Configured OUTLOOK_TOKEN_FILE not found: {})",
                        persona.bin_name(),
                        tf.display()
                    );
                }
                bail!(
                    "Sign-in required. Run `{} login` or set a valid `OUTLOOK_TOKEN_FILE` ({}) / `OUTLOOK_ACCESS_TOKEN`.",
                    persona.bin_name(),
                    tf.display()
                );
            }
            bail!(
                "Sign-in required. Run `{} login` or set `OUTLOOK_TOKEN_FILE` / `OUTLOOK_ACCESS_TOKEN`.",
                persona.bin_name()
            );
        }
    };

    let refresh_skew_secs = (cfg.token_refresh_mins.max(1) * 60) as i64;
    let mut refresh_failure: Option<anyhow::Error> = None;
    if tokens.is_expired_soon(refresh_skew_secs) && auth.is_refreshable().await {
        match auth.refresh_now().await {
            Ok(refreshed) => {
                tokens = refreshed;
            }
            Err(refresh_err) => {
                if tokens.is_expired_soon(0) {
                    refresh_failure = Some(refresh_err);
                } else {
                    eprintln!(
                        "Warning: automatic token refresh failed ({refresh_err}); continuing with existing token (expires: {}).",
                        format_unix_ts(tokens.expires_at)
                    );
                }
            }
        }
    }

    let auth_source = auth_source_label(Some(&tokens), cfg);
    let scope_diag = diagnose_access_token_scopes(&tokens.access_token);

    if tokens.is_expired_soon(0) {
        let err_detail = match refresh_failure {
            Some(ref err) => format!(
                "Token expired (exp: {}, auth_mode={}, source={}) — automatic refresh failed: {err:#}",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode,
                auth_source
            ),
            None => format!(
                "Token expired (exp: {}, auth_mode={}, source={}) — refresh the token and retry.",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode,
                auth_source
            ),
        };
        if json_out {
            let details = json!({
                "persona": persona,
                "user_id": tokens.user_id,
                "display_name": tokens.user_display_name,
                "email": tokens.user_email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "auth_source": auth_source,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": true,
                "expiring_soon": true,
                "status": "expired",
                "scopes": scope_diag.scopes,
                "roles": scope_diag.roles,
                "scope_warnings": scope_diag.warnings,
            });
            return Err(error_with_info(
                "token_expired",
                format!(
                    "Token expired (exp: {}) — refresh the token and retry.",
                    format_unix_ts(tokens.expires_at)
                ),
                Some(details),
            ));
        }
        bail!("{err_detail}");
    }

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let mut profile_err: Option<anyhow::Error> = None;
    let live_profile = if tokens.user_id.is_none() || tokens.user_display_name.is_none() {
        match graph.get_me().await {
            Ok(p) => Some(p),
            Err(e) => {
                profile_err = Some(e);
                None
            }
        }
    } else {
        None
    };

    let user_id = tokens
        .user_id
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.id.clone()));
    let display_name = tokens
        .user_display_name
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.display_name.clone()));
    let email = tokens
        .user_email
        .clone()
        .or_else(|| live_profile.as_ref().and_then(|p| p.email.clone()));

    if display_name.is_none() && user_id.is_none() {
        if let Some(err) = profile_err {
            bail!(
                "Failed to fetch profile (/me) (auth_mode={}, source={}, expires: {}): {err:#}",
                tokens.auth_mode,
                auth_source,
                format_unix_ts(tokens.expires_at)
            );
        }
    }

    let expiring_soon = !tokens.is_expired_soon(0) && tokens.is_expired_soon(refresh_skew_secs);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona,
                "user_id": user_id,
                "display_name": display_name,
                "email": email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "auth_source": auth_source,
                "share_m365_auth": cfg.share_m365_auth,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": false,
                "expiring_soon": expiring_soon,
                "status": if expiring_soon { "expiring_soon" } else { "valid" },
                "scopes": scope_diag.scopes,
                "roles": scope_diag.roles,
                "scope_warnings": scope_diag.warnings,
            }))?
        );
    } else {
        println!("persona: {}", persona.bin_name());
        println!(
            "user: {} ({})",
            display_name.as_deref().unwrap_or("-"),
            email.as_deref().unwrap_or("-")
        );
        println!("user_id: {}", user_id.as_deref().unwrap_or("-"));
        println!("client_id: {}", tokens.client_id);
        println!("tenant_id: {}", tokens.tenant_id);
        println!("auth_mode: {}", tokens.auth_mode);
        println!("auth_source: {auth_source}");
        println!(
            "token_expires: {} ({})",
            format_unix_ts(tokens.expires_at),
            if expiring_soon {
                "expiring soon — refresh recommended"
            } else {
                "valid"
            }
        );
        if !scope_diag.scopes.is_empty() {
            println!("token_scopes (scp): {}", scope_diag.scopes.join(" "));
        } else if !scope_diag.has_scope_claims {
            println!("token_scopes (scp): (no JWT claims)");
        }
        if !scope_diag.roles.is_empty() {
            println!("token_roles: {}", scope_diag.roles.join(" "));
        }
        for warning in &scope_diag.warnings {
            println!("scope_warning: {warning}");
        }
    }
    Ok(())
}

/// Configured scopes the current access token does not carry, so `doctor` can tell existing users to
/// sign in again after the default list grows. Empty when the token exposes no `scp` claim.
pub(super) fn missing_configured_scopes(configured: &[String], token_scopes: &[String]) -> Vec<String> {
    if token_scopes.is_empty() {
        return Vec::new();
    }
    let granted = |wanted: &str| {
        token_scopes.iter().any(|have| {
            have.eq_ignore_ascii_case(wanted)
                // A read/write grant covers the read-only scope of the same resource.
                || (wanted.to_ascii_lowercase().ends_with(".read")
                    && have.eq_ignore_ascii_case(&format!("{wanted}Write")))
        })
    };
    configured
        .iter()
        .map(|scope| scope.trim())
        .filter(|scope| !scope.is_empty() && !scope.eq_ignore_ascii_case("offline_access"))
        .filter(|scope| !granted(scope))
        .map(str::to_string)
        .collect()
}

/// Which built-in trust roots clients use, for `doctor`.
pub(super) fn tls_roots_label(native_roots: bool) -> &'static str {
    let env_redirect = ["SSL_CERT_FILE", "SSL_CERT_DIR"]
        .iter()
        .any(|var| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()));
    match (native_roots, env_redirect) {
        (false, _) => "webpki (SSL_CERT_FILE unusable, OS store off)",
        // rustls-native-certs reads these instead of the OS store.
        (true, true) => "webpki+SSL_CERT_FILE/SSL_CERT_DIR",
        (true, false) => "webpki+os",
    }
}

pub(super) async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    online: bool,
    json_out: bool,
) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let token_diag = tokens
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();

    let cfg_path = config::config_file_path();
    let cred_path = config::credential_fallback_path();
    let drafts = config::drafts_path_for(user_id);
    let db = config::db_path(user_id);
    let dl_dir = cfg.resolved_download_dir();
    let configured_scopes = cfg.effective_scopes();
    let missing_scopes = missing_configured_scopes(&configured_scopes, &token_diag.scopes);
    let token_file = cfg.resolved_token_file();
    let token_refresh_cmd_configured = !cfg.token_refresh_cmd.trim().is_empty();
    let auth_mode = tokens.as_ref().map(|t| t.auth_mode.clone());
    let auth_source = auth_source_label(tokens.as_ref(), cfg);
    let token_expires_at = tokens.as_ref().map(|t| format_unix_ts(t.expires_at));
    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path, cfg);

    let (cached_folders, cached_messages, cached_events, cached_contacts, cached_todo_lists) =
        if db.exists() {
            if let Ok(storage) = Storage::open(&db) {
                (
                    storage.list_mail_folders().map(|v| v.len()).unwrap_or(0),
                    storage
                        .list_mail_messages(None, false, false, false, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage
                        .list_calendar_events(None, None, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage
                        .list_contacts(None, 10_000)
                        .map(|v| v.len())
                        .unwrap_or(0),
                    storage.list_todo_lists().map(|v| v.len()).unwrap_or(0),
                )
            } else {
                (0, 0, 0, 0, 0)
            }
        } else {
            (0, 0, 0, 0, 0)
        };

    let daemon_status = crate::daemon::query_daemon_status(cfg)
        .await
        .unwrap_or_default();
    let graphics_caps = img_preview::detect_graphics_caps(cfg.image_protocol);
    let extra_cas = crate::http::extra_cas();
    let tls_roots = tls_roots_label(extra_cas.native_roots());

    let mut probe_results: Option<serde_json::Value> = None;
    if online {
        let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
        if let Some(uid) = user_id {
            graph = graph.with_my_user_id(uid);
        }
        let (start_iso, end_iso) = default_calendar_window(None, None, 1)?;
        let (me_res, mail_res, cal_res, contacts_res, todo_res) = tokio::join!(
            graph.get_me(),
            graph.list_mail_folders(),
            graph.list_calendar_view(&start_iso, &end_iso, 5),
            graph.list_contacts(5),
            graph.list_todo_lists(),
        );

        let format_probe = |res: Result<usize>| -> serde_json::Value {
            match res {
                Ok(count) => json!({ "ok": true, "count": count }),
                Err(err) => json!({ "ok": false, "error": format!("{err:#}") }),
            }
        };

        let me_val = match me_res {
            Ok(me) => json!({ "ok": true, "user": me }),
            Err(err) => {
                let wrapped = wrap_graph_permission_error(&graph, err).await;
                json!({ "ok": false, "error": format!("{wrapped:#}") })
            }
        };
        let mail_val = format_probe(mail_res.map(|v| v.len()));
        let cal_val = format_probe(cal_res.map(|v| v.len()));
        let contacts_val = format_probe(contacts_res.map(|v| v.len()));
        let todo_val = format_probe(todo_res.map(|v| v.len()));
        let all_ok = me_val["ok"].as_bool().unwrap_or(false)
            && mail_val["ok"].as_bool().unwrap_or(false)
            && cal_val["ok"].as_bool().unwrap_or(false)
            && contacts_val["ok"].as_bool().unwrap_or(false)
            && todo_val["ok"].as_bool().unwrap_or(false);

        probe_results = Some(json!({
            "ok": all_ok,
            "me": me_val,
            "mail": mail_val,
            "calendar": cal_val,
            "contacts": contacts_val,
            "todo": todo_val,
        }));
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "persona": persona,
                "scopes": token_diag.scopes,
                "roles": token_diag.roles,
                "scope_warnings": token_diag.warnings,
                "configured_scopes": configured_scopes,
                "missing_scopes": missing_scopes,
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "drafts_path": drafts,
                "db_path": db,
                "db_exists": db.exists(),
                "download_dir": dl_dir,
                "github_repo": cfg.github_repo,
                "client_id": cfg.client_id,
                "tenant_id": cfg.tenant_id,
                "graph_base_url": cfg.graph_base_url,
                "auth_base_url": cfg.auth_base_url,
                "auth_source": auth_source,
                "share_m365_auth": cfg.share_m365_auth,
                "keyring_status": keyring_status,
                "token_file": token_file,
                "token_file_exists": token_file.as_ref().map(|p| p.exists()),
                "token_refresh_cmd_configured": token_refresh_cmd_configured,
                "token_refresh_mins": cfg.token_refresh_mins,
                "auth_mode": auth_mode,
                "token_expires_at": token_expires_at,
                "logged_in": tokens.is_some(),
                "cache_counts": {
                    "mail_folders": cached_folders,
                    "mail_messages": cached_messages,
                    "calendar_events": cached_events,
                    "contacts": cached_contacts,
                    "todo_lists": cached_todo_lists,
                },
                "daemon": {
                    "running": daemon_status.running,
                    "pid": daemon_status.pid,
                    "transport": daemon_status.transport,
                    "endpoint": daemon_status.endpoint,
                    "attached_clients": daemon_status.attached_clients,
                    "auto_daemon": cfg.auto_daemon,
                },
                "image_protocol": graphics_caps.protocol.as_str(),
                "image_protocol_preference": cfg.image_protocol.as_str(),
                "cell_px": [graphics_caps.cell_px.0, graphics_caps.cell_px.1],
                "tls": {
                    "roots": tls_roots,
                    "extra_ca": extra_cas.reports(),
                },
                "online_check": probe_results,
            }))?
        );
    } else {
        println!(
            "{} v{} (persona={})",
            persona.bin_name(),
            env!("CARGO_PKG_VERSION"),
            persona.bin_name()
        );
        println!(
            "config.toml: {} (exists={})",
            cfg_path.display(),
            cfg_path.exists()
        );
        println!(
            "credentials.json: {} (exists={})",
            cred_path.display(),
            cred_path.exists()
        );
        println!("drafts: {}", drafts.display());
        println!("cache.db: {} (exists={})", db.display(), db.exists());
        println!(
            "cache_counts: folders={cached_folders}, messages={cached_messages}, events={cached_events}, contacts={cached_contacts}, todo_lists={cached_todo_lists}"
        );
        println!("download_dir: {}", dl_dir.display());
        println!("tls_roots: {tls_roots}");
        if extra_cas.reports().is_empty() {
            println!("tls_extra_ca: (none; set OUTLOOK_EXTRA_CA_PEM to add a root CA)");
        }
        for report in extra_cas.reports() {
            match &report.error {
                None => println!(
                    "tls_extra_ca: {}={} (loaded, {} certs)",
                    report.source,
                    report.path.display(),
                    report.certs
                ),
                Some(error) => println!(
                    "tls_extra_ca: {}={} (loaded={}, {} certs): {error}",
                    report.source,
                    report.path.display(),
                    report.loaded,
                    report.certs
                ),
            }
        }
        println!("github_repo: {}", cfg.github_repo);
        println!(
            "configured_scopes ({}): {}",
            configured_scopes.len(),
            configured_scopes.join(", ")
        );
        if !token_diag.scopes.is_empty() {
            println!(
                "token_scopes ({}): {}",
                token_diag.scopes.len(),
                token_diag.scopes.join(" ")
            );
        } else if tokens.is_some() {
            println!("token_scopes: (no JWT scp claims)");
        }
        if !token_diag.roles.is_empty() {
            println!(
                "token_roles ({}): {}",
                token_diag.roles.len(),
                token_diag.roles.join(" ")
            );
        }
        for warning in &token_diag.warnings {
            println!("scope_warning: {warning}");
        }
        if !missing_scopes.is_empty() {
            println!(
                "missing_scopes ({}): {}",
                missing_scopes.len(),
                missing_scopes.join(" ")
            );
            println!(
                "hint: Run 'outlook login' to grant: {}",
                missing_scopes.join(" ")
            );
        }
        println!("client_id: {}", cfg.client_id);
        println!("tenant_id: {}", cfg.tenant_id);
        println!("graph_base_url: {}", cfg.graph_base_url);
        println!("auth_base_url: {}", cfg.auth_base_url);
        println!("auth_source: {auth_source}");
        println!("keyring_status: {keyring_status}");
        println!(
            "daemon: running={}, pid={}, endpoint={}, attached_clients={}, auto_daemon={}",
            daemon_status.running,
            daemon_status
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            daemon_status.endpoint.as_deref().unwrap_or("-"),
            daemon_status.attached_clients,
            cfg.auto_daemon,
        );
        println!(
            "token_file: {}",
            token_file
                .as_ref()
                .map(|p| format!("{} (exists={})", p.display(), p.exists()))
                .unwrap_or_else(|| "(unset)".to_string())
        );
        println!(
            "token_refresh_cmd_configured: {token_refresh_cmd_configured} (token_refresh_mins={})",
            cfg.token_refresh_mins
        );
        println!("auth_mode: {}", auth_mode.as_deref().unwrap_or("-"));
        println!(
            "token_expires_at: {}",
            token_expires_at.as_deref().unwrap_or("-")
        );
        let cell_suffix = if graphics_caps.cell_px_exact {
            ""
        } else {
            " est."
        };
        println!(
            "Image protocol: {} (preference: {}, cell: {}x{}px{})",
            graphics_caps.protocol.label(),
            cfg.image_protocol.as_str(),
            graphics_caps.cell_px.0,
            graphics_caps.cell_px.1,
            cell_suffix,
        );
        if let Some(ref online_val) = probe_results {
            println!(
                "online_check: ok={} (me={}, mail={}, calendar={}, contacts={}, todo={})",
                online_val["ok"].as_bool().unwrap_or(false),
                format_probe_status(&online_val["me"]),
                format_probe_status(&online_val["mail"]),
                format_probe_status(&online_val["calendar"]),
                format_probe_status(&online_val["contacts"]),
                format_probe_status(&online_val["todo"]),
            );
        }
    }

    Ok(())
}

pub(super) fn format_probe_status(val: &serde_json::Value) -> String {
    if val["ok"].as_bool().unwrap_or(false) {
        if let Some(cnt) = val.get("count").and_then(|v| v.as_u64()) {
            format!("ok({cnt})")
        } else if let Some(name) = val
            .get("user")
            .and_then(|u| u.get("display_name"))
            .and_then(|v| v.as_str())
        {
            format!("ok({name})")
        } else {
            "ok".to_string()
        }
    } else {
        val.get("error")
            .and_then(|e| e.as_str())
            .map(|e| format!("err({})", truncate_single_line(e, 48)))
            .unwrap_or_else(|| "err".to_string())
    }
}

pub(super) fn describe_keyring_status(
    tokens: Option<&StoredTokens>,
    fallback_path: &Path,
    cfg: &AppConfig,
) -> String {
    match tokens {
        None => "not signed in (no stored token)".to_string(),
        Some(tok) if tok.auth_mode == "env_token" => {
            "env_token (using OUTLOOK_ACCESS_TOKEN)".to_string()
        }
        Some(tok) if tok.auth_mode == "token_file" => {
            "token_file (using OUTLOOK_TOKEN_FILE)".to_string()
        }
        Some(_) if fallback_path.exists() => {
            "using outlook-cli credentials.json file fallback".to_string()
        }
        Some(_) if auth_source_label(tokens, cfg).starts_with("teams-cli") => {
            "shared teams-cli session active".to_string()
        }
        Some(_) => "OS keyring active (token loaded)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Mail commands
// ---------------------------------------------------------------------------
