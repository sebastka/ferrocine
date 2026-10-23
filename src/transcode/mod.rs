//! HLS sessions: one ffmpeg process per playback, writing fMP4 segments to
//! an ephemeral directory.
//!
//! A session starts at the requested position and runs ahead of the player
//! by at most [`AHEAD`]; ffmpeg is paused (SIGSTOP) beyond that and resumed
//! when the player catches up, so a paused film does not fill the disk.
//! Segments far behind the player are deleted for the same reason. Sessions
//! not read for [`IDLE`] are stopped.

pub mod hw;
pub mod plan;
pub mod subtitles;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::auth::oidc::random_token;
use crate::config::TranscodingConfig;
use crate::metrics::Metrics;
use crate::store::now_ms;
use hw::{FfmpegInfo, HwKind};
use plan::{CommandSpec, Method, Pipeline, Plan};

/// How far ffmpeg may run ahead of the last segment requested.
const AHEAD: Duration = Duration::from_secs(180);
/// Segments further behind the last one requested are deleted.
const KEEP_BEHIND: Duration = Duration::from_secs(600);
/// Sessions are stopped when not read for this long.
pub const IDLE: Duration = Duration::from_secs(60);
/// The first segment must be ready within this time.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("too many playback sessions; try again later")]
    Busy,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub username: String,
    pub item_id: i64,
    pub title: String,
    pub method: Method,
    pub hw_accel: Option<HwKind>,
    pub started_at: i64,
    dir: PathBuf,
    pid: Option<u32>,
    last_access: AtomicI64,
    /// Highest segment number requested.
    requested: AtomicI64,
    paused: AtomicBool,
    cancel: CancellationToken,
}

#[derive(Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub user: String,
    pub item_id: i64,
    pub title: String,
    pub method: Method,
    pub hw_accel: Option<HwKind>,
    pub started_at: i64,
}

pub struct StartRequest<'a> {
    pub plan: &'a Plan,
    pub file: &'a Path,
    pub start_ms: i64,
    pub user_id: i64,
    pub username: &'a str,
    pub item_id: i64,
    pub title: &'a str,
}

pub struct Transcoder {
    cfg: TranscodingConfig,
    pub ffmpeg: FfmpegInfo,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    metrics: Metrics,
}

/// `seg00042.m4s` → 42.
fn segment_number(name: &str) -> Option<i64> {
    name.strip_prefix("seg")?.strip_suffix(".m4s")?.parse().ok()
}

/// Files a client may read from a session directory.
pub fn valid_file(name: &str) -> bool {
    name == "index.m3u8" || name == "init.mp4" || (segment_number(name).is_some() && name.len() == 12)
}

fn signal(pid: Option<u32>, stop: bool) {
    let Some(pid) = pid.and_then(|p| rustix::process::Pid::from_raw(p as i32)) else {
        return;
    };
    let sig = if stop {
        rustix::process::Signal::STOP
    } else {
        rustix::process::Signal::CONT
    };
    if let Err(e) = rustix::process::kill_process(pid, sig) {
        tracing::debug!(error = %e, "cannot signal ffmpeg");
    }
}

impl Transcoder {
    pub fn new(cfg: TranscodingConfig, ffmpeg: FfmpegInfo, metrics: Metrics) -> Self {
        Self {
            cfg,
            ffmpeg,
            sessions: Mutex::new(HashMap::new()),
            metrics,
        }
    }

    pub fn temp_dir(&self) -> &Path {
        &self.cfg.temp_dir
    }

    /// Remove what a previous process left in the scratch directory.
    pub async fn clean_temp(&self) -> anyhow::Result<()> {
        let dir = &self.cfg.temp_dir;
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let mut rd = tokio::fs::read_dir(dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().into_owned();
            // Only session directories: never delete what is not ours.
            if name.len() == 22 && e.file_type().await?.is_dir() {
                let _ = tokio::fs::remove_dir_all(e.path()).await;
            }
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let Ok(s) = self.sessions.lock() else { return Vec::new() };
        let mut out: Vec<SessionInfo> = s
            .values()
            .map(|s| SessionInfo {
                session_id: s.id.clone(),
                user: s.username.clone(),
                item_id: s.item_id,
                title: s.title.clone(),
                method: s.method,
                hw_accel: s.hw_accel,
                started_at: s.started_at,
            })
            .collect();
        out.sort_by_key(|s| s.started_at);
        out
    }

    fn count(&self) -> usize {
        self.sessions.lock().map_or(0, |s| s.len())
    }

    fn update_metrics(&self) {
        let Ok(s) = self.sessions.lock() else { return };
        let mut counts: HashMap<(&str, &str), i64> = HashMap::new();
        for m in ["remux", "transcode"] {
            for a in ["none", "vaapi", "qsv", "nvenc"] {
                counts.insert((m, a), 0);
            }
        }
        for x in s.values() {
            *counts
                .entry((x.method.as_str(), x.hw_accel.map_or("none", HwKind::as_str)))
                .or_default() += 1;
        }
        for ((m, a), n) in counts {
            self.metrics.set_sessions(m, a, n);
        }
    }

    /// Start ffmpeg and wait for the first segment, falling back from
    /// hardware to software pipelines when one fails to start.
    pub async fn start(&self, r: StartRequest<'_>) -> Result<Arc<Session>, StartError> {
        if self.ffmpeg.version.is_none() {
            return Err(StartError::Failed(anyhow::anyhow!("ffmpeg is not available")));
        }
        if self.count() >= self.cfg.max_sessions {
            return Err(StartError::Busy);
        }
        let encoding = r.plan.method == Method::Transcode;
        let pipelines = if encoding {
            plan::pipelines(self.ffmpeg.hw_accel, r.plan.tone_mapped())
        } else {
            vec![Pipeline::Software]
        };
        let id = random_token(16);
        let dir = self.cfg.temp_dir.join(&id);
        let mut last_error = None;
        for pipeline in pipelines {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("cannot create {}", dir.display()))?;
            match self.spawn(&r, pipeline, &dir).await {
                Ok((child, stderr_tail)) => {
                    let session = Arc::new(Session {
                        id: id.clone(),
                        user_id: r.user_id,
                        username: r.username.to_owned(),
                        item_id: r.item_id,
                        title: r.title.to_owned(),
                        method: r.plan.method,
                        hw_accel: if encoding { pipeline.accel() } else { None },
                        started_at: now_ms(),
                        dir: dir.clone(),
                        pid: child.id(),
                        last_access: AtomicI64::new(now_ms()),
                        requested: AtomicI64::new(0),
                        paused: AtomicBool::new(false),
                        cancel: CancellationToken::new(),
                    });
                    if let Ok(mut s) = self.sessions.lock() {
                        s.insert(id.clone(), session.clone());
                    }
                    self.update_metrics();
                    tokio::spawn(supervise(session.clone(), child, stderr_tail, self.cfg.segment_seconds));
                    tracing::info!(
                        session = %id,
                        user = %r.username,
                        item = r.item_id,
                        method = r.plan.method.as_str(),
                        pipeline = ?pipeline,
                        tone_mapping = r.plan.tone_mapped(),
                        start_ms = r.start_ms,
                        "playback session started"
                    );
                    return Ok(session);
                }
                Err(e) => {
                    tracing::warn!(pipeline = ?pipeline, error = %format!("{e:#}"), "ffmpeg pipeline failed to start");
                    last_error = Some(e);
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Err(StartError::Failed(
            last_error.unwrap_or_else(|| anyhow::anyhow!("no pipeline")),
        ))
    }

    /// Spawn ffmpeg; succeed once the playlist lists a segment.
    async fn spawn(
        &self,
        r: &StartRequest<'_>,
        pipeline: Pipeline,
        dir: &Path,
    ) -> anyhow::Result<(tokio::process::Child, Arc<Mutex<Vec<String>>>)> {
        let args = plan::hls_args(
            r.plan,
            pipeline,
            &CommandSpec {
                input: r.file,
                start_ms: r.start_ms,
                out_dir: dir,
                segment_seconds: self.cfg.segment_seconds,
                device: &self.cfg.device,
                x264_preset: &self.cfg.x264_preset,
                tone_mapper: self.ffmpeg.tone_mapper.unwrap_or(hw::ToneMapper::Tonemapx),
            },
        );
        tracing::debug!(args = %args.join(" "), "starting ffmpeg");
        let mut child = tokio::process::Command::new(&self.cfg.ffmpeg)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("cannot run ffmpeg")?;
        // Keep the last lines of ffmpeg's log for error messages.
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut t) = tail.lock() {
                        t.push(line);
                        if t.len() > 20 {
                            t.remove(0);
                        }
                    }
                }
            });
        }
        let playlist = dir.join("index.m3u8");
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&playlist).await
                && text.contains(".m4s")
            {
                return Ok((child, tail));
            }
            if let Some(status) = child.try_wait()? {
                // A very short input may finish before we look.
                if status.success() && playlist.exists() {
                    return Ok((child, tail));
                }
                let log = tail.lock().map(|t| t.join(" | ")).unwrap_or_default();
                bail!("ffmpeg exited with {status}: {log}");
            }
            if tokio::time::Instant::now() > deadline {
                let _ = child.kill().await;
                bail!("ffmpeg produced no segment within {}s", START_TIMEOUT.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A file of a session, for its owner; records the access.
    pub fn file(&self, id: &str, user_id: i64, name: &str) -> Option<PathBuf> {
        if !valid_file(name) {
            return None;
        }
        let session = self.sessions.lock().ok()?.get(id).cloned()?;
        if session.user_id != user_id {
            return None;
        }
        session.last_access.store(now_ms(), Ordering::Relaxed);
        if let Some(n) = segment_number(name) {
            session.requested.fetch_max(n, Ordering::Relaxed);
            // Resume at once rather than at the next tick.
            if session.paused.swap(false, Ordering::Relaxed) {
                signal(session.pid, false);
            }
        }
        Some(session.dir.join(name))
    }

    pub async fn stop(&self, id: &str, user_id: Option<i64>) -> bool {
        let session = {
            let Ok(mut s) = self.sessions.lock() else { return false };
            match s.get(id) {
                Some(x) if user_id.is_none_or(|u| u == x.user_id) => s.remove(id),
                _ => None,
            }
        };
        let Some(session) = session else { return false };
        session.cancel.cancel();
        self.update_metrics();
        tracing::info!(session = %id, "playback session stopped");
        true
    }

    /// Stop idle sessions until `shutdown`, then every session.
    pub async fn reaper(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            let now = now_ms();
            let idle: Vec<String> = self
                .sessions
                .lock()
                .map(|s| {
                    s.values()
                        .filter(|x| now - x.last_access.load(Ordering::Relaxed) > IDLE.as_millis() as i64)
                        .map(|x| x.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            for id in idle {
                tracing::debug!(session = %id, "idle session");
                self.stop(&id, None).await;
            }
        }
        let all: Vec<String> = self
            .sessions
            .lock()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default();
        for id in all {
            self.stop(&id, None).await;
        }
    }
}

/// Watch a session's ffmpeg: throttle it, prune old segments, and clean up
/// when the session is stopped.
async fn supervise(
    session: Arc<Session>,
    mut child: tokio::process::Child,
    tail: Arc<Mutex<Vec<String>>>,
    segment_seconds: u32,
) {
    let ahead = (AHEAD.as_secs() / u64::from(segment_seconds)) as i64;
    let behind = (KEEP_BEHIND.as_secs() / u64::from(segment_seconds)) as i64;
    let mut running = true;
    let mut pruned_to = 0i64;
    loop {
        tokio::select! {
            _ = session.cancel.cancelled() => break,
            status = child.wait(), if running => {
                running = false;
                match status {
                    Ok(s) if s.success() => tracing::debug!(session = %session.id, "ffmpeg finished"),
                    Ok(s) => {
                        let log = tail.lock().map(|t| t.join(" | ")).unwrap_or_default();
                        tracing::warn!(session = %session.id, status = %s, log = %log, "ffmpeg failed");
                    }
                    Err(e) => tracing::warn!(session = %session.id, error = %e, "cannot wait for ffmpeg"),
                }
            }
            _ = tokio::time::sleep(TICK) => {
                let requested = session.requested.load(Ordering::Relaxed);
                let produced = newest_segment(&session.dir).await;
                if running {
                    let paused = session.paused.load(Ordering::Relaxed);
                    if !paused && produced - requested > ahead {
                        session.paused.store(true, Ordering::Relaxed);
                        signal(session.pid, true);
                    } else if paused && produced - requested < ahead / 2 {
                        session.paused.store(false, Ordering::Relaxed);
                        signal(session.pid, false);
                    }
                }
                let keep_from = requested - behind;
                if keep_from > pruned_to {
                    for n in pruned_to..keep_from {
                        let _ = tokio::fs::remove_file(session.dir.join(format!("seg{n:05}.m4s"))).await;
                    }
                    pruned_to = keep_from;
                }
            }
        }
    }
    if running {
        // Resume a paused ffmpeg so that it can exit cleanly.
        signal(session.pid, false);
        let _ = child.kill().await;
    }
    let _ = tokio::fs::remove_dir_all(&session.dir).await;
}

/// Highest segment number written so far.
async fn newest_segment(dir: &Path) -> i64 {
    let mut newest = 0;
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        if let Some(n) = segment_number(&e.file_name().to_string_lossy()) {
            newest = newest.max(n);
        }
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_session_files_are_served() {
        assert!(valid_file("index.m3u8"));
        assert!(valid_file("init.mp4"));
        assert!(valid_file("seg00042.m4s"));
        assert_eq!(segment_number("seg00042.m4s"), Some(42));
        for bad in ["../x", "seg1.m4s", "seg00001.m4s.tmp", "index.m3u8.tmp", "seg0000a.m4s"] {
            assert!(!valid_file(bad), "{bad}");
        }
    }
}
