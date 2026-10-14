#![allow(dead_code)] // Removed in Task 5, once the timelapse code is wired into main.
mod capture;
mod config;
mod ffmpeg;
mod metrics;
mod store;
mod timelapse;
mod web;

use std::{io::IsTerminal, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use tokio::{net::TcpListener, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{capture::Capturer, config::Config, metrics::Metrics, store::Store, web::AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let cfg = Arc::new(Config::parse());
    let store = Store::new(&cfg.data_dir);
    std::fs::create_dir_all(store.frames_dir())
        .with_context(|| format!("creating {}", store.frames_dir().display()))?;
    let removed = store
        .cleanup_parts()
        .context("removing stale .part files")?;
    info!(removed, "removed stale .part files");

    let metrics = Metrics::new();
    if let Some(frame) = store.newest_frame().context("finding newest frame")? {
        metrics.set_last_frame(frame.time.timestamp());
    }

    let capture = tokio::spawn(Capturer::new(cfg.clone(), store.clone(), metrics.clone()).run());

    let state = Arc::new(AppState {
        store,
        metrics,
        stale_after: cfg.stale_after(),
        started_at: Utc::now(),
    });
    let listener = TcpListener::bind(cfg.listen_addr)
        .await
        .with_context(|| format!("binding {}", cfg.listen_addr))?;
    info!(addr = %cfg.listen_addr, camera = %cfg.rtsp_url, "listening");

    let server =
        axum::serve(listener, web::router(state)).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        res = async { server.await } => res.context("http server failed")?,
        res = capture => bail!("capture task exited unexpectedly: {res:?}"),
    }
    info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("installing Ctrl+C handler");
    };
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
