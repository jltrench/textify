//! Textify - extract text from any region of the screen for Omarchy.
//!
//! Pipeline (all local, no network):
//!   slurp  -> select a screen region (or use --full for the whole screen)
//!   grim   -> capture that region to a temp PNG
//!   magick -> pre-process (grayscale, upscale, threshold) for better accuracy
//!   tesseract -> OCR the image to text
//!   wl-copy -> put the text on the clipboard
//!
//! Commands:
//!   textify region [--lang LANG] [--copy] [--json]   Select a region and OCR it
//!   textify full  [--lang LANG] [--copy] [--json]    OCR the whole screen
//!   textify file <path> [--lang LANG] [--json]       OCR an existing image file
//!   textify copy                                      Read clipboard text from stdin
//!   textify langs                                     List installed languages
//!   textify --version
//!
//! Security: every external binary is resolved from PATH and invoked with a
//! fixed argument list (no shell interpolation). Temp files live in a private
//! directory under $XDG_RUNTIME_DIR or /tmp and are removed on exit. No
//! elevated privileges. Clipboard text is accepted through stdin rather than
//! argv so it is not exposed through process listings.

mod engine;

use std::env;
use std::process::exit;

use engine::{run_ocr, OcrOptions, OcrResult};

fn fail(msg: &str) -> ! {
    eprintln!("Textify: {msg}");
    exit(1);
}

fn take_value(raw: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    match raw.get(*i) {
        Some(v) => v.clone(),
        None => fail(&format!("{flag} needs a value")),
    }
}

fn parse_common(raw: &[String]) -> (OcrOptions, Vec<String>) {
    let mut opts = OcrOptions::default();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--lang" => opts.lang = take_value(raw, &mut i, "--lang"),
            "--copy" => opts.copy = true,
            "--json" => opts.json = true,
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    (opts, positional)
}

fn cmd_region(raw: &[String]) {
    let (opts, _) = parse_common(raw);
    match run_ocr(OcrOptions {
        region: true,
        ..opts
    }) {
        Ok(r) => print_result(&r, opts.json),
        Err(e) => fail(&e),
    }
}

fn cmd_full(raw: &[String]) {
    let (opts, _) = parse_common(raw);
    match run_ocr(OcrOptions {
        region: false,
        ..opts
    }) {
        Ok(r) => print_result(&r, opts.json),
        Err(e) => fail(&e),
    }
}

fn cmd_file(raw: &[String]) {
    let (opts, positional) = parse_common(raw);
    let path = positional.first().map(String::as_str).unwrap_or("");
    if path.is_empty() {
        fail("usage: textify file <path> [--lang LANG] [--json]");
    }
    match run_ocr(OcrOptions {
        file: Some(path.to_string()),
        ..opts
    }) {
        Ok(r) => print_result(&r, opts.json),
        Err(e) => fail(&e),
    }
}

fn cmd_langs() {
    match engine::list_langs() {
        Ok(langs) => {
            if langs.is_empty() {
                println!("[]");
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&langs).unwrap_or_else(|_| "[]".into())
                );
            }
        }
        Err(e) => fail(&e),
    }
}

fn cmd_copy(raw: &[String]) {
    if !raw.is_empty() {
        fail("usage: printf '%s' \"text\" | textify copy");
    }
    let text = match engine::read_stdin_limited() {
        Ok(text) => text,
        Err(error) => fail(&error),
    };
    if text.trim().is_empty() {
        fail("usage: printf '%s' \"text\" | textify copy");
    }
    match engine::copy_to_clipboard(&text) {
        Ok(()) => println!("{}", serde_json::json!({ "copied": true })),
        Err(e) => fail(&e),
    }
}

fn cmd_lang() {
    match engine::detect_lang() {
        Ok((layout, lang)) => {
            println!("{}", serde_json::json!({ "layout": layout, "lang": lang }));
        }
        Err(e) => fail(&e),
    }
}

fn print_result(r: &OcrResult, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(r)
                .unwrap_or_else(|_| "{\"text\":\"\",\"lang\":\"\",\"copied\":false}".into())
        );
    } else {
        println!("{}", r.text);
    }
}

fn main() {
    let mut argv = env::args().skip(1);
    match argv.next().as_deref() {
        Some("region") => cmd_region(&argv.collect::<Vec<_>>()),
        Some("full") => cmd_full(&argv.collect::<Vec<_>>()),
        Some("file") => cmd_file(&argv.collect::<Vec<_>>()),
        Some("copy") => cmd_copy(&argv.collect::<Vec<_>>()),
        Some("lang") => cmd_lang(),
        Some("langs") => cmd_langs(),
        Some("--version") | Some("version") => println!("Textify {}", env!("CARGO_PKG_VERSION")),
        Some(other) => fail(&format!(
            "unknown command \"{other}\"; usage: textify <region|full|file|copy|lang|langs>"
        )),
        None => fail("usage: textify <region|full|file|copy|lang|langs>"),
    }
}
