//! Process lifecycle: database, ffmpeg detection, scanner, HTTP server,
//! background tasks and signals.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::{self, AppState, Parts, ScanRequest};
use crate::auth::LOGIN_TTL;
use crate::auth::oidc::Provider;
use crate::config::Config;
use crate::media::scanner::{ScanOptions, Scanner};
use crate::metrics::Metrics;
use crate::store::{Store, now_ms};
use crate::transcode::Transcoder;

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("Ferrocine/{}", crate::VERSION))
        .pool_max_idle_per_host(4)
        .build()?)
}

pub async fn connect_with_retry(config: &Config) -> anyhow::Result<Store> {
    let mut delay = Duration::from_secs(1);
    let mut attempt = 0;
    loop {
        match Store::connect(&config.database).await {
            Ok(s) => return Ok(s),
            Err(e) if attempt < 30 => {
                tracing::warn!(error = %format!("{e:#}"), retry_in = ?delay, "database unavailable");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn run(config_path: PathBuf, config: Config) -> anyhow::Result<()> {
    let config = Arc::new(config);
    let metrics = Metrics::new();
    let store = connect_with_retry(&config).await?;
    store.migrate().await?;
    store.sync_libraries(&config.libraries).await?;
    let indexed = store.backfill_search_text().await?;
    if indexed > 0 {
        tracing::info!(items = indexed, "search text filled");
    }
    tracing::info!(dialect = ?store.dialect(), libraries = config.libraries.len(), "database ready");

    let shutdown = CancellationToken::new();
    spawn_signal_handlers(shutdown.clone());
    let http = http_client()?;

    let ffmpeg = crate::transcode::hw::detect(&config.transcoding).await;
    let transcoder = Arc::new(Transcoder::new(config.transcoding.clone(), ffmpeg, metrics.clone()));
    transcoder.clean_temp().await?;
    let reaper = tokio::spawn(transcoder.clone().reaper(shutdown.clone()));

    let oidc = config.auth.oidc.clone().map(|o| {
        let p = Arc::new(Provider::new(o, &config.server.public_url, http.clone()));
        tokio::spawn(p.clone().refresh_loop(shutdown.clone()));
        tracing::info!(issuer = %p.config.issuer, redirect_uri = %p.redirect_uri, "OpenID Connect sign-in");
        p
    });

    let (scan_tx, scan_rx) = mpsc::channel::<ScanRequest>(32);
    let providers = Arc::new(crate::metadata::ProviderState::default());
    let state = Arc::new(AppState::new(Parts {
        config_path,
        config: config.clone(),
        store: store.clone(),
        oidc,
        http: http.clone(),
        providers: providers.clone(),
        shutdown: shutdown.clone(),
        scanner: Scanner::new(store.clone(), http.clone(), providers),
        scan_tx: scan_tx.clone(),
        transcoder: transcoder.clone(),
        metrics: metrics.clone(),
    }));
    spawn_sighup(state.clone());

    let scans = tokio::spawn(scan_loop(state.clone(), scan_rx, shutdown.clone()));
    tokio::spawn(housekeeping(state.clone(), shutdown.clone()));
    if config.scanner.on_start {
        for l in &config.libraries {
            let _ = scan_tx.try_send(ScanRequest {
                library: l.id.clone(),
                opts: ScanOptions::default(),
            });
        }
    }
    tokio::spawn(periodic_scans(state.clone()));

    let listener = tokio::net::TcpListener::bind(config.server.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.server.listen))?;
    tracing::info!(listen = %config.server.listen, public_url = %config.server.public_url, "HTTP server ready");
    let router = api::router(state.clone());
    let stop = shutdown.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
        .context("HTTP server failed")?;
    shutdown.cancel();
    let _ = reaper.await;
    // A scan in progress is abandoned; the next one resumes incrementally.
    scans.abort();
    tracing::info!("stopped");
    Ok(())
}

/// Run requested scans one at a time, ignoring duplicates of a scan already queued.
async fn scan_loop(state: Arc<AppState>, mut rx: mpsc::Receiver<ScanRequest>, shutdown: CancellationToken) {
    loop {
        let req = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                None => return,
            },
        };
        // Coalesce what queued up meanwhile: one scan per library, with
        // `refresh_metadata` if any request asked for it.
        let mut batch = vec![req];
        while let Ok(r) = rx.try_recv() {
            batch.push(r);
        }
        let mut merged: Vec<ScanRequest> = Vec::new();
        for r in batch {
            match merged.iter_mut().find(|m| m.library == r.library) {
                Some(m) => m.opts.refresh_metadata |= r.opts.refresh_metadata,
                None => merged.push(r),
            }
        }
        for r in merged {
            let config = state.config();
            let Some(lib) = state.library(&r.library) else {
                continue;
            };
            let result = state.scanner.scan(&config, &lib, r.opts).await;
            match &result {
                Ok(Some(_)) => state.metrics.scan(&lib.id, true),
                Ok(None) => {}
                Err(_) => state.metrics.scan(&lib.id, false),
            }
            if let Ok(counts) = state.store.item_counts().await {
                for l in &config.libraries {
                    state.metrics.set_items(&l.id, counts.get(&l.id).copied().unwrap_or(0));
                }
            }
            if shutdown.is_cancelled() {
                return;
            }
        }
    }
}

/// Purge expired sessions and sweep caches.
async fn housekeeping(state: Arc<AppState>, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(300)) => {}
        }
        state.sessions.sweep();
        let idle = state.config().auth.idle().as_millis_i64();
        match state
            .store
            .purge_sessions(now_ms(), idle, LOGIN_TTL.as_millis() as i64)
            .await
        {
            Ok(n) if n > 0 => tracing::info!(sessions = n, "expired sessions removed"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "cannot purge sessions"),
        }
    }
}

/// Scan every library each `scanner.interval`, as configured at the time.
async fn periodic_scans(state: Arc<AppState>) {
    let mut last = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        let config = state.config();
        let interval = config.scanner.interval.0;
        if interval.is_zero() || last.elapsed() < interval {
            continue;
        }
        last = tokio::time::Instant::now();
        for l in &config.libraries {
            let _ = state.scan_tx.try_send(ScanRequest {
                library: l.id.clone(),
                opts: ScanOptions::default(),
            });
        }
    }
}

/// SIGHUP reloads the configuration, like `POST /api/v1/reload`.
fn spawn_sighup(state: Arc<AppState>) {
    tokio::spawn(async move {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            tracing::error!("cannot install the SIGHUP handler");
            return;
        };
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => return,
                _ = hup.recv() => {}
            }
            tracing::info!("SIGHUP received, reloading the configuration");
            if let Err(e) = crate::reload::reload(&state).await {
                tracing::error!(error = %e, "configuration reload failed; the running one is kept");
            }
        }
    });
}

fn spawn_signal_handlers(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot install the SIGTERM handler");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received, shutting down"),
        }
        shutdown.cancel();
    });
}
