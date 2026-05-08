use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use owo_colors::{OwoColorize, Style};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, Semaphore};
use tracing::{debug, error, info, warn};

/// 通过 [`WorkerPool::render`] 调用 worker 时使用的默认 PNG 缩放因子。
pub const DEFAULT_RENDER_SCALE: f32 = 4.0;

/// 日志行 worker slot 前缀的可见宽度（slot id 数字位数 + 1 个空格），
/// 父进程格式化器按此宽度补齐以保持列对齐。
pub fn log_prefix_width(pool_size: usize) -> usize {
    slot_id_digits(pool_size) + 1
}

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RenderRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub source: String,
    pub template: String,
    pub scale: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<usize>,
    pub format: String,
    pub stitch: bool,
}

#[derive(Debug, Deserialize)]
pub struct RenderResponse {
    #[allow(dead_code)]
    pub id: Option<String>,
    pub ok: bool,
    #[allow(dead_code)]
    pub format: Option<String>,
    pub data: Option<String>,
    #[allow(dead_code)]
    pub pages: Option<usize>,
    pub errors: Option<Vec<CompileError>>,
    #[allow(dead_code)]
    pub warnings: Option<Vec<CompileError>>,
}

#[derive(Debug, Deserialize)]
pub struct CompileError {
    #[allow(dead_code)]
    pub kind: String,
    pub message: String,
    pub span: Option<SpanInfo>,
    #[allow(dead_code)]
    #[serde(default)]
    pub hints: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SpanInfo {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Deserialize)]
struct ReadyMessage {
    pub ready: bool,
    pub protocol_version: u32,
    pub fonts_loaded: usize,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("failed to spawn worker: {0}")]
    Spawn(std::io::Error),
    #[error("worker did not send ready message: {0}")]
    NotReady(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("compilation failed: {0}")]
    Compile(String),
    #[error("invalid base64 data: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("worker process exited unexpectedly")]
    Exited,
    #[error("render timed out after {0:?}")]
    Timeout(Duration),
}

// ---------------------------------------------------------------------------
// PoolConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub pool_size: usize,
    pub render_timeout: Duration,
    pub spawn_timeout: Duration,
    pub memory_limit_bytes: u64,
    pub cpu_lifetime_secs: u64,
    /// `Some(n)` 表示将 `RLIMIT_FSIZE` 限制为 n 字节；`None` 表示不设置该限制。
    pub fsize_limit_bytes: Option<u64>,
    pub extra_args: Vec<String>,
    /// Worker binary name/path. Defaults to "typst-stdio-worker".
    pub worker_bin: String,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            pool_size: 2,
            render_timeout: Duration::from_secs(30),
            spawn_timeout: Duration::from_secs(120),
            memory_limit_bytes: 512 * 1024 * 1024,
            cpu_lifetime_secs: 3600,
            fsize_limit_bytes: Some(0),
            extra_args: Vec::new(),
            worker_bin: "typst-stdio-worker".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerProcess (single child process)
// ---------------------------------------------------------------------------

struct WorkerProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    request_counter: u64,
}

impl WorkerProcess {
    async fn spawn(config: &PoolConfig, slot_idx: usize) -> Result<Self, WorkerError> {
        let memory_limit = config.memory_limit_bytes;
        let cpu_lifetime = config.cpu_lifetime_secs;
        let fsize_limit = config.fsize_limit_bytes;

        debug!(
            worker_bin = %config.worker_bin,
            extra_args = ?config.extra_args,
            memory_limit_mib = memory_limit / 1024 / 1024,
            cpu_lifetime,
            ?fsize_limit,
            slot_idx,
            "spawning worker process"
        );

        let mut cmd = Command::new(&config.worker_bin);
        cmd.args(&config.extra_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()); // subprocess has RLIMIT_FSIZE=0, must own stderr pipe by parent process (this crate)

        unsafe {
            cmd.pre_exec(move || {
                use libc::{rlimit, setrlimit, RLIMIT_AS, RLIMIT_CPU, RLIMIT_FSIZE};

                let mem = rlimit {
                    rlim_cur: memory_limit,
                    rlim_max: memory_limit,
                };
                if setrlimit(RLIMIT_AS, &mem) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                let cpu = rlimit {
                    rlim_cur: cpu_lifetime,
                    rlim_max: cpu_lifetime,
                };
                if setrlimit(RLIMIT_CPU, &cpu) != 0 {
                    return Err(std::io::Error::last_os_error());
                }

                if let Some(fsize_limit) = fsize_limit {
                    let fsize = rlimit {
                        rlim_cur: fsize_limit,
                        rlim_max: fsize_limit,
                    };
                    if setrlimit(RLIMIT_FSIZE, &fsize) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }

                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(WorkerError::Spawn)?;
        let pid = child.id().unwrap_or(0);
        debug!(pid, "worker process spawned, waiting for ready message");

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout_raw = child.stdout.take().expect("stdout was piped");
        let stderr_raw = child.stderr.take().expect("stderr was piped");
        let mut stdout = BufReader::new(stdout_raw);

        // Forward child stderr to our stderr byte-for-byte, prefixing each line
        // with a colored slot id so child's own tracing format (timestamps,
        // levels, colors) is preserved and doesn't get wrapped by our tracing.
        let prefix = slot_prefix_bytes(slot_idx, config.pool_size);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr_raw);
            let mut buf = Vec::with_capacity(256);
            let mut out = tokio::io::stderr();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut line = Vec::with_capacity(prefix.len() + buf.len());
                        line.extend_from_slice(&prefix);
                        line.extend_from_slice(&buf);
                        if !line.ends_with(b"\n") {
                            line.push(b'\n');
                        }
                        if let Err(e) = out.write_all(&line).await {
                            eprintln!("worker stderr forward error: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(worker_pid = pid, error = %e, "error reading worker stderr");
                        break;
                    }
                }
            }
            debug!(worker_pid = pid, "worker stderr forwarder exiting");
        });

        // Piped child stdout will be read, await a line read.
        let mut ready_line = String::new();
        debug!("reading ready line from worker stdout...");
        let n = stdout
            .read_line(&mut ready_line)
            .await
            .map_err(|e| WorkerError::NotReady(e.to_string()))?;

        if n == 0 {
            // Delay 500ms if failed for get some stderr feedback.
            let status = match tokio::time::timeout(
                Duration::from_millis(500),
                child.wait(),
            )
            .await
            {
                Ok(Ok(s)) => format!("{s}"),
                Ok(Err(e)) => format!("wait failed: {e}"),
                Err(_) => "still running after 500ms".to_string(),
            };
            return Err(WorkerError::NotReady(format!(
                "worker closed stdout before sending ready (exit: {status})"
            )));
        }

        debug!(ready_line_len = ready_line.len(), raw = %ready_line.trim(), "received ready line from worker");

        let ready: ReadyMessage = serde_json::from_str(ready_line.trim())
            .map_err(|e| WorkerError::NotReady(format!("invalid ready JSON: {e}")))?;

        if !ready.ready {
            return Err(WorkerError::NotReady("ready field is false".into()));
        }

        info!(
            protocol_version = ready.protocol_version,
            fonts_loaded = ready.fonts_loaded,
            "typst worker ready"
        );

        Ok(Self {
            child,
            stdin,
            stdout,
            request_counter: 0,
        })
    }

    async fn render(
        &mut self,
        source: &str,
        template: &str,
        scale: f32,
        max_pages: Option<usize>,
    ) -> Result<Vec<u8>, WorkerError> {
        self.request_counter += 1;
        let id = format!("req-{}", self.request_counter);

        let request = RenderRequest {
            id: Some(id.clone()),
            source: source.to_string(),
            template: template.to_string(),
            scale,
            max_pages,
            format: "png".to_string(),
            stitch: true,
        };

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        debug!(id = %id, request_bytes = line.len(), "sending render request to worker stdin");
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        debug!(id = %id, "render request flushed, waiting for response on stdout...");

        let mut response_line = String::new();
        let n = self.stdout.read_line(&mut response_line).await?;
        if n == 0 {
            debug!(id = %id, "stdout returned 0 bytes (EOF), worker has exited");
            return Err(WorkerError::Exited);
        }
        debug!(id = %id, response_bytes = n, "received response from worker");

        let response: RenderResponse = serde_json::from_str(response_line.trim())?;
        debug!(
            id = %id,
            ok = response.ok,
            has_data = response.data.is_some(),
            has_errors = response.errors.is_some(),
            pages = ?response.pages,
            "parsed worker response"
        );

        if !response.ok {
            let error_msg = response
                .errors
                .as_ref()
                .map(|errs| {
                    errs.iter()
                        .map(|e| {
                            if let Some(span) = &e.span {
                                format!("[{}:{}] {}", span.line, span.column, e.message)
                            } else {
                                e.message.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(WorkerError::Compile(error_msg));
        }

        let data = response
            .data
            .ok_or_else(|| WorkerError::Compile("response missing data field".into()))?;

        let png_bytes = base64::engine::general_purpose::STANDARD.decode(&data)?;
        Ok(png_bytes)
    }

    async fn kill_and_reap(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

// ---------------------------------------------------------------------------
// WorkerSlot
// ---------------------------------------------------------------------------

struct WorkerSlot {
    worker: Option<WorkerProcess>,
    respawning: bool,
}

// ---------------------------------------------------------------------------
// WorkerPool
// ---------------------------------------------------------------------------

pub struct WorkerPool {
    slots: Vec<Arc<Mutex<WorkerSlot>>>,
    semaphore: Arc<Semaphore>,
    ready_notify: Arc<Notify>,
    config: Arc<PoolConfig>,
    total_renders: AtomicU64,
    total_deaths: AtomicU64,
}

impl WorkerPool {
    /// Spawn all workers in parallel, waiting for every Ready message.
    pub async fn new(config: PoolConfig) -> Result<Self, WorkerError> {
        let config = Arc::new(config);
        let pool_size = config.pool_size;

        debug!(pool_size, worker_bin = %config.worker_bin, ?config.render_timeout, ?config.spawn_timeout, "creating worker pool");

        let mut spawn_futures = Vec::with_capacity(pool_size);
        for i in 0..pool_size {
            let cfg = config.clone();
            spawn_futures.push(async move {
                debug!(slot_idx = i, "spawning worker for slot");
                tokio::time::timeout(cfg.spawn_timeout, WorkerProcess::spawn(&cfg, i)).await
            });
        }

        debug!("waiting for all workers to spawn...");
        let results = futures::future::join_all(spawn_futures).await;

        let mut slots = Vec::with_capacity(pool_size);
        for (i, result) in results.into_iter().enumerate() {
            let worker = match result {
                Ok(Ok(w)) => w,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(WorkerError::NotReady(format!(
                        "worker {i} spawn timed out after {:?}",
                        config.spawn_timeout,
                    )));
                }
            };
            slots.push(Arc::new(Mutex::new(WorkerSlot {
                worker: Some(worker),
                respawning: false,
            })));
        }

        info!(pool_size, "worker pool ready");

        Ok(Self {
            slots,
            semaphore: Arc::new(Semaphore::new(pool_size)),
            ready_notify: Arc::new(Notify::new()),
            config,
            total_renders: AtomicU64::new(0),
            total_deaths: AtomicU64::new(0),
        })
    }

    pub async fn render(&self, source: &str, template: &str) -> Result<Vec<u8>, WorkerError> {
        self.render_with_options(source, template, DEFAULT_RENDER_SCALE, None)
            .await
    }

    pub async fn render_with_options(
        &self,
        source: &str,
        template: &str,
        scale: f32,
        max_pages: Option<usize>,
    ) -> Result<Vec<u8>, WorkerError> {
        let render_id = self.total_renders.fetch_add(1, Ordering::Relaxed);
        let source_len = source.len();
        debug!(render_id, source_len, template, scale, ?max_pages, "render request queued");

        debug!(render_id, "acquiring semaphore permit...");
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        debug!(render_id, "semaphore permit acquired");

        debug!(render_id, "acquiring worker slot...");
        let (slot_idx, slot_arc) = self.acquire_slot().await?;
        debug!(render_id, slot_idx, "worker slot acquired, locking...");
        let mut slot = slot_arc.lock().await;
        debug!(render_id, slot_idx, has_worker = slot.worker.is_some(), respawning = slot.respawning, "slot locked");

        if slot.worker.is_none() && !slot.respawning {
            warn!(slot_idx, "slot had no worker and was not respawning; spawning synchronously");
            match tokio::time::timeout(
                self.config.spawn_timeout,
                WorkerProcess::spawn(&self.config, slot_idx),
            )
            .await
            {
                Ok(Ok(w)) => slot.worker = Some(w),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(WorkerError::NotReady(
                        "synchronous respawn timed out".into(),
                    ))
                }
            }
        }

        let worker = slot.worker.as_mut().expect("worker must be present");
        let start = Instant::now();
        debug!(render_id, slot_idx, "dispatching render to worker process");

        let result = tokio::time::timeout(
            self.config.render_timeout,
            worker.render(source, template, scale, max_pages),
        )
        .await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok(png)) => {
                info!(
                    render_id,
                    slot_idx,
                    elapsed_ms = elapsed.as_millis() as u64,
                    png_bytes = png.len(),
                    "render ok"
                );
                Ok(png)
            }
            Ok(Err(WorkerError::Exited)) => {
                error!(render_id, slot_idx, elapsed_ms = elapsed.as_millis() as u64, "worker exited during render");
                self.handle_worker_death(&mut slot, slot_idx, slot_arc.clone());
                Err(WorkerError::Exited)
            }
            Ok(Err(WorkerError::Io(ref _e))) => {
                warn!(render_id, slot_idx, elapsed_ms = elapsed.as_millis() as u64, "I/O error during render (worker likely dead)");
                self.handle_worker_death(&mut slot, slot_idx, slot_arc.clone());
                Err(WorkerError::Exited)
            }
            Ok(Err(e)) => {
                debug!(render_id, slot_idx, elapsed_ms = elapsed.as_millis() as u64, %e, "render failed (worker alive)");
                Err(e)
            }
            Err(_) => {
                let timeout = self.config.render_timeout;
                warn!(render_id, slot_idx, ?timeout, "render timed out, killing worker");
                self.handle_worker_death(&mut slot, slot_idx, slot_arc.clone());
                Err(WorkerError::Timeout(timeout))
            }
        }
    }

    /// Find an available (unlocked, not-respawning) slot. Returns (index, Arc).
    async fn acquire_slot(&self) -> Result<(usize, Arc<Mutex<WorkerSlot>>), WorkerError> {
        loop {
            for (idx, slot_arc) in self.slots.iter().enumerate() {
                let Ok(slot) = slot_arc.try_lock() else {
                    debug!(idx, "slot is locked by another task, skipping");
                    continue;
                };
                if !slot.respawning {
                    drop(slot);
                    debug!(idx, "found available slot");
                    return Ok((idx, slot_arc.clone()));
                }
                debug!(idx, "slot is respawning, skipping");
            }
            debug!("no available slots, waiting for ready_notify...");
            self.ready_notify.notified().await;
            debug!("ready_notify received, retrying slot acquisition");
        }
    }

    /// Kill the worker, reap the zombie, and kick off an eager background respawn.
    fn handle_worker_death(
        &self,
        slot: &mut tokio::sync::MutexGuard<'_, WorkerSlot>,
        slot_idx: usize,
        slot_arc: Arc<Mutex<WorkerSlot>>,
    ) {
        let death_count = self.total_deaths.fetch_add(1, Ordering::Relaxed) + 1;
        let mut worker = match slot.worker.take() {
            Some(w) => w,
            None => return,
        };
        slot.respawning = true;

        error!(slot_idx, death_count, "worker died, starting background respawn");

        let config = self.config.clone();
        let notify = self.ready_notify.clone();

        tokio::spawn(async move {
            debug!(slot_idx, "killing and reaping dead worker...");
            worker.kill_and_reap().await;
            drop(worker);
            debug!(slot_idx, "dead worker reaped, starting respawn...");

            let start = Instant::now();
            let spawn_result = tokio::time::timeout(
                config.spawn_timeout,
                WorkerProcess::spawn(&config, slot_idx),
            )
            .await;

            let mut slot = slot_arc.lock().await;
            match spawn_result {
                Ok(Ok(new_worker)) => {
                    info!(slot_idx, elapsed_ms = start.elapsed().as_millis() as u64, "worker respawned successfully");
                    slot.worker = Some(new_worker);
                }
                Ok(Err(e)) => {
                    error!(slot_idx, %e, "worker respawn failed");
                }
                Err(_) => {
                    error!(slot_idx, timeout = ?config.spawn_timeout, "worker respawn timed out");
                }
            }
            slot.respawning = false;
            debug!(slot_idx, "respawn complete, notifying waiters");
            notify.notify_waiters();
        });
    }

    /// Snapshot of pool health for diagnostics.
    // TODO: 退出报错时应该报告一下情况
    pub async fn stats(&self) -> PoolStats {
        let mut alive = 0usize;
        let mut respawning = 0usize;
        let mut empty = 0usize;
        for slot_arc in &self.slots {
            let slot = slot_arc.lock().await;
            if slot.worker.is_some() {
                alive += 1;
            } else if slot.respawning {
                respawning += 1;
            } else {
                empty += 1;
            }
        }
        PoolStats {
            pool_size: self.config.pool_size,
            alive,
            respawning,
            empty,
            total_renders: self.total_renders.load(Ordering::Relaxed),
            total_deaths: self.total_deaths.load(Ordering::Relaxed),
        }
    }

    pub async fn shutdown(self) {
        debug!("shutting down worker pool, closing {} slots...", self.slots.len());
        for (idx, slot_arc) in self.slots.iter().enumerate() {
            let mut slot = slot_arc.lock().await;
            if let Some(mut worker) = slot.worker.take() {
                debug!(slot_idx = idx, "closing worker stdin and waiting for exit");
                drop(worker.stdin);
                let _ = worker.child.wait().await;
                debug!(slot_idx = idx, "worker exited");
            }
        }
        info!("worker pool shut down");
    }
}

// ---------------------------------------------------------------------------
// PoolStats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PoolStats {
    pub pool_size: usize,
    pub alive: usize,
    pub respawning: usize,
    pub empty: usize,
    pub total_renders: u64,
    pub total_deaths: u64,
}

impl std::fmt::Display for PoolStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pool({}/{} alive, {} respawning, {} empty | renders: {}, deaths: {})",
            self.alive,
            self.pool_size,
            self.respawning,
            self.empty,
            self.total_renders,
            self.total_deaths,
        )
    }
}

// ---------------------------------------------------------------------------
// Internal helpers for stderr forwarder prefix
// ---------------------------------------------------------------------------

fn slot_id_digits(pool_size: usize) -> usize {
    pool_size.saturating_sub(1).max(1).to_string().len()
}

fn stderr_supports_color() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
    })
}

fn slot_prefix_bytes(slot_idx: usize, pool_size: usize) -> Vec<u8> {
    let width = slot_id_digits(pool_size);
    let num = format!("{slot_idx:0width$}");
    if stderr_supports_color() {
        format!("{} ", num.style(Style::new().bold().cyan())).into_bytes()
    } else {
        format!("{num} ").into_bytes()
    }
}
