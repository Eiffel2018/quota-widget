//! A deliberately narrow, read-only client of the official Codex CLI app-server.
//!
//! Protocol reference: https://learn.chatgpt.com/docs/app-server
//! Native npm layout: https://github.com/openai/codex/blob/main/codex-cli/bin/codex.js
//! Outgoing methods are confined to `initialize`, `initialized`, and
//! `account/rateLimits/read`. Authentication belongs to the already-installed CLI.
//! This module never reads authentication files, starts a task, or starts login.

use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const LOOP_INTERVAL: Duration = Duration::from_millis(40);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const FRAME_QUEUE_CAPACITY: usize = 32;
const WRITE_QUEUE_CAPACITY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeCommand {
    Refresh,
    Stop,
}

/// Payloads are transient. Do not serialize or log this event enum.
#[derive(Debug)]
pub enum BridgeEvent {
    Connecting,
    /// The protocol handshake succeeded; quota availability is still unknown.
    Connected,
    /// Complete `account/rateLimits/read` result, not a sparse notification.
    RateLimits(Value),
    Error(BridgeError),
    Stopped,
}

/// Fixed, non-sensitive errors. Never includes a path, stderr or server text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    NotFound,
    AbsolutePathRequired,
    ProtectedPath,
    NativeExecutableRequired,
    UnreadableExecutable,
    WorkingDirectory,
    StartFailed,
    Transport,
    Timeout,
    Protocol,
    Unavailable,
    UnsupportedRequest,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFound => {
                "Standalone Codex CLI not found. Choose its native executable in settings."
            }
            Self::AbsolutePathRequired => {
                "Choose an absolute path to the native Codex CLI executable."
            }
            Self::ProtectedPath => {
                "WindowsApps paths are not supported. Use the standalone Codex CLI."
            }
            Self::NativeExecutableRequired => {
                "Choose a native Codex CLI executable, not a script or launcher."
            }
            Self::UnreadableExecutable => "The selected executable cannot be read or run.",
            Self::WorkingDirectory => "Unable to prepare a safe working directory.",
            Self::StartFailed => "The Codex CLI app-server could not be started.",
            Self::Transport => "The Codex CLI connection ended or could not be read.",
            Self::Timeout => "The Codex CLI did not respond in time. Reconnect to try again.",
            Self::Protocol => {
                "The Codex CLI returned an unsupported response. Check for an official CLI update."
            }
            Self::Unavailable => {
                "Account quota is unavailable. Check your existing official CLI sign-in."
            }
            Self::UnsupportedRequest => {
                "The Codex CLI requested an operation this read-only widget cannot perform."
            }
        })
    }
}

impl std::error::Error for BridgeError {}

/// Owns precisely one worker and its app-server child. Dropping stops and reaps it.
pub struct Bridge {
    pub commands: Sender<BridgeCommand>,
    pub events: Receiver<BridgeEvent>,
    worker: Option<JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
}

impl Bridge {
    /// Starts asynchronously. Discovery/start failures arrive as redacted events.
    /// An empty path should be converted to `None` by the caller.
    pub fn spawn(configured_path: Option<PathBuf>) -> Self {
        Self::spawn_worker(
            move || {
                let executable = discover_cli(configured_path.as_deref())?;
                let mut command = Command::new(executable);
                command.arg("app-server");
                Ok(command)
            },
            Timing::default(),
        )
    }

    fn spawn_worker<F>(make_command: F, timing: Timing) -> Self
    where
        F: FnOnce() -> Result<Command, BridgeError> + Send + 'static,
    {
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_requested);
        let worker = thread::Builder::new()
            .name("quota-read-only-bridge".into())
            .spawn(move || {
                let _ = event_tx.send(BridgeEvent::Connecting);
                let outcome = if worker_stop.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    make_command().and_then(|command| {
                        run(command, &command_rx, &event_tx, &worker_stop, timing)
                    })
                };
                if let Err(error) = outcome {
                    let _ = event_tx.send(BridgeEvent::Error(error));
                }
                let _ = event_tx.send(BridgeEvent::Stopped);
            });
        match worker {
            Ok(worker) => Self {
                commands,
                events,
                worker: Some(worker),
                stop_requested,
            },
            Err(_) => {
                // Thread failure must also be visible, without exposing OS details.
                let (tx, events) = mpsc::channel();
                let _ = tx.send(BridgeEvent::Error(BridgeError::StartFailed));
                let _ = tx.send(BridgeEvent::Stopped);
                Self {
                    commands,
                    events,
                    worker: None,
                    stop_requested,
                }
            }
        }
    }

    pub fn refresh(&self) {
        if !self.stop_requested.load(Ordering::Acquire) {
            let _ = self.commands.send(BridgeCommand::Refresh);
        }
    }

    /// Signals cancellation without joining. Safe to call from the UI thread.
    /// The atomic flag takes priority over any queued refresh commands.
    pub fn request_stop(&self) {
        if !self.stop_requested.swap(true, Ordering::AcqRel) {
            let _ = self.commands.send(BridgeCommand::Stop);
        }
    }

    /// A finished worker has already killed and reaped its exact owned child.
    /// No handle also means finished, including after spawn failure or stop().
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Idempotent blocking join, also used by Drop as a final cleanup safeguard.
    /// UI callers should request_stop(), poll is_finished(), then call stop().
    /// This never sends logout or terminates an unrelated process.
    pub fn stop(&mut self) {
        self.request_stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy)]
struct Timing {
    poll: Duration,
    timeout: Duration,
    refresh_floor: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: POLL_INTERVAL,
            timeout: REQUEST_TIMEOUT,
            refresh_floor: MIN_REFRESH_INTERVAL,
        }
    }
}

/// Discovery examines only executable locations. It does not execute `which`,
/// npm, a shell, or a candidate executable, and never reads the CLI's auth files.
pub fn discover_cli(configured: Option<&Path>) -> Result<PathBuf, BridgeError> {
    if let Some(path) = configured {
        return validate_executable(path);
    }
    for candidate in automatic_candidates() {
        if let Ok(path) = validate_executable(&candidate) {
            return Ok(path);
        }
    }
    Err(BridgeError::NotFound)
}

fn protected_path(path: &Path) -> bool {
    // Check both separators even on Unix so Windows-form inputs are rejected
    // before touching any protected directory or following an alias.
    path.to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case("windowsapps"))
}

fn script_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["cmd", "bat", "ps1", "js", "mjs", "cjs", "sh", "bash", "py"]
                .iter()
                .any(|script| extension.eq_ignore_ascii_case(script))
        })
}

fn validate_executable(path: &Path) -> Result<PathBuf, BridgeError> {
    if protected_path(path) {
        return Err(BridgeError::ProtectedPath);
    }
    if !path.is_absolute() {
        return Err(BridgeError::AbsolutePathRequired);
    }
    if script_path(path) {
        return Err(BridgeError::NativeExecutableRequired);
    }
    let resolved = fs::canonicalize(path).map_err(|_| BridgeError::UnreadableExecutable)?;
    if protected_path(&resolved) {
        return Err(BridgeError::ProtectedPath);
    }
    if script_path(&resolved) {
        return Err(BridgeError::NativeExecutableRequired);
    }
    let metadata = fs::metadata(&resolved).map_err(|_| BridgeError::UnreadableExecutable)?;
    if !metadata.is_file() {
        return Err(BridgeError::NativeExecutableRequired);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(BridgeError::UnreadableExecutable);
        }
    }
    #[cfg(windows)]
    if !resolved
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err(BridgeError::NativeExecutableRequired);
    }
    let mut magic = [0u8; 4];
    File::open(&resolved)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|_| BridgeError::UnreadableExecutable)?;
    if !native_magic(magic) {
        return Err(BridgeError::NativeExecutableRequired);
    }
    Ok(resolved)
}

fn native_magic(magic: [u8; 4]) -> bool {
    if cfg!(target_os = "windows") {
        magic[..2] == *b"MZ"
    } else if cfg!(target_os = "macos") {
        matches!(
            magic,
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        )
    } else {
        magic == *b"\x7fELF"
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "codex.exe"
    } else {
        "codex"
    }
}

fn npm_target() -> Option<(&'static str, &'static str)> {
    match (env::consts::OS, env::consts::ARCH) {
        ("windows", "x86_64") => Some(("x86_64-pc-windows-msvc", "codex-win32-x64")),
        ("windows", "aarch64") => Some(("aarch64-pc-windows-msvc", "codex-win32-arm64")),
        ("macos", "x86_64") => Some(("x86_64-apple-darwin", "codex-darwin-x64")),
        ("macos", "aarch64") => Some(("aarch64-apple-darwin", "codex-darwin-arm64")),
        ("linux", "x86_64") => Some(("x86_64-unknown-linux-musl", "codex-linux-x64")),
        ("linux", "aarch64") => Some(("aarch64-unknown-linux-musl", "codex-linux-arm64")),
        _ => None,
    }
}

fn push_npm_package(candidates: &mut Vec<PathBuf>, root: &Path, triple: &str) {
    // `bin` is the current layout; `codex` supports older official releases.
    for folder in ["bin", "codex"] {
        candidates.push(
            root.join("vendor")
                .join(triple)
                .join(folder)
                .join(binary_name()),
        );
    }
}

fn npm_candidates(node_modules: &Path, candidates: &mut Vec<PathBuf>) {
    let Some((triple, platform_package)) = npm_target() else {
        return;
    };
    let scope = node_modules.join("@openai");
    let main = scope.join("codex");
    push_npm_package(candidates, &scope.join(platform_package), triple);
    push_npm_package(candidates, &main, triple);
    push_npm_package(
        candidates,
        &main.join("node_modules/@openai").join(platform_package),
        triple,
    );
}

fn automatic_candidates() -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path)
                .filter(|p| p.is_absolute() && !protected_path(p))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = PathBuf::from(home);
        for sub in [".local/bin", ".cargo/bin", ".npm-global/bin", ".bun/bin"] {
            directories.push(home.join(sub));
        }
    }
    if cfg!(windows) {
        for (key, suffix) in [
            ("APPDATA", "npm"),
            ("LOCALAPPDATA", "Programs/nodejs"),
            ("ProgramFiles", "nodejs"),
        ] {
            if let Some(root) = env::var_os(key) {
                directories.push(PathBuf::from(root).join(suffix));
            }
        }
    } else {
        directories.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    let mut candidates = Vec::new();
    for directory in directories {
        if !directory.is_absolute() || protected_path(&directory) {
            continue;
        }
        let native = directory.join(binary_name());
        candidates.push(native.clone());
        npm_candidates(&directory.join("node_modules"), &mut candidates);
        if let Some(prefix) = directory.parent() {
            npm_candidates(&prefix.join("lib/node_modules"), &mut candidates);
            npm_candidates(&prefix.join("node_modules"), &mut candidates);
        }
        // Resolve an npm/pnpm/bun symlink solely to locate its native sibling.
        // Never execute or read the contents of the JavaScript entry point.
        for entry in [native, directory.join("codex")] {
            if let Ok(resolved) = fs::canonicalize(entry) {
                if protected_path(&resolved) {
                    continue;
                }
                for root in resolved.ancestors().take(5) {
                    if root.file_name().is_some_and(|name| name == "codex")
                        && root
                            .parent()
                            .and_then(Path::file_name)
                            .is_some_and(|name| name == "@openai")
                    {
                        if let Some((triple, _)) = npm_target() {
                            push_npm_package(&mut candidates, root, triple);
                        }
                        npm_candidates(&root.join("node_modules"), &mut candidates);
                        if let Some(node_modules) = root.parent().and_then(Path::parent) {
                            npm_candidates(node_modules, &mut candidates);
                        }
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    candidates
}

struct WorkingDirectory(PathBuf);
impl WorkingDirectory {
    fn create() -> Result<Self, BridgeError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..20 {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "quota-widget-bridge-{}-{now}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    let directory = Self(path);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700))
                            .map_err(|_| BridgeError::WorkingDirectory)?;
                    }
                    return Ok(directory);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(BridgeError::WorkingDirectory),
            }
        }
        Err(BridgeError::WorkingDirectory)
    }
}
impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Child::kill acts on this exact process handle/PID, never a name search.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
enum Incoming {
    Json(Value),
    Invalid,
}

fn read_frames<R: Read>(reader: R, tx: SyncSender<Incoming>, overflow: Arc<AtomicBool>) {
    let mut reader = BufReader::new(reader);
    loop {
        // `take` bounds memory even if a process never terminates a line.
        let mut bytes = Vec::new();
        let result = (&mut reader)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes);
        let item = match result {
            Ok(0) => break,
            Ok(_) if bytes.len() > MAX_FRAME_BYTES => Incoming::Invalid,
            Ok(_) => match serde_json::from_slice(&bytes) {
                Ok(value) => Incoming::Json(value),
                Err(_) => Incoming::Invalid,
            },
            Err(_) => Incoming::Invalid,
        };
        let invalid = matches!(item, Incoming::Invalid);
        match tx.try_send(item) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                overflow.store(true, Ordering::Release);
                break;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => break,
        }
        if invalid {
            break;
        }
    }
}

fn write_json(writer: &mut impl Write, message: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *writer, message).map_err(|_| BridgeError::Transport)?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|_| BridgeError::Transport)
}

/// Pipe writes must never hold up cancellation or the request timeout. A child
/// can keep stdout open while ceasing to read stdin, filling its input pipe.
/// This bounded queue leaves the supervising worker free to kill/reap it.
struct PipeWriter {
    messages: SyncSender<Value>,
    failed: Arc<AtomicBool>,
}

impl PipeWriter {
    fn spawn(mut writer: impl Write + Send + 'static) -> Result<Self, BridgeError> {
        let (messages, requests) = mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        let thread_failed = Arc::clone(&failed);
        // Like the reader, this thread owns only a pipe. Killing the owned child
        // releases normal blocked writes. Do not join pipe threads: an unrelated
        // descendant that inherited a pipe must not delay the supervisor's stop.
        let _writer = thread::Builder::new()
            .name("quota-protocol-writer".into())
            .spawn(move || {
                while let Ok(message) = requests.recv() {
                    if write_json(&mut writer, &message).is_err() {
                        thread_failed.store(true, Ordering::Release);
                        break;
                    }
                }
            })
            .map_err(|_| BridgeError::StartFailed)?;
        Ok(Self { messages, failed })
    }

    fn send(&self, message: Value) -> Result<(), BridgeError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(BridgeError::Transport);
        }
        self.messages
            .try_send(message)
            .map_err(|_| BridgeError::Transport)
    }
}

fn initialize() -> Value {
    json!({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "quota_widget", "title": "Quota Widget", "version": env!("CARGO_PKG_VERSION")}
    }})
}

fn quota_request(id: u64) -> Value {
    json!({"id": id, "method": "account/rateLimits/read"})
}

fn is_quota_result(value: &Value) -> bool {
    value.is_object()
        && (value.get("rateLimits").is_some_and(Value::is_object)
            || value
                .get("rateLimitsByLimitId")
                .is_some_and(Value::is_object))
}

#[derive(Clone, Copy)]
struct Pending {
    id: u64,
    since: Instant,
    epoch: u64,
}

fn run(
    mut command: Command,
    commands: &Receiver<BridgeCommand>,
    events: &Sender<BridgeEvent>,
    stop_requested: &AtomicBool,
    timing: Timing,
) -> Result<(), BridgeError> {
    // Discovery may have been underway when the UI asked to close/reconnect.
    if stop_requested.load(Ordering::Acquire) {
        return Ok(());
    }
    let work = WorkingDirectory::create()?;
    command
        .current_dir(&work.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    if stop_requested.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut child = OwnedChild(command.spawn().map_err(|_| BridgeError::StartFailed)?);
    if stop_requested.load(Ordering::Acquire) {
        return Ok(());
    }
    let input = child.0.stdin.take().ok_or(BridgeError::Transport)?;
    let output = child.0.stdout.take().ok_or(BridgeError::Transport)?;
    let (line_tx, lines) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
    let overflow = Arc::new(AtomicBool::new(false));
    let reader_overflow = Arc::clone(&overflow);
    // The pipe reader owns no process. It exits on EOF or when its bounded
    // queue is disconnected; it cannot make stop wait on a malformed stream.
    let _reader = thread::Builder::new()
        .name("quota-protocol-reader".into())
        .spawn(move || read_frames(output, line_tx, reader_overflow))
        .map_err(|_| BridgeError::StartFailed)?;
    let input = PipeWriter::spawn(input)?;
    input.send(initialize())?;
    let mut pending = Some(Pending {
        id: 1,
        since: Instant::now(),
        epoch: 0,
    });
    let mut initialized = false;
    let mut epoch = 0u64;
    let mut next_id = 2u64;
    let mut refresh_pending = true;
    let mut last_sent = Instant::now();
    let mut next_poll = last_sent + timing.poll;
    loop {
        if stop_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        // Bound each batch so a flood of Refresh cannot starve Stop or I/O.
        for _ in 0..256 {
            match commands.try_recv() {
                Ok(BridgeCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Ok(BridgeCommand::Refresh) => refresh_pending = true,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if overflow.load(Ordering::Acquire) {
            return Err(BridgeError::Protocol);
        }
        if input.failed.load(Ordering::Acquire) {
            return Err(BridgeError::Transport);
        }
        if let Some(request) = pending {
            if request.since.elapsed() >= timing.timeout {
                return Err(BridgeError::Timeout);
            }
        }
        if initialized
            && pending.is_none()
            && (Instant::now() >= next_poll || refresh_pending)
            && last_sent.elapsed() >= timing.refresh_floor
        {
            input.send(quota_request(next_id))?;
            last_sent = Instant::now();
            pending = Some(Pending {
                id: next_id,
                since: last_sent,
                epoch,
            });
            next_id += 1;
            refresh_pending = false;
            next_poll = last_sent + timing.poll;
        }
        match lines.recv_timeout(LOOP_INTERVAL) {
            Ok(Incoming::Invalid) => return Err(BridgeError::Protocol),
            Ok(Incoming::Json(message)) => {
                if !message.is_object() {
                    return Err(BridgeError::Protocol);
                }
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    // Never fulfill server requests for tokens, tools or approvals.
                    if message.get("id").is_some() {
                        return Err(BridgeError::UnsupportedRequest);
                    }
                    match method {
                        "account/updated" => {
                            epoch = epoch.wrapping_add(1);
                            refresh_pending = true;
                        }
                        "account/rateLimits/updated" => refresh_pending = true,
                        _ => {}
                    }
                    continue;
                }
                let Some(request) = pending else { continue };
                if message.get("id").and_then(Value::as_u64) != Some(request.id) {
                    continue;
                }
                pending = None;
                if message.get("error").is_some() {
                    if !initialized {
                        return Err(BridgeError::Protocol);
                    }
                    let _ = events.send(BridgeEvent::Error(BridgeError::Unavailable));
                    continue;
                }
                let Some(result) = message.get("result") else {
                    return Err(BridgeError::Protocol);
                };
                if !initialized {
                    if !result.is_object() {
                        return Err(BridgeError::Protocol);
                    }
                    input.send(json!({"method": "initialized", "params": {}}))?;
                    initialized = true;
                    let _ = events.send(BridgeEvent::Connected);
                    // First quota read need not wait for the refresh click floor.
                    last_sent = Instant::now()
                        .checked_sub(timing.refresh_floor)
                        .unwrap_or_else(Instant::now);
                } else if request.epoch == epoch {
                    if is_quota_result(result) {
                        let _ = events.send(BridgeEvent::RateLimits(result.clone()));
                    } else {
                        let _ = events.send(BridgeEvent::Error(BridgeError::Unavailable));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if child
                    .0
                    .try_wait()
                    .map_err(|_| BridgeError::Transport)?
                    .is_some()
                {
                    return Err(BridgeError::Transport);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(BridgeError::Transport),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_methods_are_exactly_read_only() {
        let init = initialize();
        assert_eq!(init["method"], "initialize");
        assert!(init["params"].get("capabilities").is_none());
        let request = quota_request(7);
        assert_eq!(
            request,
            json!({"id": 7, "method": "account/rateLimits/read"})
        );
        assert!(request.get("params").is_none());
    }

    #[test]
    fn paths_reject_protected_locations_and_launchers() {
        for path in [
            r"C:\Program Files\WindowsApps\Codex\codex.exe",
            "/tmp/WindowsApps/codex",
            r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\codex.exe",
        ] {
            assert!(protected_path(Path::new(path)));
            assert_eq!(
                validate_executable(Path::new(path)),
                Err(BridgeError::ProtectedPath)
            );
        }
        for path in [
            "codex.cmd",
            "CODEX.BAT",
            "codex.ps1",
            "codex.js",
            "codex.py",
        ] {
            assert!(script_path(Path::new(path)));
        }
        assert_eq!(
            validate_executable(Path::new("codex")),
            Err(BridgeError::AbsolutePathRequired)
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_header_rejects_extensionless_scripts_and_symlinks_to_scripts() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = WorkingDirectory::create().unwrap();
        let script = directory.0.join("codex");
        fs::write(&script, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            validate_executable(&script),
            Err(BridgeError::NativeExecutableRequired)
        );
        let alias = directory.0.join("alias");
        symlink(&script, &alias).unwrap();
        assert_eq!(
            validate_executable(&alias),
            Err(BridgeError::NativeExecutableRequired)
        );
        fs::remove_file(alias).unwrap();
        fs::remove_file(script).unwrap();
    }

    #[test]
    fn discovers_current_and_legacy_npm_native_layouts_without_running_npm() {
        let mut candidates = Vec::new();
        npm_candidates(Path::new("/synthetic/node_modules"), &mut candidates);
        if let Some((triple, package)) = npm_target() {
            assert!(candidates.contains(
                &Path::new("/synthetic/node_modules/@openai")
                    .join(package)
                    .join("vendor")
                    .join(triple)
                    .join("bin")
                    .join(binary_name())
            ));
            assert!(candidates.contains(
                &Path::new("/synthetic/node_modules/@openai/codex/vendor")
                    .join(triple)
                    .join("codex")
                    .join(binary_name())
            ));
        }
    }

    #[test]
    fn bounded_reader_rejects_large_and_malformed_frames_without_error_text() {
        for bytes in [
            b"not-json-secret\n".to_vec(),
            vec![b'x'; MAX_FRAME_BYTES + 10],
        ] {
            let (tx, rx) = mpsc::sync_channel(2);
            let overflow = Arc::new(AtomicBool::new(false));
            read_frames(bytes.as_slice(), tx, overflow);
            assert!(matches!(rx.recv().unwrap(), Incoming::Invalid));
        }
    }

    #[test]
    fn bounded_reader_detects_queue_flood() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let overflow = Arc::new(AtomicBool::new(false));
        read_frames(b"{}\n{}\n{}\n".as_slice(), tx, Arc::clone(&overflow));
        assert!(overflow.load(Ordering::Acquire));
    }

    #[test]
    fn quota_result_requires_a_source_bucket() {
        assert!(is_quota_result(&json!({"rateLimits": {"primary": null}})));
        assert!(is_quota_result(&json!({"rateLimitsByLimitId": {}})));
        assert!(!is_quota_result(&json!({"rateLimits": null})));
        assert!(!is_quota_result(&json!({"unexpected": 100})));
    }

    #[test]
    fn missing_worker_handle_is_finished_and_repeated_stop_is_safe() {
        let (commands, _) = mpsc::channel();
        let (_, events) = mpsc::channel();
        let mut bridge = Bridge {
            commands,
            events,
            worker: None,
            stop_requested: Arc::new(AtomicBool::new(false)),
        };
        assert!(bridge.is_finished());
        bridge.request_stop();
        bridge.request_stop();
        bridge.stop();
        bridge.stop();
        assert!(bridge.is_finished());
    }

    #[test]
    fn blocked_pipe_writes_have_bounded_nonblocking_backpressure() {
        struct BlockedWriter {
            entered: Sender<()>,
            release: Receiver<()>,
        }
        impl Write for BlockedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                let _ = self.release.recv();
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (entered, writing) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = PipeWriter::spawn(BlockedWriter {
            entered,
            release: blocked,
        })
        .unwrap();
        writer.send(initialize()).unwrap();
        writing.recv_timeout(Duration::from_secs(1)).unwrap();
        for id in 0..WRITE_QUEUE_CAPACITY {
            writer.send(quota_request(id as u64)).unwrap();
        }
        let start = Instant::now();
        let outcome = writer.send(quota_request(100));
        let elapsed = start.elapsed();
        release.send(()).unwrap();
        assert_eq!(outcome, Err(BridgeError::Transport));
        assert!(elapsed < Duration::from_millis(100));
    }

    #[test]
    fn stop_during_command_preparation_is_nonblocking_and_never_spawns() {
        let (entered, preparing) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let mut bridge = Bridge::spawn_worker(
            move || {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                // Deliberately nonexistent: reaching spawn would report StartFailed.
                Ok(Command::new("/synthetic/no-cli-may-be-started"))
            },
            Timing::default(),
        );
        preparing.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        bridge.request_stop();
        bridge.request_stop();
        let elapsed = start.elapsed();
        let finished_while_preparing = bridge.is_finished();
        release.send(()).unwrap();
        bridge.stop();
        assert!(elapsed < Duration::from_millis(100));
        assert!(!finished_while_preparing);
        assert!(bridge.is_finished());
        assert!(bridge
            .events
            .try_iter()
            .all(|event| !matches!(event, BridgeEvent::Error(_))));
    }

    #[cfg(unix)]
    struct Mock {
        bridge: Bridge,
        directory: WorkingDirectory,
        transcript: PathBuf,
        pidfile: PathBuf,
    }

    #[cfg(unix)]
    impl Mock {
        fn spawn(mode: &str, timeout: Duration, poll: Duration) -> Self {
            let directory = WorkingDirectory::create().unwrap();
            let transcript = directory.0.join("requests.jsonl");
            let pidfile = directory.0.join("pid");
            let mode = mode.to_owned();
            let transcript_arg = transcript.clone();
            let pidfile_arg = pidfile.clone();
            let bridge = Bridge::spawn_worker(
                move || {
                    // Tests alone bypass native CLI discovery, invoking the trusted
                    // interpreter with our synthetic fixture rather than a real CLI.
                    let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
                        .into_iter()
                        .find(|path| Path::new(path).is_file())
                        .unwrap();
                    let mut command = Command::new(python);
                    command
                        .arg("-u")
                        .arg(concat!(
                            env!("CARGO_MANIFEST_DIR"),
                            "/tests/fixtures/mock_app_server.py"
                        ))
                        .arg(mode)
                        .arg(transcript_arg)
                        .arg(pidfile_arg);
                    Ok(command)
                },
                Timing {
                    poll,
                    timeout,
                    refresh_floor: Duration::from_millis(15),
                },
            );
            Self {
                bridge,
                directory,
                transcript,
                pidfile,
            }
        }

        fn next_quota(&self) -> Value {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                match self
                    .bridge
                    .events
                    .recv_timeout(Duration::from_millis(500))
                    .unwrap()
                {
                    BridgeEvent::RateLimits(value) => return value,
                    BridgeEvent::Error(error) => panic!("unexpected redacted error: {error}"),
                    _ => {}
                }
            }
            panic!("mock quota timeout")
        }

        fn requests(&self) -> Vec<Value> {
            fs::read_to_string(&self.transcript)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        fn wait_error(&self) -> BridgeError {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let BridgeEvent::Error(error) = self
                    .bridge
                    .events
                    .recv_timeout(Duration::from_millis(500))
                    .unwrap()
                {
                    return error;
                }
            }
            panic!("mock error timeout")
        }

        fn wait_request(&self, method: &str) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if fs::read_to_string(&self.transcript)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|request| request["method"] == method)
                {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("mock request timeout");
        }

        fn assert_reaped(&self) {
            #[cfg(target_os = "linux")]
            {
                let pid = fs::read_to_string(&self.pidfile).unwrap();
                assert!(
                    !Path::new("/proc").join(pid.trim()).exists(),
                    "mock child was not reaped"
                );
            }
        }

        fn stop_responsively(&mut self) {
            let start = Instant::now();
            self.bridge.request_stop();
            self.bridge.request_stop();
            assert!(start.elapsed() < Duration::from_millis(100));
            while !self.bridge.is_finished() && start.elapsed() < Duration::from_secs(1) {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(self.bridge.is_finished(), "bridge cleanup took too long");
            self.bridge.stop();
            self.bridge.stop();
            assert!(self.bridge.is_finished());
            self.assert_reaped();
        }
    }

    #[cfg(unix)]
    impl Drop for Mock {
        fn drop(&mut self) {
            self.bridge.stop();
            let _ = fs::remove_file(&self.transcript);
            let cwdfile = self.pidfile.with_extension("cwd");
            if let Ok(cwd) = fs::read_to_string(&cwdfile) {
                assert!(
                    !Path::new(&cwd).exists(),
                    "bridge working directory was not removed"
                );
            }
            let _ = fs::remove_file(cwdfile);
            let _ = fs::remove_file(&self.pidfile);
            // This directory contains only the test-owned fixture output.
            assert!(self.directory.0.read_dir().unwrap().next().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn mock_handshake_read_and_stop_reaps_only_owned_child() {
        let mut unrelated = OwnedChild(
            Command::new("/usr/bin/python3")
                .args(["-c", "import time; time.sleep(60)"])
                .spawn()
                .unwrap(),
        );
        let mut mock = Mock::spawn("normal", Duration::from_secs(2), Duration::from_secs(30));
        let value = mock.next_quota();
        assert_eq!(value["rateLimits"]["credits"]["balance"], "12.3456");
        mock.bridge.stop();
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "an unrelated process was terminated"
        );
        let cwd = fs::read_to_string(mock.pidfile.with_extension("cwd")).unwrap();
        assert!(Path::new(&cwd)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("quota-widget-bridge-"));
        assert!(!Path::new(&cwd).exists());
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[1]["method"], "initialized");
        assert_eq!(requests[2]["method"], "account/rateLimits/read");
        mock.assert_reaped();
    }

    #[cfg(unix)]
    #[test]
    fn refreshes_coalesce_while_request_is_in_flight() {
        let mock = Mock::spawn("coalesce", Duration::from_secs(2), Duration::from_secs(30));
        loop {
            if matches!(
                mock.bridge
                    .events
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap(),
                BridgeEvent::Connected
            ) {
                break;
            }
        }
        // Wait until the initial read has been submitted, then queue clicks.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if fs::read_to_string(&mock.transcript)
                .unwrap_or_default()
                .contains("account/rateLimits/read")
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..100 {
            mock.bridge.refresh();
        }
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 1);
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 2);
        thread::sleep(Duration::from_millis(120));
        let reads = mock
            .requests()
            .iter()
            .filter(|value| value["method"] == "account/rateLimits/read")
            .count();
        assert_eq!(reads, 2);
    }

    #[cfg(unix)]
    #[test]
    fn sparse_notifications_trigger_full_reads_and_are_never_observations() {
        let mock = Mock::spawn("sparse", Duration::from_secs(2), Duration::from_secs(30));
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 1);
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn account_update_refreshes_rates_without_claiming_identity_changed() {
        let mock = Mock::spawn("switch", Duration::from_secs(2), Duration::from_secs(30));
        loop {
            match mock
                .bridge
                .events
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
            {
                BridgeEvent::RateLimits(value) => {
                    assert_eq!(value["rateLimits"]["primary"]["usedPercent"], 2);
                    break;
                }
                BridgeEvent::Error(error) => panic!("unexpected redacted error: {error}"),
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn polling_runs_without_refresh_commands() {
        let mock = Mock::spawn("normal", Duration::from_secs(2), Duration::from_millis(80));
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 1);
        assert_eq!(mock.next_quota()["rateLimits"]["primary"]["usedPercent"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn mock_errors_are_redacted_and_unsupported_requests_are_not_fulfilled() {
        for (mode, expected) in [
            ("error", BridgeError::Unavailable),
            ("malformed", BridgeError::Protocol),
            ("oversized", BridgeError::Protocol),
            ("server_request", BridgeError::UnsupportedRequest),
        ] {
            let mock = Mock::spawn(mode, Duration::from_secs(2), Duration::from_secs(30));
            let error = mock.wait_error();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("secret"));
            assert!(mock.requests().iter().all(|request| matches!(
                request["method"].as_str(),
                Some("initialize" | "initialized" | "account/rateLimits/read")
            )));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_quota_keeps_bridge_alive_and_refresh_retries_the_read() {
        let mut mock = Mock::spawn("error", Duration::from_secs(2), Duration::from_secs(30));
        assert_eq!(mock.wait_error(), BridgeError::Unavailable);
        assert!(!mock.bridge.is_finished());
        mock.bridge.commands.send(BridgeCommand::Refresh).unwrap();
        assert_eq!(mock.wait_error(), BridgeError::Unavailable);
        assert!(!mock.bridge.is_finished());
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|request| request["method"] == "account/rateLimits/read")
                .count(),
            2
        );
        mock.stop_responsively();
    }

    #[cfg(unix)]
    #[test]
    fn terminal_transport_errors_disconnect_refresh_sender_after_reaping() {
        for (mode, expected) in [
            ("timeout", BridgeError::Timeout),
            ("read_timeout", BridgeError::Timeout),
            ("malformed", BridgeError::Protocol),
            ("server_request", BridgeError::UnsupportedRequest),
        ] {
            let mut mock = Mock::spawn(mode, Duration::from_millis(160), Duration::from_secs(30));
            assert_eq!(mock.wait_error(), expected);
            // Errors are emitted only after run() has returned and reaped its
            // child. Join completes the epilogue without requesting a retry.
            mock.bridge.worker.take().unwrap().join().unwrap();
            assert!(mock.bridge.is_finished());
            assert!(mock.bridge.commands.send(BridgeCommand::Refresh).is_err());
            assert!(mock
                .bridge
                .events
                .try_iter()
                .any(|event| matches!(event, BridgeEvent::Stopped)));
            mock.assert_reaped();
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_also_applies_to_quota_reads_after_handshake() {
        let mock = Mock::spawn(
            "read_timeout",
            Duration::from_millis(160),
            Duration::from_secs(30),
        );
        assert_eq!(mock.wait_error(), BridgeError::Timeout);
    }

    #[cfg(unix)]
    #[test]
    fn mock_timeout_and_manual_stop_are_bounded() {
        let mut mock = Mock::spawn(
            "timeout",
            Duration::from_millis(160),
            Duration::from_secs(30),
        );
        assert_eq!(mock.wait_error(), BridgeError::Timeout);
        mock.bridge.stop();
        mock.assert_reaped();
        for (mode, method) in [
            ("timeout", "initialize"),
            ("read_timeout", "account/rateLimits/read"),
        ] {
            let mut mock = Mock::spawn(mode, Duration::from_secs(20), Duration::from_secs(30));
            mock.wait_request(method);
            mock.stop_responsively();
        }
    }

    #[cfg(unix)]
    #[test]
    fn stop_while_waiting_for_poll_reaps_child() {
        let mut mock = Mock::spawn("normal", Duration::from_secs(20), Duration::from_secs(30));
        mock.next_quota();
        mock.stop_responsively();
    }

    #[cfg(unix)]
    #[test]
    fn stop_takes_priority_over_refresh_backlog() {
        let mut mock = Mock::spawn("timeout", Duration::from_secs(20), Duration::from_secs(30));
        mock.wait_request("initialize");
        for _ in 0..100_000 {
            mock.bridge.refresh();
        }
        mock.stop_responsively();
    }
}
