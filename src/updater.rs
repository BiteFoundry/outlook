//! One-click GitHub Release self-updater (`teams update`) with SHA-256 verification
//! and safe Windows NTFS running-executable replacement.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::models::UpdateCheckResult;

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    #[serde(default)]
    url: String,
    #[serde(default)]
    browser_download_url: String,
}

/// Silently cleans up any `<current_exe>.old` left behind by a previous self-update.
pub fn cleanup_stale_old_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let old_path = append_ext_suffix(&exe, ".old");
        if old_path.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&old_path);
        }
    }
}

/// Checks GitHub Releases (`repo` = `"shinick-dev/teams"`) for a newer version and optionally replaces the current binary.
pub async fn check_or_apply_update(
    repo: &str,
    check_only: bool,
    force: bool,
) -> Result<UpdateCheckResult> {
    check_or_apply_update_with_api_base("https://api.github.com", repo, check_only, force).await
}

/// Testable entry point allowing an overridden GitHub API base URL (e.g. mock `axum` server in E2E tests).
pub async fn check_or_apply_update_with_api_base(
    api_base_url: &str,
    repo: &str,
    check_only: bool,
    force: bool,
) -> Result<UpdateCheckResult> {
    let binary_path =
        std::env::current_exe().context("Unable to determine current executable path")?;
    let token = discover_github_token(api_base_url).await;
    check_or_apply_update_at(api_base_url, repo, check_only, force, &binary_path, token).await
}

async fn check_or_apply_update_at(
    api_base_url: &str,
    repo: &str,
    check_only: bool,
    force: bool,
    binary_path: &Path,
    token: Option<String>,
) -> Result<UpdateCheckResult> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("Failed to create HTTP client")?;

    let ua = format!("teams-cli/{current_version}");
    let api_base = api_base_url.trim_end_matches('/');
    let release_api_url = format!("{api_base}/repos/{repo}/releases/latest");

    let mut req = client
        .get(&release_api_url)
        .header(USER_AGENT, &ua)
        .header(ACCEPT, "application/vnd.github+json");
    if let Some(tok) = token.as_deref() {
        req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("GitHub Release lookup request failed: {release_api_url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "GitHub Release lookup failed (HTTP {status}): {}. For private repositories, TEAMS_GITHUB_TOKEN / GH_TOKEN or `gh auth login` is required.",
            body.trim()
        );
    }

    let release: GitHubRelease = resp
        .json()
        .await
        .context("Failed to parse GitHub Release JSON")?;

    let latest_version = strip_tag_v(&release.tag_name).to_string();
    let current_semver = parse_semver(&current_version)
        .with_context(|| format!("Failed to parse current version: {current_version}"))?;
    let latest_semver = parse_semver(&latest_version)
        .with_context(|| format!("Failed to parse latest release tag: {}", release.tag_name))?;
    let update_available = latest_semver > current_semver;

    let selected_asset = select_platform_asset(&release.assets, &latest_version);
    let asset_name = selected_asset.map(|a| a.name.clone());
    let asset_size = selected_asset.map(|a| a.size);

    if check_only || (!update_available && !force) {
        return Ok(UpdateCheckResult {
            current_version,
            latest_version,
            update_available,
            release_url: release.html_url,
            asset_name,
            asset_size,
            updated: false,
            binary_path: binary_path.to_path_buf(),
        });
    }

    let Some(asset) = selected_asset else {
        bail!(
            "No release asset found for current platform ({}/{}) in release v{latest_version}.",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };

    let asset_bytes = download_asset_bytes(&client, asset, &ua, token.as_deref()).await?;

    if let Some(sums_asset) = release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS.txt"))
    {
        let sums_bytes = download_asset_bytes(&client, sums_asset, &ua, token.as_deref()).await?;
        let sums_text = String::from_utf8(sums_bytes)
            .context("SHA256SUMS.txt is not valid UTF-8 text")?;
        verify_sha256_sums(&sums_text, &asset.name, &asset_bytes)?;
    }

    let extracted_bytes = extract_binary_from_asset(&asset.name, &asset_bytes)?;
    replace_executable_at(binary_path, &extracted_bytes)?;

    Ok(UpdateCheckResult {
        current_version,
        latest_version,
        update_available,
        release_url: release.html_url,
        asset_name: Some(asset.name.clone()),
        asset_size: Some(asset.size),
        updated: true,
        binary_path: binary_path.to_path_buf(),
    })
}

async fn discover_github_token(api_base_url: &str) -> Option<String> {
    for var in ["TEAMS_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    if api_base_url.trim_end_matches('/') == "https://api.github.com" {
        let fut = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output();
        if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(3), fut).await {
            if output.status.success() {
                let tok = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !tok.is_empty() {
                    return Some(tok);
                }
            }
        }
    }

    None
}

async fn download_asset_bytes(
    client: &reqwest::Client,
    asset: &GitHubAsset,
    ua: &str,
    token: Option<&str>,
) -> Result<Vec<u8>> {
    if let Some(tok) = token {
        if !asset.url.trim().is_empty() {
            let resp = client
                .get(&asset.url)
                .header(USER_AGENT, ua)
                .header(ACCEPT, "application/octet-stream")
                .header(AUTHORIZATION, format!("Bearer {tok}"))
                .send()
                .await;
            if let Ok(r) = resp {
                if r.status().is_success() {
                    return r
                        .bytes()
                        .await
                        .map(|b| b.to_vec())
                        .with_context(|| format!("Failed to download asset body: {}", asset.name));
                }
            }
        }
    }

    if !asset.browser_download_url.trim().is_empty() {
        let mut req = client
            .get(&asset.browser_download_url)
            .header(USER_AGENT, ua);
        if let Some(tok) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("Asset download request failed: {}", asset.name))?;
        if resp.status().is_success() {
            return resp
                .bytes()
                .await
                .map(|b| b.to_vec())
                .with_context(|| format!("Failed to download asset body: {}", asset.name));
        }
        let status = resp.status();
        bail!("Failed to download asset '{}' (HTTP {status})", asset.name);
    }

    if !asset.url.trim().is_empty() {
        let mut req = client
            .get(&asset.url)
            .header(USER_AGENT, ua)
            .header(ACCEPT, "application/octet-stream");
        if let Some(tok) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {tok}"));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("Asset API download request failed: {}", asset.name))?;
        if resp.status().is_success() {
            return resp
                .bytes()
                .await
                .map(|b| b.to_vec())
                .with_context(|| format!("Failed to download asset body: {}", asset.name));
        }
        let status = resp.status();
        bail!("Failed to download asset '{}' via API (HTTP {status})", asset.name);
    }

    bail!("Asset '{}' has no download URL", asset.name);
}

pub(crate) fn parse_semver(version: &str) -> Result<(u64, u64, u64)> {
    let stripped = strip_tag_v(version.trim());
    let core = stripped
        .split(['-', '+'])
        .next()
        .unwrap_or(stripped)
        .trim();
    let mut parts = core.split('.');
    let major: u64 = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing major version: {version}"))?
        .parse()
        .with_context(|| format!("Invalid major version: {version}"))?;
    let minor: u64 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .with_context(|| format!("Invalid minor version: {version}"))?;
    let patch: u64 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .with_context(|| format!("Invalid patch version: {version}"))?;
    if parts.next().is_some() {
        bail!("Invalid semver format: {version}");
    }
    Ok((major, minor, patch))
}

fn strip_tag_v(tag: &str) -> &str {
    tag.trim().trim_start_matches(['v', 'V'])
}

fn select_platform_asset<'a>(assets: &'a [GitHubAsset], latest: &str) -> Option<&'a GitHubAsset> {
    select_platform_asset_for(assets, latest, std::env::consts::OS, std::env::consts::ARCH)
}

fn select_platform_asset_for<'a>(
    assets: &'a [GitHubAsset],
    latest: &str,
    os: &str,
    arch: &str,
) -> Option<&'a GitHubAsset> {
    let candidates: Vec<String> = match (os, arch) {
        ("windows", "x86_64") => vec![
            "teams.exe".to_string(),
            format!("teams-{latest}-x86_64-pc-windows-msvc.zip"),
            "teams-x86_64-pc-windows-msvc.zip".to_string(),
        ],
        ("windows", "aarch64") => vec![
            format!("teams-{latest}-aarch64-pc-windows-msvc.zip"),
            "teams-aarch64-pc-windows-msvc.zip".to_string(),
            "teams.exe".to_string(),
        ],
        ("macos", "x86_64") => vec![
            format!("teams-{latest}-x86_64-apple-darwin.tar.gz"),
            "teams-x86_64-apple-darwin.tar.gz".to_string(),
            "teams".to_string(),
        ],
        ("macos", "aarch64") => vec![
            format!("teams-{latest}-aarch64-apple-darwin.tar.gz"),
            "teams-aarch64-apple-darwin.tar.gz".to_string(),
            "teams".to_string(),
        ],
        ("linux", "x86_64") => vec![
            format!("teams-{latest}-x86_64-unknown-linux-gnu.tar.gz"),
            format!("teams-{latest}-x86_64-unknown-linux-musl.tar.gz"),
            "teams-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            "teams".to_string(),
        ],
        ("linux", "aarch64") => vec![
            format!("teams-{latest}-aarch64-unknown-linux-gnu.tar.gz"),
            format!("teams-{latest}-aarch64-unknown-linux-musl.tar.gz"),
            "teams-aarch64-unknown-linux-gnu.tar.gz".to_string(),
            "teams".to_string(),
        ],
        _ => vec![if os == "windows" {
            "teams.exe".to_string()
        } else {
            "teams".to_string()
        }],
    };

    for candidate in &candidates {
        if let Some(found) = assets.iter().find(|a| a.name == *candidate) {
            return Some(found);
        }
    }
    for candidate in &candidates {
        if let Some(found) = assets.iter().find(|a| a.name.eq_ignore_ascii_case(candidate)) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn verify_sha256_sums(
    sums_text: &str,
    asset_name: &str,
    asset_bytes: &[u8],
) -> Result<()> {
    let mut expected_hex: Option<String> = None;
    for raw_line in sums_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(hash) = parts.next().map(str::trim) else {
            continue;
        };
        let Some(file_part) = parts.next().map(str::trim) else {
            continue;
        };
        let clean_name = file_part.trim_start_matches(['*', '.', '/']);
        if clean_name == asset_name
            || Path::new(clean_name)
                .file_name()
                .and_then(|n| n.to_str())
                == Some(asset_name)
        {
            expected_hex = Some(hash.to_ascii_lowercase());
            break;
        }
    }

    let Some(expected) = expected_hex else {
        bail!("Checksum entry for '{asset_name}' not found in SHA256SUMS.txt");
    };

    let actual = hex::encode(Sha256::digest(asset_bytes));
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!(
            "SHA-256 verification failed ({asset_name}): expected {expected}, actual {actual}"
        );
    }
    Ok(())
}

fn extract_binary_from_asset(asset_name: &str, asset_bytes: &[u8]) -> Result<Vec<u8>> {
    let lower = asset_name.to_ascii_lowercase();
    if !lower.ends_with(".tar.gz") && !lower.ends_with(".tgz") && !lower.ends_with(".zip") {
        return Ok(asset_bytes.to_vec());
    }

    let tmp = tempfile::tempdir().context("Failed to create temporary directory for update extraction")?;
    let archive_path = tmp.path().join(asset_name);
    std::fs::write(&archive_path, asset_bytes)
        .with_context(|| format!("Failed to save temporary archive: {}", archive_path.display()))?;

    let extract_dir = tmp.path().join("extracted");
    std::fs::create_dir_all(&extract_dir)?;

    let status = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&extract_dir)
            .status()
            .context("Failed to run `tar -xzf`")?
    } else {
        std::process::Command::new("tar")
            .arg("-xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&extract_dir)
            .status()
            .context("Failed to run `tar -xf`")?
    };

    if !status.success() {
        bail!("Failed to extract archive ({asset_name}): exit code {status}");
    }

    let target_bin = if cfg!(windows) { "teams.exe" } else { "teams" };
    let found = find_file_recursive(&extract_dir, target_bin).ok_or_else(|| {
        anyhow::anyhow!("Executable '{target_bin}' not found in extracted archive ({asset_name})")
    })?;
    std::fs::read(&found)
        .with_context(|| format!("Failed to read extracted binary: {}", found.display()))
}

fn find_file_recursive(dir: &Path, target_name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, target_name) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case(target_name))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

pub(crate) fn replace_executable_at(target_exe: &Path, new_bytes: &[u8]) -> Result<()> {
    let new_path = append_ext_suffix(target_exe, ".new");
    let old_path = append_ext_suffix(target_exe, ".old");

    std::fs::write(&new_path, new_bytes)
        .with_context(|| format!("Failed to write temporary new binary: {}", new_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&new_path, perms)
            .with_context(|| format!("Failed to set executable permissions: {}", new_path.display()))?;
    }

    if old_path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&old_path);
    }

    let had_existing = target_exe.symlink_metadata().is_ok();
    if had_existing {
        if let Err(err) = std::fs::rename(target_exe, &old_path) {
            let _ = std::fs::remove_file(&new_path);
            return Err(err).with_context(|| {
                format!(
                    "Failed to move existing executable to backup: {} -> {}",
                    target_exe.display(),
                    old_path.display()
                )
            });
        }
    }

    if let Err(err) = std::fs::rename(&new_path, target_exe) {
        if had_existing && old_path.symlink_metadata().is_ok() {
            let _ = std::fs::rename(&old_path, target_exe);
        }
        let _ = std::fs::remove_file(&new_path);
        return Err(err).with_context(|| {
            format!(
                "Failed to replace with new executable: {} -> {}",
                new_path.display(),
                target_exe.display()
            )
        });
    }

    // On Unix, unlinking the old binary immediately is safe because running
    // processes keep their open inode valid, keeping the install dir clean.
    #[cfg(unix)]
    if had_existing {
        let _ = std::fs::remove_file(&old_path);
    }

    Ok(())
}

fn append_ext_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{header, HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
        Json, Router,
    };
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[test]
    fn test_semver_parsing_and_comparison() {
        assert_eq!(parse_semver("0.2.0").unwrap(), (0, 2, 0));
        assert_eq!(parse_semver("v1.10.3").unwrap(), (1, 10, 3));
        assert_eq!(parse_semver("V2.0.1-rc1+build4").unwrap(), (2, 0, 1));
        assert!(parse_semver("v0.3.0").unwrap() > parse_semver("0.2.9").unwrap());
        assert!(parse_semver("0.2.0").unwrap() == parse_semver("v0.2.0").unwrap());
        assert!(parse_semver("bad").is_err());
    }

    #[test]
    fn test_sha256_verification_success_and_mismatch() {
        let payload = b"teams-binary-content-v0.9.0";
        let digest = hex::encode(Sha256::digest(payload));
        let sums = format!("{digest}  teams.exe\n0000000000000000000000000000000000000000000000000000000000000000  other.tar.gz\n");
        verify_sha256_sums(&sums, "teams.exe", payload).unwrap();

        let err = verify_sha256_sums(&sums, "teams.exe", b"tampered-bytes").unwrap_err();
        assert!(err.to_string().contains("SHA-256 verification failed"));

        let missing = verify_sha256_sums(&sums, "missing.zip", payload).unwrap_err();
        assert!(missing.to_string().contains("not found in SHA256SUMS.txt"));
    }

    #[test]
    fn test_platform_asset_selection() {
        let assets = vec![
            GitHubAsset {
                name: "teams-9.9.9-x86_64-pc-windows-msvc.zip".into(),
                size: 100,
                url: String::new(),
                browser_download_url: String::new(),
            },
            GitHubAsset {
                name: "teams.exe".into(),
                size: 200,
                url: String::new(),
                browser_download_url: String::new(),
            },
            GitHubAsset {
                name: "teams-9.9.9-aarch64-apple-darwin.tar.gz".into(),
                size: 300,
                url: String::new(),
                browser_download_url: String::new(),
            },
        ];
        assert_eq!(
            select_platform_asset_for(&assets, "9.9.9", "windows", "x86_64")
                .unwrap()
                .name,
            "teams.exe"
        );
        assert_eq!(
            select_platform_asset_for(&assets, "9.9.9", "macos", "aarch64")
                .unwrap()
                .name,
            "teams-9.9.9-aarch64-apple-darwin.tar.gz"
        );
    }

    #[derive(Clone)]
    struct MockReleaseState {
        base_url: String,
        tag_name: String,
        asset_name: String,
        asset_bytes: Arc<Vec<u8>>,
        sums_text: String,
        required_token: Option<String>,
    }

    async fn mock_latest_release(
        State(state): State<MockReleaseState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        if let Some(expected_tok) = &state.required_token {
            let expected_hdr = format!("Bearer {expected_tok}");
            let got = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok());
            if got != Some(expected_hdr.as_str()) {
                return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
        }
        let body = serde_json::json!({
            "tag_name": state.tag_name,
            "html_url": format!("{}/shinick-dev/teams/releases/tag/{}", state.base_url, state.tag_name),
            "assets": [
                {
                    "name": state.asset_name,
                    "size": state.asset_bytes.len() as u64,
                    "url": format!("{}/assets/binary", state.base_url),
                    "browser_download_url": format!("{}/download/{}", state.base_url, state.asset_name)
                },
                {
                    "name": "SHA256SUMS.txt",
                    "size": state.sums_text.len() as u64,
                    "url": format!("{}/assets/sums", state.base_url),
                    "browser_download_url": format!("{}/download/SHA256SUMS.txt", state.base_url)
                }
            ]
        });
        Json(body).into_response()
    }

    async fn mock_binary_api(
        State(state): State<MockReleaseState>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        if headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
            != Some("application/octet-stream")
        {
            return (StatusCode::BAD_REQUEST, "missing octet-stream accept").into_response();
        }
        (StatusCode::OK, (*state.asset_bytes).clone()).into_response()
    }

    async fn mock_sums_api(State(state): State<MockReleaseState>) -> impl IntoResponse {
        (StatusCode::OK, state.sums_text.clone()).into_response()
    }

    async fn mock_binary_browser(State(state): State<MockReleaseState>) -> impl IntoResponse {
        (StatusCode::OK, (*state.asset_bytes).clone()).into_response()
    }

    #[tokio::test]
    async fn test_mock_github_release_check_and_atomic_old_replacement() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let new_binary = b"NEW-TEAMS-BINARY-V9.9.9".to_vec();
        let asset_name = if cfg!(windows) { "teams.exe" } else { "teams" }.to_string();
        let digest = hex::encode(Sha256::digest(&new_binary));
        let sums_text = format!("{digest}  {asset_name}\n");

        let state = MockReleaseState {
            base_url: base_url.clone(),
            tag_name: "v9.9.9".to_string(),
            asset_name: asset_name.clone(),
            asset_bytes: Arc::new(new_binary.clone()),
            sums_text,
            required_token: Some("secret-gh-token".to_string()),
        };

        let app = Router::new()
            .route("/repos/shinick-dev/teams/releases/latest", get(mock_latest_release))
            .route("/assets/binary", get(mock_binary_api))
            .route("/assets/sums", get(mock_sums_api))
            .route("/download/SHA256SUMS.txt", get(mock_sums_api))
            .route(&format!("/download/{asset_name}"), get(mock_binary_browser))
            .with_state(state);

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let fake_exe = tmp.path().join(&asset_name);
        std::fs::write(&fake_exe, b"OLD-TEAMS-BINARY-V0.2.0").unwrap();

        // 1. check_only = true leaves binary untouched
        let check_res = check_or_apply_update_at(
            &base_url,
            "shinick-dev/teams",
            true,
            false,
            &fake_exe,
            Some("secret-gh-token".to_string()),
        )
        .await
        .unwrap();
        assert!(check_res.update_available);
        assert!(!check_res.updated);
        assert_eq!(check_res.latest_version, "9.9.9");
        assert_eq!(std::fs::read(&fake_exe).unwrap(), b"OLD-TEAMS-BINARY-V0.2.0");

        // 2. check_only = false downloads, verifies SHA-256, and atomically moves old -> .old
        let apply_res = check_or_apply_update_at(
            &base_url,
            "shinick-dev/teams",
            false,
            false,
            &fake_exe,
            Some("secret-gh-token".to_string()),
        )
        .await
        .unwrap();
        assert!(apply_res.updated);
        assert_eq!(apply_res.asset_name.as_deref(), Some(asset_name.as_str()));
        assert_eq!(std::fs::read(&fake_exe).unwrap(), new_binary);
        let old_exe = append_ext_suffix(&fake_exe, ".old");
        if cfg!(unix) {
            assert!(!old_exe.exists());
        } else {
            assert_eq!(std::fs::read(&old_exe).unwrap(), b"OLD-TEAMS-BINARY-V0.2.0");
        }
        assert!(!append_ext_suffix(&fake_exe, ".new").exists());

        server.abort();
    }

    #[tokio::test]
    async fn test_mock_github_release_checksum_mismatch_aborts_before_replace() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let asset_name = if cfg!(windows) { "teams.exe" } else { "teams" }.to_string();
        let state = MockReleaseState {
            base_url: base_url.clone(),
            tag_name: "v9.9.9".to_string(),
            asset_name: asset_name.clone(),
            asset_bytes: Arc::new(b"CORRUPTED-BYTES".to_vec()),
            sums_text: format!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  {asset_name}\n"
            ),
            required_token: None,
        };

        let app = Router::new()
            .route("/repos/shinick-dev/teams/releases/latest", get(mock_latest_release))
            .route("/assets/binary", get(mock_binary_api))
            .route("/assets/sums", get(mock_sums_api))
            .route("/download/SHA256SUMS.txt", get(mock_sums_api))
            .route(&format!("/download/{asset_name}"), get(mock_binary_browser))
            .with_state(state);

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let fake_exe = tmp.path().join(&asset_name);
        std::fs::write(&fake_exe, b"ORIGINAL-SAFE-BINARY").unwrap();

        let err = check_or_apply_update_at(
            &base_url,
            "shinick-dev/teams",
            false,
            false,
            &fake_exe,
            None,
        )
        .await
        .unwrap_err();
        let err_chain = format!("{err:#}");
        assert!(err_chain.contains("SHA-256"), "unexpected error: {err_chain}");
        assert_eq!(std::fs::read(&fake_exe).unwrap(), b"ORIGINAL-SAFE-BINARY");
        assert!(!append_ext_suffix(&fake_exe, ".old").exists());

        server.abort();
    }
}
