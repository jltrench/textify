//! OCR engine: orchestrates slurp -> grim -> magick -> tesseract -> wl-copy.
//!
//! All external tools are resolved from PATH and invoked with fixed argument
//! lists via `Command` (never a shell), so no user input can inject commands.
//! Temp files are created inside a private directory under $XDG_RUNTIME_DIR
//! (or /tmp) and removed on every exit path.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::Serialize;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const MAX_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_LANGUAGE_BYTES: usize = 32;

const MAX_COMMAND_STDERR_BYTES: usize = 32 * 1024;
const MAX_REGION_OUTPUT_BYTES: usize = 128;
const MAX_LANGUAGE_COUNT: usize = 128;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u64 = 10_000;
const MAX_IMAGE_PIXELS: u64 = 50_000_000;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TSV_BYTES: usize = 256 * 1024;
const SELECT_TIMEOUT: Duration = Duration::from_secs(120);
const SHORT_PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const OCR_PROCESS_TIMEOUT: Duration = Duration::from_secs(45);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_PROCESS_MEMORY_BYTES: u64 = 768 * 1024 * 1024;
const MAX_PROCESS_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROCESS_OPEN_FILES: u64 = 128;

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// RAII guard for a file in a private temp directory. Creating the directory
/// atomically avoids the predictable-name/symlink race that is possible when
/// a file is created directly under a world-writable directory such as /tmp.
struct TempFile {
    path: PathBuf,
    dir: PathBuf,
}

impl TempFile {
    fn new(prefix: &str, ext: &str) -> Result<Self, String> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        for base in temp_dirs() {
            for attempt in 0..32u32 {
                let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
                let dir = base.join(format!("textify-{prefix}-{pid}-{nonce}-{serial}-{attempt}"));
                match create_private_dir(&dir) {
                    Ok(()) => {
                        return Ok(TempFile {
                            path: dir.join(format!("output.{ext}")),
                            dir,
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::ReadOnlyFilesystem
                        ) =>
                    {
                        break
                    }
                    Err(error) => {
                        return Err(format!(
                            "cannot create secure temp directory {}: {error}",
                            dir.display()
                        ));
                    }
                }
            }
        }

        Err("could not create a unique secure temp directory".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().mode(0o700).create(path)
    }

    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn read_bounded<R: Read>(
    mut reader: R,
    max_bytes: usize,
    output_limited: Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(max_bytes.min(8192));
    let mut buffer = [0u8; 8192];

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) > max_bytes {
            output_limited.store(true, Ordering::Relaxed);
            break;
        }
        output.extend_from_slice(&buffer[..count]);
    }

    Ok(output)
}

fn command_name(command: &Path) -> String {
    command
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn command_failure(command: &Path, status: ExitStatus, stderr: &[u8]) -> String {
    let diagnostic = String::from_utf8_lossy(stderr).trim().to_string();
    format!(
        "{} exited with {}: {}",
        command_name(command),
        status,
        if diagnostic.is_empty() {
            "no error output".into()
        } else {
            diagnostic
        }
    )
}

#[cfg(unix)]
fn set_process_limit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: setrlimit only reads the local limit structure and is safe in
    // the child between fork and exec.
    if unsafe { libc::setrlimit(resource, &limit) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn apply_process_limits(command: &mut Command, timeout: Duration) {
    let cpu_seconds = timeout.as_secs().saturating_add(1).max(1);
    // SAFETY: the closure only calls async-signal-safe libc limit functions
    // before the child replaces itself with the requested executable.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            set_process_limit(libc::RLIMIT_CPU, cpu_seconds)?;
            set_process_limit(libc::RLIMIT_AS, MAX_PROCESS_MEMORY_BYTES)?;
            set_process_limit(libc::RLIMIT_FSIZE, MAX_PROCESS_FILE_BYTES)?;
            set_process_limit(libc::RLIMIT_NOFILE, MAX_PROCESS_OPEN_FILES)?;
            set_process_limit(libc::RLIMIT_CORE, 0)?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_process_limits(_command: &mut Command, _timeout: Duration) {}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        // The helper may start ImageMagick/Tesseract worker processes. Kill
        // the private process group first so a timeout cannot leave a worker
        // behind after the direct child is reaped.
        let pid = child.id() as libc::pid_t;
        // SAFETY: the negative pid targets only the process group created for
        // this child in apply_process_limits.
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Run a fixed-argument child with bounded pipes and a hard deadline.
///
/// Reader threads are used for both pipes so a noisy child cannot deadlock on
/// a full stderr pipe while stdout is being consumed. Once either pipe or the
/// deadline is exceeded, the child is terminated and no unbounded output is
/// retained.
fn run_command(
    command: &Path,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<CommandOutput, String> {
    let mut builder = Command::new(command);
    builder
        .args(args)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_process_limits(&mut builder, timeout);
    let mut child = builder
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", command.display()))?;

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            let _ = child.wait();
            return Err(format!("{} stdout unavailable", command_name(command)));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child);
            let _ = child.wait();
            return Err(format!("{} stderr unavailable", command_name(command)));
        }
    };
    let output_limited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_limited = Arc::clone(&output_limited);
    let stderr_limited = Arc::clone(&output_limited);
    let stdout_thread = thread::spawn(move || read_bounded(stdout, max_stdout, stdout_limited));
    let stderr_thread = thread::spawn(move || read_bounded(stderr, max_stderr, stderr_limited));

    let stdin = match (stdin_data, child.stdin.take()) {
        (Some(_), Some(stdin)) => Some(stdin),
        (None, None) => None,
        (Some(_), None) => {
            terminate_child(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(format!("{} stdin unavailable", command_name(command)));
        }
        (None, Some(_)) => {
            terminate_child(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(format!(
                "{} received an unexpected stdin pipe",
                command_name(command)
            ));
        }
    };
    let stdin_thread = match (stdin, stdin_data) {
        (Some(mut stdin), Some(data)) => {
            let data = data.to_vec();
            Some(thread::spawn(move || stdin.write_all(&data)))
        }
        (None, None) => None,
        _ => {
            terminate_child(&mut child);
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return Err(format!("{} stdin setup failed", command_name(command)));
        }
    };

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if output_limited.load(Ordering::Relaxed) {
                    terminate_child(&mut child);
                    break child.wait().map_err(|error| {
                        format!("cannot reap {}: {error}", command_name(command))
                    })?;
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    terminate_child(&mut child);
                    break child.wait().map_err(|error| {
                        format!("cannot reap {}: {error}", command_name(command))
                    })?;
                }
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
            Err(error) => {
                terminate_child(&mut child);
                let _ = child.wait();
                return Err(format!(
                    "cannot wait for {}: {error}",
                    command_name(command)
                ));
            }
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| format!("{} stdout reader panicked", command_name(command)))?
        .map_err(|error| format!("cannot read {} stdout: {error}", command_name(command)))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| format!("{} stderr reader panicked", command_name(command)))?
        .map_err(|error| format!("cannot read {} stderr: {error}", command_name(command)))?;
    if let Some(stdin_thread) = stdin_thread {
        let _ = stdin_thread.join();
    }

    if timed_out {
        return Err(format!("{} timed out", command_name(command)));
    }
    if output_limited.load(Ordering::Relaxed) {
        return Err(format!(
            "{} output exceeded its limit",
            command_name(command)
        ));
    }

    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

/// Run a stdin-only helper whose normal behavior is to daemonize after
/// receiving its payload (notably `wl-copy`). Its descendants can inherit the
/// parent's stdout/stderr descriptors, so piping those streams would make
/// reader threads wait for EOF forever after the direct child exits.
fn run_stdin_only(
    command: &Path,
    args: &[&str],
    stdin_data: &[u8],
    timeout: Duration,
) -> Result<ExitStatus, String> {
    let mut builder = Command::new(command);
    builder
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_process_limits(&mut builder, timeout);
    let mut child = builder
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", command.display()))?;

    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut child);
            let _ = child.wait();
            return Err(format!("{} stdin unavailable", command_name(command)));
        }
    };
    let data = stdin_data.to_vec();
    let stdin_thread = thread::spawn(move || stdin.write_all(&data));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                terminate_child(&mut child);
                break child
                    .wait()
                    .map_err(|error| format!("cannot reap {}: {error}", command_name(command)))?;
            }
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
            Err(error) => {
                terminate_child(&mut child);
                let _ = child.wait();
                return Err(format!(
                    "cannot wait for {}: {error}",
                    command_name(command)
                ));
            }
        }
    };

    let _ = stdin_thread.join();
    if timed_out {
        return Err(format!("{} timed out", command_name(command)));
    }
    Ok(status)
}

fn run_text(
    command: &Path,
    args: &[&str],
    timeout: Duration,
    max_stdout: usize,
) -> Result<String, String> {
    let output = run_command(
        command,
        args,
        None,
        timeout,
        max_stdout,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(command, output.status, &output.stderr));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| format!("{} returned invalid UTF-8", command_name(command)))
}

fn validate_text_bytes(text: &str, field: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "{field} exceeds the {} KiB limit",
            MAX_TEXT_BYTES / 1024
        ));
    }
    Ok(())
}

pub fn read_stdin_limited() -> Result<String, String> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut output = Vec::with_capacity(MAX_TEXT_BYTES.min(8192));
    let mut buffer = [0u8; 8192];

    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read clipboard text: {error}"))?;
        if count == 0 {
            break;
        }
        if output.len().saturating_add(count) > MAX_TEXT_BYTES {
            return Err(format!(
                "clipboard text exceeds the {} KiB limit",
                MAX_TEXT_BYTES / 1024
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }

    String::from_utf8(output).map_err(|_| "clipboard text must be valid UTF-8".into())
}

#[derive(Serialize)]
pub struct OcrResult {
    pub text: String,
    pub lang: String,
    pub copied: bool,
    pub source: String,
    pub confidence: f64,
}

#[derive(Clone)]
pub struct OcrOptions {
    pub region: bool,
    pub file: Option<String>,
    pub lang: String,
    pub copy: bool,
    pub json: bool,
}

impl Default for OcrOptions {
    fn default() -> Self {
        OcrOptions {
            region: true,
            file: None,
            lang: String::new(),
            copy: true,
            json: false,
        }
    }
}

fn which(name: &str) -> Result<PathBuf, String> {
    let path = env::var_os("PATH").unwrap_or_default();
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("required tool not found on PATH: {name}"))
}

fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
    {
        dirs.push(runtime_dir);
    }

    let fallback = PathBuf::from("/tmp");
    if !dirs.iter().any(|path| path == &fallback) {
        dirs.push(fallback);
    }
    dirs
}

/// Resolve the tesseract language list to a `-l` argument. If the requested
/// language is not installed, fall back to `eng` so the command still works.
pub fn list_langs() -> Result<Vec<String>, String> {
    let tesseract = which("tesseract")?;
    let text = run_text(
        &tesseract,
        &["--list-langs"],
        SHORT_PROCESS_TIMEOUT,
        16 * 1024,
    )?;
    let mut langs = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        // Language codes are short tokens (e.g. "eng", "por", "osd"); skip the
        // header lines tesseract prints ("List of available languages...").
        if !l.is_empty() && l.len() <= MAX_LANGUAGE_BYTES && !l.contains(' ') && !l.contains('"') {
            if langs.len() >= MAX_LANGUAGE_COUNT {
                return Err("tesseract returned too many language fields".into());
            }
            langs.push(l.to_string());
        }
    }
    Ok(langs)
}

/// Map a keyboard layout / locale to the closest tesseract language code.
/// Layouts are keyboard layouts (e.g. "br", "us"), not the language of the
/// text on screen, so this is a best-effort heuristic with a safe fallback.
fn layout_to_lang(layout: &str) -> &'static str {
    let l = layout.to_lowercase();
    let l = l.trim();
    // Strip common suffixes like "(Brazil)" or " (Portugal)".
    let base = l.split(['(', ' ']).next().unwrap_or(l);
    match base {
        "br" | "pt" | "pt_br" | "pt-br" | "brazil" | "portuguese" | "portugal" => "por",
        "es" | "spa" | "spanish" | "mexico" | "latin" => "spa",
        "fr" | "french" => "fra",
        "de" | "german" => "deu",
        "it" | "italian" => "ita",
        "nl" | "dutch" => "nld",
        "ru" | "russian" => "rus",
        "ja" | "japanese" => "jpn",
        "zh" | "cn" | "chinese" => "chi_sim",
        "ko" | "korean" => "kor",
        "ar" | "arabic" => "ara",
        "hi" | "hindi" => "hin",
        "pl" | "polish" => "pol",
        "tr" | "turkish" => "tur",
        "sv" | "swedish" => "swe",
        "da" | "danish" => "dan",
        "no" | "norwegian" => "nor",
        "fi" | "finnish" => "fin",
        "cs" | "czech" => "ces",
        "hu" | "hungarian" => "hun",
        "ro" | "romanian" => "ron",
        "el" | "greek" => "ell",
        "he" | "hebrew" => "heb",
        "th" | "thai" => "tha",
        "vi" | "vietnamese" => "vie",
        "uk" | "ukrainian" => "ukr",
        _ => "eng",
    }
}

/// Detect the active keyboard layout. Tries Hyprland first (most reliable on
/// Omarchy), then fcitx5, then falls back to "us".
fn detect_layout() -> String {
    // 1. Hyprland: `hyprctl getoption input:kb_layout` prints "str: br".
    if let Ok(hyprctl) = which("hyprctl") {
        if let Ok(text) = run_text(
            &hyprctl,
            &["getoption", "input:kb_layout"],
            SHORT_PROCESS_TIMEOUT,
            4096,
        ) {
            for line in text.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("str:") {
                    let v = v.trim();
                    if !v.is_empty() && v.len() <= MAX_LANGUAGE_BYTES {
                        return v.to_string();
                    }
                }
            }
        }
    }

    // 2. fcitx5: `fcitx5-remote -n` prints the current input method name,
    //    e.g. "keyboard-us" or "keyboard-br".
    if let Ok(fcitx) = which("fcitx5-remote") {
        if let Ok(name) = run_text(&fcitx, &["-n"], SHORT_PROCESS_TIMEOUT, 4096) {
            let name = name.trim();
            if let Some(idx) = name.find("keyboard-") {
                let layout = &name[idx + "keyboard-".len()..];
                if !layout.is_empty() && layout.len() <= MAX_LANGUAGE_BYTES {
                    return layout.to_string();
                }
            }
        }
    }

    "us".to_string()
}

/// Resolve the effective OCR language: use the requested one if installed,
/// otherwise detect from the active keyboard layout, otherwise fall back to
/// whatever is installed (preferring eng).
pub fn resolve_lang(requested: &str) -> Result<String, String> {
    if requested.len() > MAX_LANGUAGE_BYTES {
        return Err("requested language exceeds its field limit".into());
    }
    let langs = list_langs()?;
    if !requested.is_empty() && langs.iter().any(|l| l == requested) {
        return Ok(requested.to_string());
    }
    // Detect from the active keyboard layout.
    let detected = layout_to_lang(&detect_layout());
    if langs.iter().any(|l| l == detected) {
        return Ok(detected.to_string());
    }
    if langs.iter().any(|l| l == "eng") {
        return Ok("eng".into());
    }
    Err("no usable tesseract language installed (need at least 'eng')".into())
}

/// Report the detected layout and the language that will be used.
pub fn detect_lang() -> Result<(String, String), String> {
    let layout = detect_layout();
    let lang = resolve_lang("")?;
    Ok((layout, lang))
}

fn path_string(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "path must be valid UTF-8".to_string())?;
    if value.len() > MAX_PATH_BYTES {
        return Err("path exceeds the 4 KiB limit".into());
    }
    Ok(value.to_string())
}

fn validate_region(region: &str) -> Result<(), String> {
    if region.len() > MAX_REGION_OUTPUT_BYTES {
        return Err("screen selection exceeded its field limit".into());
    }
    let mut fields = region.split_whitespace();
    let origin = fields.next().ok_or("screen selection is empty")?;
    let size = fields.next().ok_or("screen selection has no size")?;
    if fields.next().is_some() {
        return Err("screen selection has unexpected fields".into());
    }

    let (x, y) = origin
        .split_once(',')
        .ok_or("screen selection has an invalid origin")?;
    let (width, height) = size
        .split_once('x')
        .ok_or("screen selection has an invalid size")?;
    let x = x
        .parse::<i64>()
        .map_err(|_| "invalid selection x coordinate")?;
    let y = y
        .parse::<i64>()
        .map_err(|_| "invalid selection y coordinate")?;
    let width = width
        .parse::<u64>()
        .map_err(|_| "invalid selection width")?;
    let height = height
        .parse::<u64>()
        .map_err(|_| "invalid selection height")?;

    if x.unsigned_abs() > 100_000 || y.unsigned_abs() > 100_000 {
        return Err("screen selection coordinates are out of bounds".into());
    }
    validate_image_dimensions(width, height)
}

fn validate_image_dimensions(width: u64, height: u64) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("image has an empty dimension".into());
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err("image dimensions exceed the 10000px limit".into());
    }
    if width.saturating_mul(height) > MAX_IMAGE_PIXELS {
        return Err("image area exceeds the 50 megapixel limit".into());
    }
    Ok(())
}

fn image_dimensions(path: &Path) -> Result<(u64, u64), String> {
    let identify = which("identify")?;
    let path = path_string(path)?;
    let output = run_command(
        &identify,
        &["-format", "%w %h", path.as_str()],
        None,
        IMAGE_PROCESS_TIMEOUT,
        128,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&identify, output.status, &output.stderr));
    }

    let dimensions = String::from_utf8(output.stdout)
        .map_err(|_| "identify returned invalid UTF-8".to_string())?;
    let mut fields = dimensions.split_whitespace();
    let width = fields
        .next()
        .ok_or("identify returned no image width")?
        .parse::<u64>()
        .map_err(|_| "identify returned an invalid image width")?;
    let height = fields
        .next()
        .ok_or("identify returned no image height")?
        .parse::<u64>()
        .map_err(|_| "identify returned an invalid image height")?;
    validate_image_dimensions(width, height)?;
    Ok((width, height))
}

fn validate_image_file(path: &Path) -> Result<(u64, u64), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("cannot inspect image: {error}"))?;
    if !metadata.is_file() {
        return Err("image path is not a regular file".into());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err("image file exceeds the 64 MiB limit".into());
    }

    image_dimensions(path)
}

fn capture_region() -> Result<TempFile, String> {
    let slurp = which("slurp")?;
    let grim = which("grim")?;
    let png = TempFile::new("region", "png")?;

    // slurp prints "x,y WxH" (e.g. "100,200 800x600") to stdout.
    let region = run_text(&slurp, &[], SELECT_TIMEOUT, MAX_REGION_OUTPUT_BYTES)?;
    let region = region.trim();
    if region.is_empty() {
        return Err("selection cancelled".into());
    }
    validate_region(region)?;

    let output_path = path_string(png.path())?;
    let output = run_command(
        &grim,
        &["-g", region, output_path.as_str()],
        None,
        IMAGE_PROCESS_TIMEOUT,
        1024,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&grim, output.status, &output.stderr));
    }
    validate_image_file(png.path())?;
    Ok(png)
}

fn capture_full() -> Result<TempFile, String> {
    let grim = which("grim")?;
    let png = TempFile::new("full", "png")?;
    let output_path = path_string(png.path())?;
    let output = run_command(
        &grim,
        &[output_path.as_str()],
        None,
        IMAGE_PROCESS_TIMEOUT,
        1024,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&grim, output.status, &output.stderr));
    }
    validate_image_file(png.path())?;
    Ok(png)
}

/// Pre-process the image with ImageMagick to improve OCR accuracy.
///
/// A hard threshold destroys anti-aliased text (the serrated edges of small
/// terminal glyphs), so we keep it gentle: grayscale, a modest upscale for
/// small text, and a light sharpen. Tesseract's own PSM 6 (uniform block)
/// handles the rest.
///
/// The upscale is adaptive: only small captures (e.g. a tight region) get
/// enlarged. Full-screen captures are already large, and upscaling them 3x
/// only adds noise and slows OCR down.
fn preprocess(src: &str) -> Result<TempFile, String> {
    let magick = which("magick")?;
    let out = TempFile::new("proc", "png")?;

    // Read the source dimensions so we can decide whether to upscale.
    let (w, h) = image_dimensions(Path::new(src))?;

    let mut args: Vec<String> = vec![
        "-limit".into(),
        "memory".into(),
        "128MiB".into(),
        "-limit".into(),
        "map".into(),
        "256MiB".into(),
        "-limit".into(),
        "disk".into(),
        "256MiB".into(),
        "-limit".into(),
        "area".into(),
        "50000000".into(),
        src.to_string(),
        "-colorspace".into(),
        "Gray".into(),
    ];
    // Upscale only when the capture is small (region grabs, low-res sources).
    if w > 0 && h > 0 && w < 1200 && h < 1200 {
        args.push("-resize".into());
        args.push("300%".into());
    }
    args.push("-sharpen".into());
    args.push("0x0.5".into());
    args.push(out.path().to_string_lossy().into_owned());

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let output = run_command(
        &magick,
        &arg_refs,
        None,
        IMAGE_PROCESS_TIMEOUT,
        1024,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&magick, output.status, &output.stderr));
    }
    validate_image_file(out.path())?;
    Ok(out)
}

fn ocr_image(path: &str, lang: &str) -> Result<String, String> {
    let tesseract = which("tesseract")?;
    let output = run_command(
        &tesseract,
        &[path, "stdout", "-l", lang, "--psm", "6"],
        None,
        OCR_PROCESS_TIMEOUT,
        MAX_TEXT_BYTES,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&tesseract, output.status, &output.stderr));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "tesseract returned invalid UTF-8".to_string())?;
    validate_text_bytes(&text, "OCR result")?;
    Ok(text)
}

/// Run tesseract in TSV mode and return the mean per-word confidence (0-100).
/// Low confidence is a strong signal that the region was icons/graphics rather
/// than text, so the panel can warn instead of copying garbage.
fn ocr_confidence(path: &str, lang: &str) -> Result<f64, String> {
    let tesseract = which("tesseract")?;
    let output = run_command(
        &tesseract,
        &[path, "stdout", "-l", lang, "--psm", "6", "tsv"],
        None,
        OCR_PROCESS_TIMEOUT,
        MAX_TSV_BYTES,
        MAX_COMMAND_STDERR_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_failure(&tesseract, output.status, &output.stderr));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "tesseract confidence output was not valid UTF-8".to_string())?;
    let mut sum = 0.0;
    let mut n = 0u32;
    for line in text.lines().skip(1) {
        // TSV columns: ... level page block par line word left top width height conf ...
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        if let Ok(c) = cols[10].trim().parse::<f64>() {
            if c >= 0.0 {
                sum += c;
                n += 1;
            }
        }
    }
    if n == 0 {
        Ok(0.0)
    } else {
        Ok(sum / n as f64)
    }
}

pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    validate_text_bytes(text, "clipboard text")?;
    let wl_copy = which("wl-copy")?;
    let status = run_stdin_only(&wl_copy, &[], text.as_bytes(), CLIPBOARD_TIMEOUT)?;
    if !status.success() {
        return Err(format!("wl-copy exited with {status}"));
    }
    Ok(())
}

pub fn run_ocr(opts: OcrOptions) -> Result<OcrResult, String> {
    let lang = resolve_lang(&opts.lang)?;

    // 1. Obtain a source image. Captures are owned TempFiles kept alive for
    //    the whole function (auto-cleaned on drop); user files are borrowed.
    let mut owned_files: Vec<TempFile> = Vec::new();
    let (source, owned) = if let Some(path) = &opts.file {
        let source_path = Path::new(path);
        path_string(source_path)?;
        if !source_path.is_file() {
            return Err(format!("file not found: {path}"));
        }
        validate_image_file(source_path)?;
        (path.clone(), false)
    } else if opts.region {
        let f = capture_region()?;
        let p = f.path().to_string_lossy().into_owned();
        owned_files.push(f);
        (p, true)
    } else {
        let f = capture_full()?;
        let p = f.path().to_string_lossy().into_owned();
        owned_files.push(f);
        (p, true)
    };

    // 2. Pre-process (only for captures; user files are used as-is). magick is
    //    optional: on failure we fall back to the raw capture.
    let processed = if owned {
        preprocess(&source).ok()
    } else {
        None
    };
    let ocr_input = processed
        .as_ref()
        .map(|f| f.path().to_string_lossy().into_owned())
        .unwrap_or(source.clone());

    // 3. OCR.
    let text = ocr_image(&ocr_input, &lang)?;
    let confidence = ocr_confidence(&ocr_input, &lang)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    let confidence = if confidence.is_finite() {
        confidence
    } else {
        0.0
    };

    // 4. Copy to clipboard.
    let mut copied = false;
    if opts.copy && !text.trim().is_empty() {
        copied = copy_to_clipboard(&text).is_ok();
    }

    // owned_files (and processed) are dropped here, cleaning up temp files.
    Ok(OcrResult {
        text: text.trim_end().to_string(),
        lang,
        copied,
        source: if owned { "screen".into() } else { source },
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_file_uses_a_private_directory_and_cleans_up() {
        let dir;
        {
            let temp = TempFile::new("test", "tmp").expect("private temp file");
            dir = temp.dir.clone();

            assert!(dir.is_dir());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&dir)
                    .expect("temp directory metadata")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o700);
            }
            fs::write(temp.path(), b"textify").expect("temp file write");
        }

        assert!(!dir.exists());
    }

    #[test]
    fn text_and_image_limits_are_hard() {
        let oversized = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(validate_text_bytes(&oversized, "test").is_err());
        assert!(validate_image_dimensions(10_001, 1).is_err());
        assert!(validate_image_dimensions(10_000, 5_001).is_err());
        assert!(validate_region("0,0 10001x1").is_err());
        assert!(validate_region("0,0 800x600").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn child_output_is_bounded() {
        let result = run_command(
            Path::new("/usr/bin/yes"),
            &[],
            None,
            Duration::from_secs(2),
            1024,
            1024,
        );
        assert!(result
            .err()
            .is_some_and(|error| error.contains("output exceeded its limit")));
    }

    #[cfg(unix)]
    #[test]
    fn child_stdin_is_used_without_an_argument() {
        let result = run_command(
            Path::new("/bin/cat"),
            &[],
            Some(b"private screen text"),
            Duration::from_secs(2),
            1024,
            1024,
        )
        .expect("cat should receive stdin");
        assert_eq!(result.stdout, b"private screen text");
    }

    #[cfg(unix)]
    #[test]
    fn child_timeout_is_hard() {
        let result = run_command(
            Path::new("/bin/sleep"),
            &["2"],
            None,
            Duration::from_millis(20),
            1024,
            1024,
        );
        assert!(result
            .err()
            .is_some_and(|error| error.contains("timed out")));
    }

    #[cfg(unix)]
    #[test]
    fn stdin_only_helper_does_not_wait_for_inherited_descriptors() {
        let started = Instant::now();
        let status = run_stdin_only(
            Path::new("/bin/sh"),
            &["-c", "cat >/dev/null; (sleep 2) & exit 0"],
            b"clipboard text",
            Duration::from_secs(1),
        )
        .expect("stdin-only helper should exit");
        assert!(status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
