use std::ffi::OsStr;

use anyhow::Result;

fn needs_multi_thread_runtime<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iter = args.into_iter();
    let _bin = iter.next();
    match iter.next() {
        None => true,
        Some(arg) => match arg.as_ref().to_str() {
            Some("tui" | "serve" | "api" | "mcp" | "mcp-server") => true,
            Some("daemon") => matches!(
                iter.next().as_ref().and_then(|s| s.as_ref().to_str()),
                Some("run")
            ),
            _ => false,
        },
    }
}

fn main() -> Result<()> {
    // Windows debug binaries default to a 1 MiB main-thread stack, which clap's
    // unoptimized Subcommand derive builder and deep async futures can exceed.
    let handle = std::thread::Builder::new()
        .name("outlook-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_main)?;
    match handle.join() {
        Ok(res) => res,
        Err(panic_payload) => std::panic::resume_unwind(panic_payload),
    }
}

fn run_main() -> Result<()> {
    if let Some(res) = outlook_cli::cli::try_fast_help_or_version() {
        return res;
    }

    if needs_multi_thread_runtime(std::env::args_os()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let _ = rustls::crypto::ring::default_provider().install_default();
            tokio::task::spawn_blocking(outlook_cli::updater::cleanup_stale_old_binary);
            outlook_cli::cli::run_from_env().await
        })
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        outlook_cli::updater::cleanup_stale_old_binary();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(outlook_cli::cli::run_from_env())
    }
}

#[cfg(test)]
mod tests {
    use super::needs_multi_thread_runtime;

    #[test]
    fn detects_multi_thread_commands_only_for_tui_serve_mcp_and_daemon_run() {
        assert!(needs_multi_thread_runtime(["outlook"]));
        assert!(needs_multi_thread_runtime(["outlook", "tui"]));
        assert!(needs_multi_thread_runtime(["outlook", "serve"]));
        assert!(needs_multi_thread_runtime(["outlook", "api"]));
        assert!(needs_multi_thread_runtime(["outlook", "mcp"]));
        assert!(needs_multi_thread_runtime(["outlook", "mcp-server"]));
        assert!(needs_multi_thread_runtime(["outlook", "daemon", "run"]));

        for sub in ["status", "stop", "start", "restart"] {
            assert!(
                !needs_multi_thread_runtime(["outlook", "daemon", sub]),
                "daemon {sub}"
            );
        }

        for cmd in [
            "folders",
            "mail",
            "read",
            "send",
            "reply",
            "forward",
            "mark",
            "move",
            "archive",
            "delete",
            "search",
            "download",
            "preview",
            "preview-file",
            "calendar",
            "event",
            "create-event",
            "rsvp",
            "delete-event",
            "tasks",
            "task-lists",
            "add-task",
            "done",
            "delete-task",
            "contacts",
            "sync",
            "whoami",
            "doctor",
            "--help",
            "-h",
            "--version",
            "-V",
        ] {
            assert!(!needs_multi_thread_runtime(["outlook", cmd]), "{cmd}");
        }
    }
}
