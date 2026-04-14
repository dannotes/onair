#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

use anyhow::Result;
use onair::config::Db;
use onair::state::{
    bulb_poll_loop, force_off, monitor_loop, resolve_bulb, AppState, Config, EventLevel,
};
use onair::{bulb, platform};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Tiny CLI: just --version and --help. Anything else falls through to the daemon.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("onair {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "onair {} — turns a Philips WiZ bulb red while you're on a Teams call",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        println!("Usage: onair [OPTIONS]");
        println!();
        println!("Options:");
        println!("  -V, --version    print version and exit");
        println!("  -h, --help       print this help and exit");
        println!();
        println!("Once running, open http://localhost:9876 to configure.");
        return Ok(());
    }

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,onair=debug")),
        )
        .with_target(false)
        .init();

    tracing::info!("Onair v{} starting...", env!("CARGO_PKG_VERSION"));

    // ----- DB + config load -----
    let db_path = platform::default_db_path();
    let db: Option<Arc<Db>> = match Db::open(&db_path) {
        Ok(db) => {
            db.prune_old();
            Some(Arc::new(db))
        }
        Err(e) => {
            tracing::error!("could not open sqlite db at {}: {}", db_path.display(), e);
            tracing::warn!("running without persistence — config changes will not survive restart");
            None
        }
    };

    let config = match &db {
        Some(db) => match db.load_config() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("load_config failed ({}), falling back to defaults", e);
                Config::defaults()
            }
        },
        None => Config::defaults(),
    };

    // ----- AppState -----
    let state = Arc::new(AppState::new(config));
    if let Some(db) = db.as_ref() {
        *state.db.write() = Some(db.clone());
    }
    state.log_event(
        EventLevel::Inf,
        format!("Onair v{} starting", env!("CARGO_PKG_VERSION")),
    );
    if let Some(db) = db.as_ref() {
        state.log_event(EventLevel::Inf, format!("db: {}", db.path.display()));
    }

    // ----- First-run convenience: auto-pick the only bulb on the LAN -----
    let needs_first_run_pick = state.config.read().bulb_mac.is_empty();
    if needs_first_run_pick {
        state.log_event(
            EventLevel::Inf,
            "no bulb configured — scanning network for first-run setup",
        );
        match bulb::discover(Duration::from_secs(3)).await {
            Ok(bulbs) => {
                *state.last_discovery.write() = bulbs.clone();
                if bulbs.len() == 1 {
                    let only = &bulbs[0];
                    state.config.write().bulb_mac = only.mac.clone();
                    state.persist_config();
                    state.log_event(
                        EventLevel::Ok,
                        format!(
                            "auto-selected only bulb on LAN: {} at {}",
                            only.mac, only.ip
                        ),
                    );
                } else if bulbs.is_empty() {
                    state.log_event(
                        EventLevel::Wrn,
                        "no bulbs responded to broadcast discovery — this is normal if your bulb is already paired with the Philips WiZ app, Alexa, or Google Home. Open http://localhost:9876 → Settings → Bulb Selection and enter the bulb's IP directly.",
                    );
                } else {
                    state.log_event(
                        EventLevel::Inf,
                        format!(
                            "{} bulbs found — open http://localhost:9876 to pick one",
                            bulbs.len()
                        ),
                    );
                }
            }
            Err(e) => {
                state.log_event(
                    EventLevel::Wrn,
                    format!("first-run discovery failed: {}", e),
                );
            }
        }
    }

    // Resolve the configured bulb (no-op if still empty).
    resolve_bulb(state.clone()).await;

    // ----- First-run convenience: auto-open dashboard in the user's browser -----
    // Only when invoked from an interactive terminal (so it never fires under
    // launchd / systemd / Windows Startup-folder shortcut, where there's no TTY
    // and possibly no logged-in browser session). Once-only — gated on
    // `first_run_completed` so subsequent manual `onair` runs don't re-open.
    let interactive = std::io::stdout().is_terminal();
    let first_run_pending = !state.config.read().first_run_completed;
    if first_run_pending && interactive {
        let port = state.config.read().ui_port;
        let url = format!("http://127.0.0.1:{port}");
        let s2 = state.clone();
        tokio::spawn(async move {
            // Brief delay so the web server is bound and listening before the
            // browser tries to connect.
            tokio::time::sleep(Duration::from_millis(500)).await;
            s2.log_event(
                EventLevel::Inf,
                format!("opening dashboard in your browser: {url}"),
            );
            if let Err(e) = platform::open_url(&url) {
                tracing::warn!("could not open browser: {} (visit {} manually)", e, &url);
            }
            s2.config.write().first_run_completed = true;
            s2.persist_config();
        });
    }

    // ----- Background tasks -----
    let monitor = tokio::spawn(monitor_loop(state.clone()));
    let bulb_poll = tokio::spawn(bulb_poll_loop(state.clone()));
    let web = tokio::spawn(onair::web::serve(state.clone()));

    let shutdown_state = state.clone();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl+C received, shutting down");
            shutdown_state.log_event(EventLevel::Inf, "shutting down (ctrl+c) — turning bulb off");
            force_off(&shutdown_state).await;
        }
        res = monitor => {
            tracing::warn!("monitor loop exited: {:?}", res);
        }
        res = bulb_poll => {
            tracing::warn!("bulb poll loop exited: {:?}", res);
        }
        res = web => {
            tracing::warn!("web server exited: {:?}", res);
        }
    }

    Ok(())
}
