//! OCR engine: orchestrates slurp -> grim -> magick -> tesseract -> wl-copy.
//!
//! All external tools are resolved from PATH and invoked with fixed argument
//! lists via `Command` (never a shell), so no user input can inject commands.
//! Temp files are created inside a private directory under $XDG_RUNTIME_DIR
//! (or /tmp) and removed on every exit path.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use serde::Serialize;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn run(cmd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cmd.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "{} exited with {}: {}",
            cmd.file_name().unwrap_or_default().to_string_lossy(),
            out.status,
            if err.is_empty() {
                "no error output".into()
            } else {
                err
            }
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve the tesseract language list to a `-l` argument. If the requested
/// language is not installed, fall back to `eng` so the command still works.
pub fn list_langs() -> Result<Vec<String>, String> {
    let tesseract = which("tesseract")?;
    let out = Command::new(&tesseract)
        .args(["--list-langs"])
        .output()
        .map_err(|e| format!("cannot run tesseract: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut langs = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        // Language codes are short tokens (e.g. "eng", "por", "osd"); skip the
        // header lines tesseract prints ("List of available languages...").
        if !l.is_empty() && l.len() <= 8 && !l.contains(' ') && !l.contains('"') {
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
    if let Ok(out) = Command::new("hyprctl")
        .args(["getoption", "input:kb_layout"])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("str:") {
                    let v = v.trim();
                    if !v.is_empty() {
                        return v.to_string();
                    }
                }
            }
        }
    }

    // 2. fcitx5: `fcitx5-remote -n` prints the current input method name,
    //    e.g. "keyboard-us" or "keyboard-br".
    if let Ok(out) = Command::new("fcitx5-remote").args(["-n"]).output() {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(idx) = name.find("keyboard-") {
                let layout = name[idx + "keyboard-".len()..].to_string();
                if !layout.is_empty() {
                    return layout;
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

fn capture_region() -> Result<TempFile, String> {
    let slurp = which("slurp")?;
    let grim = which("grim")?;
    let png = TempFile::new("region", "png")?;

    // slurp prints "x,y WxH" (e.g. "100,200 800x600") to stdout.
    let region = run(&slurp, &[])?;
    let region = region.trim();
    if region.is_empty() {
        return Err("selection cancelled".into());
    }

    let output_path = png.path().to_string_lossy().into_owned();
    let out = Command::new(&grim)
        .args(["-g", region, output_path.as_str()])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run grim: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "grim exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(png)
}

fn capture_full() -> Result<TempFile, String> {
    let grim = which("grim")?;
    let png = TempFile::new("full", "png")?;
    let output_path = png.path().to_string_lossy().into_owned();
    let out = Command::new(&grim)
        .args([output_path.as_str()])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run grim: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "grim exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
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
    let (w, h) = image_size(src)?;

    let mut args: Vec<String> = vec![src.to_string(), "-colorspace".into(), "Gray".into()];
    // Upscale only when the capture is small (region grabs, low-res sources).
    if w > 0 && h > 0 && w < 1200 && h < 1200 {
        args.push("-resize".into());
        args.push("300%".into());
    }
    args.push("-sharpen".into());
    args.push("0x0.5".into());
    args.push(out.path().to_string_lossy().into_owned());

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let status = Command::new(&magick)
        .args(&arg_refs)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run magick: {e}"))?;
    if !status.success() {
        return Err(format!("magick exited with {status}"));
    }
    Ok(out)
}

/// Read the pixel dimensions of an image via `identify -format %wx%h`.
fn image_size(path: &str) -> Result<(u32, u32), String> {
    let identify = which("identify")?;
    let out = Command::new(&identify)
        .args(["-format", "%wx%h", path])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run identify: {e}"))?;
    if !out.status.success() {
        return Ok((0, 0));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if let Some((w, h)) = s.split_once('x') {
        let w = w.parse().unwrap_or(0);
        let h = h.parse().unwrap_or(0);
        return Ok((w, h));
    }
    Ok((0, 0))
}

fn ocr_image(path: &str, lang: &str) -> Result<String, String> {
    let tesseract = which("tesseract")?;
    let out = Command::new(&tesseract)
        .args([path, "stdout", "-l", lang, "--psm", "6"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run tesseract: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tesseract exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run tesseract in TSV mode and return the mean per-word confidence (0-100).
/// Low confidence is a strong signal that the region was icons/graphics rather
/// than text, so the panel can warn instead of copying garbage.
fn ocr_confidence(path: &str, lang: &str) -> f64 {
    let tesseract = match which("tesseract") {
        Ok(t) => t,
        Err(_) => return 0.0,
    };
    let out = match Command::new(&tesseract)
        .args([path, "stdout", "-l", lang, "--psm", "6", "tsv"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return 0.0,
    };
    let text = String::from_utf8_lossy(&out.stdout);
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
        0.0
    } else {
        sum / n as f64
    }
}

pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let wl_copy = which("wl-copy")?;
    let mut child = Command::new(&wl_copy)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run wl-copy: {e}"))?;
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .ok_or("wl-copy stdin unavailable")?
        .write_all(text.as_bytes())
        .map_err(|e| format!("cannot write to wl-copy: {e}"))?;
    let status = child.wait().map_err(|e| format!("wl-copy failed: {e}"))?;
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
        if !Path::new(path).is_file() {
            return Err(format!("file not found: {path}"));
        }
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
    let confidence = ocr_confidence(&ocr_input, &lang);

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
}
