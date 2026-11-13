//! Arduino Lite — a small GTK4 / libadwaita front-end for `arduino-cli`.
//!
//! The editor is GtkSourceView; compiling, uploading, board and port discovery
//! and the serial monitor are all delegated to `arduino-cli` running in
//! background threads so the UI never blocks.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};
use sourceview5::prelude::*;

const APP_ID: &str = "dev.arduinolite.Ide";

const NEW_SKETCH: &str =
    "#include <Arduino.h>\n\nvoid setup() {\n  \n}\n\nvoid loop() {\n  \n}\n";

const BAUDS: [&str; 7] = [
    "9600", "19200", "38400", "57600", "115200", "230400", "460800",
];

const LOG_CAP: i32 = 500_000;
const SERIAL_CAP: i32 = 200_000;

// ───────────────────────────── data & messages ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Board {
    name: String,
    fqbn: String,
}

#[derive(Debug, Clone, PartialEq)]
struct PortInfo {
    address: String,
    label: String,
}

/// One selectable value of a board option (e.g. "16 MHz").
#[derive(Debug, Clone, PartialEq)]
struct OptionValue {
    value: String,
    label: String,
    default: bool,
}

/// A board menu option as reported by `arduino-cli board details` (e.g. "CPU Speed").
#[derive(Debug, Clone, PartialEq)]
struct BoardOption {
    id: String,
    label: String,
    values: Vec<OptionValue>,
}

/// A core / platform as listed by `arduino-cli core search`.
#[derive(Debug, Clone, PartialEq)]
struct Platform {
    id: String,
    name: String,
    maintainer: String,
    installed: Option<String>,
    latest: String,
}

/// Widgets of the Board Manager window.
#[derive(Clone)]
struct Manager {
    window: adw::Window,
    controls: Vec<gtk::Widget>,
    progress_box: gtk::Box,
    op_label: gtk::Label,
    status_label: gtk::Label,
    progress: gtk::ProgressBar,
    cancel_btn: gtk::Button,
    urls_list: gtk::ListBox,
    url_entry: gtk::Entry,
    platform_list: gtk::ListBox,
    log: gtk::TextView,
    spinner: gtk::Spinner,
}

enum Msg {
    Log(String),
    TaskDone { ok: bool, label: &'static str },
    Boards(Result<Vec<Board>, String>),
    Ports(Result<Vec<PortInfo>, String>),
    BoardOptions(String, Result<Vec<BoardOption>, String>),
    Urls(Result<Vec<String>, String>),
    Platforms(Result<Vec<Platform>, String>),
    MgrLog(String),
    MgrDone { ok: bool, label: String, cancelled: bool },
    Monitor(u64, String),
    MonitorEnded(u64),
}

struct Monitor {
    child: Child,
    stdin: Option<ChildStdin>,
}

// ───────────────────────────── arduino-cli helpers ─────────────────────────

/// arduino-cli is always started in a directory that is guaranteed to exist, so a stale or
/// deleted working directory of this app can never leak into its child processes.
fn cli() -> Command {
    let mut c = Command::new(std::env::var("ARDUINO_CLI").unwrap_or_else(|_| "arduino-cli".into()));
    c.current_dir(std::env::temp_dir());
    c
}

const CLI_HINT: &str =
    "Install arduino-cli from https://arduino.github.io/arduino-cli/ (or set ARDUINO_CLI to its path).";

/// Run a command to completion and return its stdout.
fn run_capture(mut cmd: Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run arduino-cli ({e}). {CLI_HINT}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Forward a pipe to the UI line by line.
fn pump_lines<R: Read + Send + 'static>(
    r: R,
    tx: async_channel::Sender<Msg>,
    wrap: fn(String) -> Msg,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send_blocking(wrap(line)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Run a long task (compile / upload), streaming its output to the UI.
fn run_task(mut cmd: Command, label: &'static str, tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let ok = match cmd.spawn() {
            Ok(mut child) => {
                let h1 = child.stdout.take().map(|o| pump_lines(o, tx.clone(), Msg::Log));
                let h2 = child
                    .stderr
                    .take()
                    .map(|e| pump_lines(e, tx.clone(), Msg::Log));
                let status = child.wait();
                for h in [h1, h2].into_iter().flatten() {
                    let _ = h.join();
                }
                matches!(status, Ok(s) if s.success())
            }
            Err(e) => {
                let _ = tx.send_blocking(Msg::Log(format!(
                    "Could not start arduino-cli: {e}\n{CLI_HINT}\n"
                )));
                false
            }
        };
        let _ = tx.send_blocking(Msg::TaskDone { ok, label });
    });
}

/// Lets the UI interrupt a running Board Manager job.
#[derive(Default)]
struct CancelToken {
    cancelled: AtomicBool,
    /// pid of the arduino-cli process currently running (0 = none)
    pid: AtomicU32,
}

fn signal(pid: u32, sig: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Ask the running process to stop (SIGTERM), and force it (SIGKILL) if it ignores us.
/// SIGTERM rather than SIGINT: a process started from a backgrounded shell inherits an ignored SIGINT.
fn cancel_token(t: &Arc<CancelToken>) {
    t.cancelled.store(true, Ordering::SeqCst);
    let pid = t.pid.load(Ordering::SeqCst);
    if pid != 0 {
        let t = Arc::clone(t);
        std::thread::spawn(move || {
            signal(pid, "TERM");
            std::thread::sleep(Duration::from_secs(3));
            if t.pid.load(Ordering::SeqCst) == pid {
                signal(pid, "KILL");
            }
        });
    }
}

/// Run several arduino-cli invocations in order for the Board Manager, stopping at the first
/// failure or when cancelled.
fn run_steps(
    steps: Vec<Vec<String>>,
    label: String,
    tx: async_channel::Sender<Msg>,
    token: Arc<CancelToken>,
) {
    std::thread::spawn(move || {
        let mut ok = true;
        for step in steps {
            if token.cancelled.load(Ordering::SeqCst) {
                ok = false;
                break;
            }
            let _ = tx.send_blocking(Msg::MgrLog(format!("$ arduino-cli {}\n", step.join(" "))));
            let mut cmd = cli();
            cmd.args(&step)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id();
                    token.pid.store(pid, Ordering::SeqCst);
                    if token.cancelled.load(Ordering::SeqCst) {
                        signal(pid, "TERM");
                    }
                    let h1 = child.stdout.take().map(|o| pump_lines(o, tx.clone(), Msg::MgrLog));
                    let h2 = child.stderr.take().map(|e| pump_lines(e, tx.clone(), Msg::MgrLog));
                    let status = child.wait();
                    token.pid.store(0, Ordering::SeqCst);
                    for h in [h1, h2].into_iter().flatten() {
                        let _ = h.join();
                    }
                    if !matches!(status, Ok(s) if s.success()) {
                        ok = false;
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send_blocking(Msg::MgrLog(format!(
                        "Could not start arduino-cli: {e}\n{CLI_HINT}\n"
                    )));
                    ok = false;
                    break;
                }
            }
        }
        let cancelled = token.cancelled.load(Ordering::SeqCst);
        let _ = tx.send_blocking(Msg::MgrDone {
            ok: ok && !cancelled,
            label,
            cancelled,
        });
    });
}

/// Bytes appended to text sent through the serial monitor.
fn line_ending(index: u32) -> &'static [u8] {
    match index {
        0 => b"",
        1 => b"\n",
        2 => b"\r",
        _ => b"\r\n",
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

/// Forward raw serial bytes to the UI, keeping UTF-8 sequences intact.
fn pump_monitor<R: Read + Send + 'static>(
    mut r: R,
    tx: async_channel::Sender<Msg>,
    generation: u64,
    notify_end: bool,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => pending.len(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                    pending.drain(..valid);
                    if !text.is_empty()
                        && tx.send_blocking(Msg::Monitor(generation, text)).is_err()
                    {
                        return;
                    }
                }
            }
        }
        if notify_end {
            let _ = tx.send_blocking(Msg::MonitorEnded(generation));
        }
    });
}

/// Ask the monitor process to exit cleanly (so it releases the port), then force it.
fn terminate(mut m: Monitor) {
    drop(m.stdin.take());
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(m.child.id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for _ in 0..75 {
        if let Ok(Some(_)) = m.child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = m.child.kill();
    let _ = m.child.wait();
}

fn parse_boards(s: &str) -> Result<Vec<Board>, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("unreadable board list: {e}"))?;
    let arr = v
        .get("boards")
        .and_then(|b| b.as_array())
        .or_else(|| v.as_array())
        .ok_or("unexpected board list format")?;
    let mut out: Vec<Board> = arr
        .iter()
        .filter_map(|b| {
            if b.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
                return None;
            }
            let name = b.get("name")?.as_str()?;
            let fqbn = b.get("fqbn")?.as_str()?;
            Some(Board {
                name: name.to_owned(),
                fqbn: fqbn.to_owned(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.dedup_by(|a, b| a.fqbn == b.fqbn);
    Ok(out)
}

fn parse_ports(s: &str) -> Result<Vec<PortInfo>, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("unreadable port list: {e}"))?;
    let empty = Vec::new();
    let arr = v
        .get("detected_ports")
        .and_then(|p| p.as_array())
        .or_else(|| v.as_array())
        .unwrap_or(&empty);
    Ok(arr
        .iter()
        .filter_map(|e| {
            let port = e.get("port")?;
            let address = port.get("address")?.as_str()?;
            let proto = port
                .get("protocol")
                .and_then(|x| x.as_str())
                .unwrap_or("serial");
            if proto != "serial" {
                return None;
            }
            let board = e
                .get("matching_boards")
                .and_then(|m| m.as_array())
                .and_then(|m| m.first())
                .and_then(|b| b.get("name"))
                .and_then(|n| n.as_str());
            let label = match board {
                Some(b) => format!("{address} — {b}"),
                None => address.to_owned(),
            };
            Some(PortInfo {
                address: address.to_owned(),
                label,
            })
        })
        .collect())
}

fn parse_options(s: &str) -> Result<Vec<BoardOption>, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("unreadable board details: {e}"))?;
    let Some(arr) = v.get("config_options").and_then(|c| c.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(arr
        .iter()
        .filter_map(|o| {
            let id = o.get("option")?.as_str()?;
            let label = o.get("option_label").and_then(|l| l.as_str()).unwrap_or(id);
            let values: Vec<OptionValue> = o
                .get("values")?
                .as_array()?
                .iter()
                .filter_map(|x| {
                    let value = x.get("value")?.as_str()?;
                    Some(OptionValue {
                        value: value.to_owned(),
                        label: x
                            .get("value_label")
                            .and_then(|l| l.as_str())
                            .unwrap_or(value)
                            .to_owned(),
                        default: x.get("selected").and_then(|b| b.as_bool()).unwrap_or(false),
                    })
                })
                .collect();
            if values.is_empty() {
                return None;
            }
            Some(BoardOption {
                id: id.to_owned(),
                label: label.to_owned(),
                values,
            })
        })
        .collect())
}

fn find_urls(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(a) = m.get("additional_urls").and_then(|a| a.as_array()) {
                return Some(a.iter().filter_map(|u| u.as_str().map(str::to_owned)).collect());
            }
            m.values().find_map(find_urls)
        }
        _ => None,
    }
}

fn parse_urls(s: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("unreadable configuration: {e}"))?;
    Ok(find_urls(&v).unwrap_or_default())
}

fn parse_platforms(s: &str) -> Result<Vec<Platform>, String> {
    let v: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("unreadable platform list: {e}"))?;
    let empty = Vec::new();
    let arr = v
        .get("platforms")
        .and_then(|p| p.as_array())
        .or_else(|| v.as_array())
        .unwrap_or(&empty);
    let text = |x: Option<&serde_json::Value>| -> Option<String> {
        x.and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_owned)
    };
    let mut out: Vec<Platform> = arr
        .iter()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.to_owned();
            let latest = text(p.get("latest_version").or_else(|| p.get("latest"))).unwrap_or_default();
            let installed = text(p.get("installed_version").or_else(|| p.get("installed")));
            let deprecated = p.get("deprecated").and_then(|d| d.as_bool()).unwrap_or(false);
            if deprecated && installed.is_none() {
                return None;
            }
            let name = text(p.get("name"))
                .or_else(|| {
                    let rel = p.get("releases")?.as_object()?;
                    let r = rel.get(&latest).or_else(|| rel.values().next())?;
                    text(r.get("name"))
                })
                .unwrap_or_else(|| id.clone());
            Some(Platform {
                maintainer: text(p.get("maintainer")).unwrap_or_default(),
                id,
                name,
                installed,
                latest,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.installed
            .is_some()
            .cmp(&a.installed.is_some())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Build `vendor:arch:board:opt1=val1,opt2=val2` from the user's explicit picks.
fn compose_fqbn(base: &str, options: &[BoardOption], picks: &BTreeMap<String, String>) -> String {
    let parts: Vec<String> = if options.is_empty() {
        // Option list not loaded yet: still honour the saved picks.
        picks.iter().map(|(k, v)| format!("{k}={v}")).collect()
    } else {
        options
            .iter()
            .filter_map(|o| picks.get(&o.id).map(|v| format!("{}={}", o.id, v)))
            .collect()
    };
    if parts.is_empty() {
        base.to_owned()
    } else {
        format!("{base}:{}", parts.join(","))
    }
}

// ───────────────────────────── sketch preparation ──────────────────────────

fn sanitize(stem: &str) -> String {
    let mut s: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s = "sketch".into();
    }
    if s.starts_with('-') {
        s.insert(0, '_');
    }
    s.truncate(63);
    s
}

struct Prepared {
    sketch_dir: PathBuf,
    build_dir: PathBuf,
    note: Option<String>,
}

/// arduino-cli insists on `<name>/<name>.ino`. We keep the user's `.cpp` as the real
/// source and assemble a throw-away sketch folder (with a tiny `.ino` stub) in the temp dir.
fn prepare_sketch(main: &Path) -> std::io::Result<Prepared> {
    let stem = main
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("sketch")
        .to_owned();
    let ext = main
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("cpp")
        .to_lowercase();
    let name = sanitize(&stem);

    let mut hasher = DefaultHasher::new();
    main.hash(&mut hasher);
    let base = std::env::temp_dir()
        .join("arduino-lite")
        .join(format!("{:016x}", hasher.finish()));

    let sketch_dir = base.join("sketch").join(&name);
    if sketch_dir.exists() {
        fs::remove_dir_all(&sketch_dir)?;
    }
    fs::create_dir_all(&sketch_dir)?;

    fs::copy(main, sketch_dir.join(format!("{name}.{ext}")))?;

    let parent = main.parent().unwrap_or_else(|| Path::new("."));
    let project_folder = parent.file_name().and_then(|n| n.to_str()) == Some(stem.as_str());
    let mut note = None;
    if project_folder {
        for entry in fs::read_dir(parent)? {
            let p = entry?.path();
            if p == main || !p.is_file() {
                continue;
            }
            let e = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if matches!(e.as_str(), "cpp" | "c" | "h" | "hpp" | "ino") {
                if let Some(fname) = p.file_name() {
                    let dest = sketch_dir.join(fname);
                    if !dest.exists() {
                        fs::copy(&p, &dest)?;
                    }
                }
            }
        }
    } else {
        note = Some(format!(
            "Single-file build. To include sibling .h/.cpp files, keep the sketch in a folder named \"{stem}\".\n"
        ));
    }

    if ext == "cpp" || ext == "c" {
        let ino = sketch_dir.join(format!("{name}.ino"));
        if !ino.exists() {
            fs::write(
                &ino,
                "// Entry stub generated by Arduino Lite; the real code lives in the .cpp file.\n#include <Arduino.h>\n",
            )?;
        }
    }

    Ok(Prepared {
        sketch_dir,
        build_dir: base.join("build"),
        note,
    })
}

// ───────────────────────────── settings ────────────────────────────────────

fn config_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(dir.join("arduino-lite").join("board"))
}

fn load_saved_board() -> Option<String> {
    fs::read_to_string(config_path()?)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn save_board(fqbn: &str) {
    if let Some(p) = config_path() {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::write(p, fqbn);
    }
}

fn options_path() -> Option<PathBuf> {
    Some(config_path()?.with_file_name("board-options"))
}

fn load_picks(fqbn: &str) -> BTreeMap<String, String> {
    let Some(p) = options_path() else {
        return BTreeMap::new();
    };
    let Ok(text) = fs::read_to_string(p) else {
        return BTreeMap::new();
    };
    for line in text.lines() {
        if let Some((f, rest)) = line.split_once('\t') {
            if f == fqbn {
                return rest
                    .split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
            }
        }
    }
    BTreeMap::new()
}

fn save_picks(fqbn: &str, picks: &BTreeMap<String, String>) {
    let Some(p) = options_path() else {
        return;
    };
    let existing = fs::read_to_string(&p).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| l.split_once('\t').map(|(f, _)| f != fqbn).unwrap_or(false))
        .map(str::to_owned)
        .collect();
    if !picks.is_empty() {
        let joined: Vec<String> = picks.iter().map(|(k, v)| format!("{k}={v}")).collect();
        lines.push(format!("{fqbn}\t{}", joined.join(",")));
    }
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = fs::write(p, out);
}

// ───────────────────────────── UI helpers ──────────────────────────────────

fn append(view: &gtk::TextView, text: &str, cap: i32) {
    let buf = view.buffer();
    let mut end = buf.end_iter();
    buf.insert(&mut end, text);
    if buf.char_count() > cap {
        let mut start = buf.start_iter();
        let mut cut = buf.iter_at_offset(cap / 4);
        buf.delete(&mut start, &mut cut);
    }
    let mark = buf.create_mark(None, &buf.end_iter(), false);
    view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
    buf.delete_mark(&mark);
}

fn apply_scheme(buffer: &sourceview5::Buffer) {
    let dark = adw::StyleManager::default().is_dark();
    let mgr = sourceview5::StyleSchemeManager::default();
    let ids: &[&str] = if dark {
        &["Adwaita-dark", "classic-dark"]
    } else {
        &["Adwaita", "classic"]
    };
    for id in ids {
        if let Some(s) = mgr.scheme(id) {
            buffer.set_style_scheme(Some(&s));
            return;
        }
    }
}

fn text_view() -> gtk::TextView {
    gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(10)
        .right_margin(10)
        .top_margin(6)
        .bottom_margin(6)
        .build()
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .hexpand(true)
        .vexpand(true)
        .min_content_height(80)
        .build()
}

fn string_dropdown(model: &gtk::StringList, searchable: bool) -> gtk::DropDown {
    let dd = gtk::DropDown::new(Some(model.clone()), gtk::Expression::NONE);
    if searchable {
        let expr = gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            gtk::Expression::NONE,
            "string",
        );
        dd.set_expression(Some(&expr));
        dd.set_enable_search(true);
        dd.set_search_match_mode(gtk::StringFilterMatchMode::Substring);
    }
    dd
}

fn icon_label(icon: &str, label: &str) -> adw::ButtonContent {
    adw::ButtonContent::builder()
        .icon_name(icon)
        .label(label)
        .build()
}

// ───────────────────────────── settings, recent files, text helpers ─────────

#[derive(Debug, Clone, PartialEq)]
struct Settings {
    /// "system", "light" or "dark"
    theme: String,
    /// Editor font size in points; 0 = keep the system default.
    font_pt: u32,
    wrap: bool,
    line_numbers: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "system".into(),
            font_pt: 0,
            wrap: false,
            line_numbers: true,
        }
    }
}

impl Settings {
    fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "theme" if matches!(v, "system" | "light" | "dark") => s.theme = v.to_owned(),
                "font_pt" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if n == 0 || (6..=48).contains(&n) {
                            s.font_pt = n;
                        }
                    }
                }
                "wrap" => s.wrap = v == "true",
                "line_numbers" => s.line_numbers = v != "false",
                _ => {}
            }
        }
        s
    }

    fn serialize(&self) -> String {
        format!(
            "theme={}\nfont_pt={}\nwrap={}\nline_numbers={}\n",
            self.theme, self.font_pt, self.wrap, self.line_numbers
        )
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(config_path()?.with_file_name("settings"))
}

fn load_settings() -> Settings {
    settings_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|t| Settings::parse(&t))
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    if let Some(p) = settings_path() {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::write(p, s.serialize());
    }
}

fn recent_path() -> Option<PathBuf> {
    Some(config_path()?.with_file_name("recent"))
}

fn load_recent() -> Vec<String> {
    recent_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn save_recent(list: &[String]) {
    if let Some(p) = recent_path() {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let _ = fs::write(p, list.join("\n") + "\n");
    }
}

/// Move `path` to the front of the recent list, dropping duplicates and the oldest entries.
fn push_recent(list: &mut Vec<String>, path: &str, max: usize) {
    list.retain(|p| p != path);
    list.insert(0, path.to_owned());
    list.truncate(max);
}

/// Toggle `//` comments on a block of lines: comment all, or uncomment when all are commented.
fn toggle_comment_lines(lines: &[String]) -> Vec<String> {
    let nonblank: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    let uncomment =
        !nonblank.is_empty() && nonblank.iter().all(|l| l.trim_start().starts_with("//"));
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.clone();
            }
            let indent_len = l.len() - l.trim_start().len();
            let (indent, rest) = l.split_at(indent_len);
            if uncomment {
                let rest = rest.strip_prefix("//").unwrap_or(rest);
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                format!("{indent}{rest}")
            } else {
                format!("{indent}// {rest}")
            }
        })
        .collect()
}

/// Pipe source text through `clang-format`, honouring a `.clang-format` file near the sketch.
fn run_clang_format(text: &str, filename: &str) -> Result<String, String> {
    let mut child = Command::new("clang-format")
        .arg("--style=file")
        .arg("--fallback-style=LLVM")
        .arg(format!("--assume-filename={filename}"))
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "clang-format is not installed (for example: sudo apt install clang-format)".to_owned()
            } else {
                format!("could not run clang-format: {e}")
            }
        })?;
    let mut stdin = child.stdin.take().ok_or("clang-format gave no stdin")?;
    let data = text.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if out.status.success() {
        String::from_utf8(out.stdout).map_err(|e| e.to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Remove stale firmware images so "Export" never copies leftovers from another board.
fn clean_outputs(build_dir: &Path) {
    if let Ok(rd) = fs::read_dir(build_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && is_firmware_file(&p) {
                let _ = fs::remove_file(p);
            }
        }
    }
}

fn is_firmware_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("hex" | "bin" | "elf" | "uf2" | "map")
    )
}

fn menu_section(items: &[(&str, &str)]) -> gio::Menu {
    let m = gio::Menu::new();
    for (label, action) in items {
        m.append(Some(label), Some(action));
    }
    m
}

/// The hamburger menu: essentials only. Everything else stays reachable through its shortcut.
fn build_menu_model(recent_menu: &gio::Menu) -> gio::Menu {
    let root = gio::Menu::new();

    let file = gio::Menu::new();
    file.append_submenu(Some("Open Recent"), recent_menu);
    file.append(Some("Save As…"), Some("win.save-as"));
    root.append_section(None, &file);

    root.append_section(
        None,
        &menu_section(&[
            ("Find…", "win.find"),
            ("Replace…", "win.replace"),
            ("Go to Line…", "win.goto-line"),
            ("Comment / Uncomment Lines", "win.comment"),
            ("Auto Format", "win.format"),
        ]),
    );

    root.append_section(
        None,
        &menu_section(&[
            ("Export Compiled Binary…", "win.export-binaries"),
            ("Show Build Folder", "win.show-build-folder"),
        ]),
    );

    let view = gio::Menu::new();
    let theme = gio::Menu::new();
    for (label, value) in [("System", "system"), ("Light", "light"), ("Dark", "dark")] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("win.theme"), Some(&value.to_variant()));
        theme.append_item(&item);
    }
    view.append_submenu(Some("Theme"), &theme);
    view.append_submenu(
        Some("Zoom"),
        &menu_section(&[
            ("Zoom In", "win.zoom-in"),
            ("Zoom Out", "win.zoom-out"),
            ("Reset Zoom", "win.zoom-reset"),
        ]),
    );
    view.append(Some("Word Wrap"), Some("win.wrap"));
    root.append_section(None, &view);

    root.append_section(
        None,
        &menu_section(&[
            ("Keyboard Shortcuts", "win.shortcuts"),
            ("About Arduino Lite", "win.about"),
        ]),
    );
    root
}

// ───────────────────────────── application state ───────────────────────────

struct App {
    win: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    spinner: gtk::Spinner,
    buffer: sourceview5::Buffer,
    log_view: gtk::TextView,
    serial_view: gtk::TextView,
    serial_entry: gtk::Entry,
    stack: adw::ViewStack,
    board_model: gtk::StringList,
    board_dd: gtk::DropDown,
    port_model: gtk::StringList,
    port_dd: gtk::DropDown,
    options_box: gtk::Box,
    options_btn: gtk::MenuButton,
    baud_dd: gtk::DropDown,
    monitor_btn: gtk::ToggleButton,
    verify_action: gio::SimpleAction,
    upload_action: gio::SimpleAction,
    tx: async_channel::Sender<Msg>,

    path: RefCell<Option<PathBuf>>,
    project_path: RefCell<Option<PathBuf>>,
    modified: Cell<bool>,
    tabs_box: gtk::Box,
    unsaved_buffers: RefCell<BTreeMap<PathBuf, String>>,
    busy: Cell<bool>,
    loading_boards: Cell<bool>,
    suppress_toggle: Cell<bool>,
    boards: RefCell<Vec<Board>>,
    ports: RefCell<Vec<PortInfo>>,
    ending_dd: gtk::DropDown,
    editor: sourceview5::View,
    bottom: gtk::Box,
    find_bar: gtk::SearchBar,
    find_entry: gtk::SearchEntry,
    replace_entry: gtk::Entry,
    replace_row: gtk::Box,
    search_ctx: sourceview5::SearchContext,
    search_settings: sourceview5::SearchSettings,
    recent_menu: gio::Menu,
    recent: RefCell<Vec<String>>,
    settings: RefCell<Settings>,
    font_provider: gtk::CssProvider,
    last_build: RefCell<Option<PathBuf>>,
    manager: RefCell<Option<Manager>>,
    mgr_busy: Cell<bool>,
    mgr_cancel: RefCell<Option<Arc<CancelToken>>>,
    mgr_tick: Rc<Cell<u64>>,
    mgr_seq: Cell<u64>,
    mgr_url_buttons: RefCell<Vec<gtk::Button>>,
    mgr_plat_buttons: RefCell<Vec<gtk::Button>>,
    platforms: RefCell<Vec<Platform>>,
    urls: RefCell<Vec<String>>,
    options: RefCell<Vec<BoardOption>>,
    picks: RefCell<BTreeMap<String, String>>,
    monitor: RefCell<Option<Monitor>>,
    monitor_gen: Cell<u64>,
}

impl App {
    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn log(&self, text: &str) {
        append(&self.log_view, text, LOG_CAP);
    }

    fn editor_text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
            .to_string()
    }

    fn refresh_title(&self) {
        let path = self.path.borrow().clone();
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let shown = if self.modified.get() {
            format!("• {name}")
        } else {
            name
        };
        let sub = path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        self.title.set_title(&shown);
        self.title.set_subtitle(&sub);
        self.win.set_title(Some(&format!("{shown} — Arduino Lite")));
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.verify_action.set_enabled(!busy);
        self.upload_action.set_enabled(!busy);
        self.spinner.set_spinning(busy);
        self.spinner.set_visible(busy);
    }

    fn selected_fqbn(&self) -> Option<String> {
        let i = self.board_dd.selected();
        if i == gtk::INVALID_LIST_POSITION {
            return None;
        }
        self.boards.borrow().get(i as usize).map(|b| b.fqbn.clone())
    }

    fn selected_port(&self) -> Option<String> {
        let i = self.port_dd.selected();
        if i == gtk::INVALID_LIST_POSITION {
            return None;
        }
        self.ports.borrow().get(i as usize).map(|p| p.address.clone())
    }

    // ── recent files, settings ──

    fn add_recent(&self, path: &Path) {
        {
            let mut r = self.recent.borrow_mut();
            push_recent(&mut r, &path.to_string_lossy(), 8);
            save_recent(&r);
        }
        self.rebuild_recent_menu();
    }

    fn rebuild_recent_menu(&self) {
        self.recent_menu.remove_all();
        let recent = self.recent.borrow().clone();
        if recent.is_empty() {
            // "win.none" does not exist, so GTK shows the entry greyed out.
            self.recent_menu.append(Some("No recent files"), Some("win.none"));
            return;
        }
        for p in recent {
            let label = Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
                .replace('_', "__"); // a lone underscore would become a mnemonic
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(Some("win.open-recent"), Some(&p.to_variant()));
            self.recent_menu.append_item(&item);
        }
    }

    fn persist_settings(&self) {
        save_settings(&self.settings.borrow());
    }

    fn apply_font(&self) {
        let pt = self.settings.borrow().font_pt;
        let css = format!("
            .editor {{
                {}
            }}

            button.file-tab-button {{
                border-radius: 6px 6px 0 0;
                font-weight: normal;
            }}

            .tabs-scroll scrollbar {{
                opacity: 0;
                min-width: 0;
                min-height: 0;
            }}
        ", if pt == 0 { String::new() } else { format!("font-size: {pt}pt;") });
        self.font_provider.load_from_string(&css);
    }

    /// delta > 0 zooms in, delta < 0 out, 0 resets to the system size.
    fn zoom(&self, delta: i32) {
        let current = self.settings.borrow().font_pt;
        let new = if delta == 0 {
            0
        } else {
            let base = if current == 0 {
                self.editor
                    .pango_context()
                    .font_description()
                    .map(|d| d.size() / gtk::pango::SCALE)
                    .filter(|&n| n >= 8)
                    .unwrap_or(11) as u32
            } else {
                current
            };
            (base as i32 + delta).clamp(6, 48) as u32
        };
        self.settings.borrow_mut().font_pt = new;
        self.apply_font();
        self.persist_settings();
    }

    fn apply_theme(&self, theme: &str) {
        let scheme = match theme {
            "light" => adw::ColorScheme::ForceLight,
            "dark" => adw::ColorScheme::ForceDark,
            _ => adw::ColorScheme::Default,
        };
        adw::StyleManager::default().set_color_scheme(scheme);
    }

    // ── edit menu ──

    fn undo(&self) {
        if self.buffer.can_undo() {
            self.buffer.undo();
        }
        self.editor.grab_focus();
    }

    fn redo(&self) {
        if self.buffer.can_redo() {
            self.buffer.redo();
        }
        self.editor.grab_focus();
    }

    fn clipboard(&self, signal: &str) {
        self.editor.grab_focus();
        self.editor.emit_by_name::<()>(signal, &[]);
    }

    fn select_all(&self) {
        let (s, e) = (self.buffer.start_iter(), self.buffer.end_iter());
        self.buffer.select_range(&s, &e);
        self.editor.grab_focus();
    }

    fn show_find(&self, replace: bool) {
        self.find_bar.set_search_mode(true);
        self.replace_row.set_visible(replace);
        if let Some((s, e)) = self.buffer.selection_bounds() {
            let t = self.buffer.text(&s, &e, false);
            if !t.is_empty() && !t.contains('\n') {
                self.find_entry.set_text(&t);
            }
        }
        self.find_entry.grab_focus();
    }

    /// Select the next / previous match. `incremental` keeps the current match when it still fits.
    fn find_next(&self, forward: bool, incremental: bool) {
        if self
            .search_settings
            .search_text()
            .map(|t| t.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        let buf = &self.buffer;
        let from = match buf.selection_bounds() {
            Some((s, e)) => {
                if forward && !incremental {
                    e
                } else {
                    s
                }
            }
            None => buf.iter_at_mark(&buf.get_insert()),
        };
        let found = if forward {
            self.search_ctx.forward(&from)
        } else {
            self.search_ctx.backward(&from)
        };
        match found {
            Some((mut s, e, _wrapped)) => {
                buf.select_range(&e, &s);
                self.editor.scroll_to_iter(&mut s, 0.15, false, 0.0, 0.0);
            }
            None => self.toast("No matches"),
        }
    }

    fn replace_one(&self) {
        let text = self.replace_entry.text().to_string();
        if let Some((mut s, mut e)) = self.buffer.selection_bounds() {
            // Fails harmlessly when the selection is not a match.
            let _ = self.search_ctx.replace(&mut s, &mut e, &text);
        }
        self.find_next(true, false);
    }

    fn replace_all(&self) {
        let text = self.replace_entry.text().to_string();
        let count = self.search_ctx.occurrences_count();
        match self.search_ctx.replace_all(&text) {
            Ok(()) if count > 0 => self.toast(&format!("Replaced {count} occurrence(s)")),
            Ok(()) => self.toast("Replaced all matches"),
            Err(e) => self.toast(&format!("Replace failed: {e}")),
        }
    }

    async fn goto_line(&self) {
        let total = self.buffer.line_count();
        let entry = gtk::Entry::builder()
            .placeholder_text("Line number")
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let d = adw::MessageDialog::new(
            Some(&self.win),
            Some("Go to line"),
            Some(&format!("Enter a line number from 1 to {total}.")),
        );
        d.add_responses(&[("cancel", "_Cancel"), ("go", "_Go")]);
        d.set_response_appearance("go", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("go"));
        d.set_close_response("cancel");
        d.set_extra_child(Some(&entry));
        gtk::prelude::GtkWindowExt::set_focus(&d, Some(&entry));
        {
            // Emitting a response by hand does not close the dialog, so close it as well.
            let d = d.clone();
            entry.connect_activate(move |_| {
                d.response("go");
                d.close();
            });
        }
        let answer = d.clone().choose_future().await;
        d.close();
        if answer.as_str() == "go" {
            if let Ok(n) = entry.text().trim().parse::<i32>() {
                let line = (n - 1).clamp(0, total - 1);
                if let Some(mut it) = self.buffer.iter_at_line(line) {
                    self.buffer.place_cursor(&it);
                    self.editor.scroll_to_iter(&mut it, 0.1, true, 0.0, 0.5);
                }
            }
        }
        self.editor.grab_focus();
    }

    fn line_text(&self, line: i32) -> String {
        let Some(a) = self.buffer.iter_at_line(line) else {
            return String::new();
        };
        let mut b = a;
        if !b.ends_line() {
            b.forward_to_line_end();
        }
        self.buffer.text(&a, &b, false).to_string()
    }

    fn toggle_comment(&self) {
        let buf = &self.buffer;
        let had_selection = buf.selection_bounds().is_some();
        let (s, e) = buf.selection_bounds().unwrap_or_else(|| {
            let i = buf.iter_at_mark(&buf.get_insert());
            (i, i)
        });
        let first = s.line();
        let mut last = e.line();
        if last > first && e.line_offset() == 0 {
            last -= 1; // selection ends at the start of a line: leave that line alone
        }
        let cursor_offset = buf.iter_at_mark(&buf.get_insert()).offset();
        let old: Vec<String> = (first..=last).map(|l| self.line_text(l)).collect();
        let new = toggle_comment_lines(&old);
        if new == old {
            return;
        }
        let (Some(mut a), Some(mut b)) = (buf.iter_at_line(first), buf.iter_at_line(last)) else {
            return;
        };
        if !b.ends_line() {
            b.forward_to_line_end();
        }
        let start_offset = a.offset();
        buf.begin_user_action();
        buf.delete(&mut a, &mut b);
        buf.insert(&mut a, &new.join("\n"));
        buf.end_user_action();
        if had_selection {
            let start = buf.iter_at_offset(start_offset);
            buf.select_range(&a, &start);
        } else {
            let delta = new[0].chars().count() as i32 - old[0].chars().count() as i32;
            let it = buf.iter_at_offset((cursor_offset + delta).max(start_offset));
            buf.place_cursor(&it);
        }
        self.editor.grab_focus();
    }

    fn auto_format(&self) {
        let text = self.editor_text();
        let name = self
            .path
            .borrow()
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sketch.cpp".into());
        match run_clang_format(&text, &name) {
            Ok(out) if out == text => self.toast("Already formatted"),
            Ok(out) => {
                let buf = &self.buffer;
                let line = buf.iter_at_mark(&buf.get_insert()).line();
                let (mut a, mut b) = (buf.start_iter(), buf.end_iter());
                buf.begin_user_action();
                buf.delete(&mut a, &mut b);
                buf.insert(&mut a, &out);
                buf.end_user_action();
                if let Some(it) = buf.iter_at_line(line.min(buf.line_count() - 1)) {
                    buf.place_cursor(&it);
                }
                self.toast("Formatted");
            }
            Err(e) => self.toast(&e),
        }
        self.editor.grab_focus();
    }

    // ── sketch menu ──

    async fn show_build_folder(&self) {
        let dir = self.last_build.borrow().clone();
        match dir {
            Some(d) if d.exists() => {
                let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&d)));
                if let Err(e) = launcher.launch_future(Some(&self.win)).await {
                    self.toast(&format!("Could not open the folder: {e}"));
                }
            }
            _ => self.toast("Verify the sketch first: there is no build folder yet"),
        }
    }

    async fn export_binaries(&self) {
        let dir = self.last_build.borrow().clone();
        let src = self.project_path.borrow().clone();
        let (Some(dir), Some(src)) = (dir, src) else {
            self.toast("Verify the sketch first");
            return;
        };
        let files: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && is_firmware_file(p))
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            self.toast("No compiled binaries found: verify the sketch first");
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Export compiled binaries to…")
            .accept_label("Export")
            .build();
        if let Some(parent) = src.parent() {
            dialog.set_initial_folder(Some(&gio::File::for_path(parent)));
        }
        let Ok(folder) = dialog.select_folder_future(Some(&self.win)).await else {
            return;
        };
        let Some(dest) = folder.path() else {
            return;
        };
        let mut copied = 0;
        for f in &files {
            if let Some(name) = f.file_name() {
                if fs::copy(f, dest.join(name)).is_ok() {
                    copied += 1;
                }
            }
        }
        self.toast(&format!("Exported {copied} file(s) to {}", dest.display()));
    }

    async fn add_file(&self) {
        let src = self.path.borrow().clone();
        let Some(src) = src else {
            self.toast("Save the sketch first, then add files next to it");
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Add file to sketch")
            .accept_label("Add")
            .build();
        let Ok(file) = dialog.open_future(Some(&self.win)).await else {
            return;
        };
        let (Some(from), Some(dir)) = (file.path(), src.parent()) else {
            return;
        };
        let Some(name) = from.file_name() else {
            return;
        };
        let dest = dir.join(name);
        if dest.exists() {
            self.toast("A file with that name already exists in the sketch folder");
            return;
        }
        match fs::copy(&from, &dest) {
            Ok(_) => {
                self.refresh_file_tabs();
                self.toast(&format!("Added {} to {}", name.to_string_lossy(), dir.display()));
            }
            Err(e) => self.toast(&format!("Could not add the file: {e}")),
        }
    }

    // ── tools / help ──

    fn show_serial(&self) {
        self.bottom.set_visible(true);
        if let Some(a) = self.win.lookup_action("bottom-panel") {
            a.change_state(&true.to_variant());
        }
        self.stack.set_visible_child_name("serial");
    }

    fn show_cli_version(&self) {
        self.bottom.set_visible(true);
        self.stack.set_visible_child_name("output");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut c = cli();
            c.arg("version");
            let msg = match run_capture(c) {
                Ok(s) => format!("{}\n", s.trim()),
                Err(e) => format!("arduino-cli: {e}\n"),
            };
            let _ = tx.send_blocking(Msg::Log(msg));
        });
    }

    fn show_shortcuts(&self) {
        let rows = [
            ("Ctrl+N", "New sketch"),
            ("Ctrl+O", "Open"),
            ("Ctrl+S", "Save"),
            ("Ctrl+Shift+S", "Save as"),
            ("Ctrl+Q", "Quit"),
            ("Ctrl+F", "Find"),
            ("Ctrl+H", "Find and replace"),
            ("F3 / Shift+F3", "Next / previous match"),
            ("Ctrl+L", "Go to line"),
            ("Ctrl+/", "Comment or uncomment lines"),
            ("Ctrl+T", "Auto format (clang-format)"),
            ("Ctrl+R", "Verify / compile"),
            ("Ctrl+U", "Upload"),
            ("Ctrl+Shift+B", "Board Manager"),
            ("Ctrl+Shift+M", "Serial monitor"),
            ("F5", "Refresh ports"),
            ("Ctrl+= / Ctrl+-", "Zoom in / out"),
            ("Ctrl+0", "Reset zoom"),
            ("F1", "This list"),
        ];
        let grid = gtk::Grid::builder()
            .column_spacing(24)
            .row_spacing(4)
            .halign(gtk::Align::Center)
            .build();
        for (i, (key, desc)) in rows.iter().enumerate() {
            let k = gtk::Label::builder()
                .label(*key)
                .xalign(0.0)
                .css_classes(["monospace"])
                .build();
            let v = gtk::Label::builder().label(*desc).xalign(0.0).build();
            grid.attach(&k, 0, i as i32, 1, 1);
            grid.attach(&v, 1, i as i32, 1, 1);
        }
        let d = adw::MessageDialog::new(Some(&self.win), Some("Keyboard shortcuts"), None);
        d.set_extra_child(Some(&grid));
        d.add_response("close", "_Close");
        d.set_default_response(Some("close"));
        d.set_close_response("close");
        d.present();
    }

    fn show_about(&self) {
        let about = adw::AboutWindow::builder()
            .transient_for(&self.win)
            .application_name("Arduino Lite")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A lightweight GTK4 front-end for arduino-cli.")
            .license_type(gtk::License::MitX11)
            .build();
        about.present();
    }

    // ── project file tabs ──

    fn supported_source_file(path: &Path) -> bool {
        matches!(
            path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
            Some("ino" | "cpp" | "c" | "h" | "hpp")
        )
    }

    fn project_files(&self) -> Vec<PathBuf> {
        let Some(active) = self.path.borrow().clone() else {
            return Vec::new();
        };
        let Some(dir) = active.parent() else {
            return vec![active];
        };
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && Self::supported_source_file(path))
            .collect();
        files.sort();
        if !files.contains(&active) {
            files.push(active);
            files.sort();
        }
        files
    }

    fn refresh_file_tabs(&self) {
        while let Some(child) = self.tabs_box.first_child() {
            self.tabs_box.remove(&child);
        }

        let files = self.project_files();
        let active = self.path.borrow().clone();
        for path in files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let is_active = active.as_ref() == Some(&path);
            let dirty = self.unsaved_buffers.borrow().contains_key(&path)
                || (is_active && self.modified.get());
            let label = if dirty { format!("{name} •") } else { name };
            let button = gtk::Button::with_label(&label);
            button.add_css_class("file-tab-button");
            button.set_tooltip_text(Some(&path.display().to_string()));
            if is_active {
                button.add_css_class("suggested-action");
            }
            let target = path.to_string_lossy().to_string();
            button.set_action_name(Some("win.open-tab"));
            button.set_action_target_value(Some(&target.to_variant()));
            self.tabs_box.append(&button);
        }
    }

    fn switch_to_tab(&self, path: &Path) {
        if self.path.borrow().as_deref() == Some(path) {
            return;
        }
        if let Some(current) = self.path.borrow().clone() {
            if self.modified.get() {
                self.unsaved_buffers.borrow_mut().insert(current, self.editor_text());
            }
        }
        let text = self.unsaved_buffers.borrow().get(path).cloned()
            .or_else(|| fs::read_to_string(path).ok());
        if let Some(text) = text {
            self.load_into_editor(&text, Some(path.to_path_buf()));
            self.refresh_file_tabs();
        } else {
            self.toast(&format!("Could not open {}", path.display()));
        }
    }

    fn reload_project_files(&self) {
        self.refresh_file_tabs();
        self.toast("Project files refreshed");
    }

    async fn create_project_file(&self) {
        let Some(current) = self.path.borrow().clone() else {
            self.toast("Open or save a sketch before creating a project file");
            return;
        };
        let Some(dir) = current.parent() else { return };
        let dialog = gtk::FileDialog::builder()
            .title("Create project file")
            .accept_label("Create")
            .initial_name("new_file.cpp")
            .build();
        dialog.set_initial_folder(Some(&gio::File::for_path(dir)));
        let Ok(file) = dialog.save_future(Some(&self.win)).await else { return };
        let Some(path) = file.path() else { return };
        if path.exists() {
            self.toast("A file with that name already exists");
            return;
        }
        match fs::write(&path, "") {
            Ok(()) => {
                self.switch_to_tab(&path);
                self.refresh_file_tabs();
                self.toast(&format!("Created {}", path.file_name().unwrap_or_default().to_string_lossy()));
            }
            Err(e) => self.toast(&format!("Could not create file: {e}")),
        }
    }

    // ── files ──

    fn load_into_editor(&self, text: &str, path: Option<PathBuf>) {
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(text);
        self.buffer.end_irreversible_action();
        self.buffer.place_cursor(&self.buffer.start_iter());
        *self.path.borrow_mut() = path;
        self.modified.set(false);
        self.refresh_title();
    }

    fn open_path(&self, path: &Path) {
        if let Some(current) = self.path.borrow().clone() {
            if self.modified.get() {
                self.unsaved_buffers.borrow_mut().insert(current, self.editor_text());
            }
        }
        match self.unsaved_buffers.borrow().get(path).cloned().or_else(|| fs::read_to_string(path).ok()) {
            Some(text) => {
                *self.project_path.borrow_mut() = Some(path.to_path_buf());
                self.load_into_editor(&text, Some(path.to_path_buf()));
                self.add_recent(path);
                self.refresh_file_tabs();
            }
            None => self.toast(&format!("Could not open {}", path.display())),
        }
    }

    fn write_to(&self, path: &Path) -> bool {
        match fs::write(path, self.editor_text()) {
            Ok(()) => {
                *self.path.borrow_mut() = Some(path.to_path_buf());
                if self.project_path.borrow().is_none() {
                    *self.project_path.borrow_mut() = Some(path.to_path_buf());
                }
                self.modified.set(false);
                self.unsaved_buffers.borrow_mut().remove(path);
                self.refresh_title();
                self.add_recent(path);
                self.refresh_file_tabs();
                true
            }
            Err(e) => {
                self.toast(&format!("Could not save: {e}"));
                false
            }
        }
    }

    async fn save_as(&self) -> bool {
        let dialog = gtk::FileDialog::builder()
            .title("Save sketch")
            .accept_label("Save")
            .initial_name("sketch.cpp")
            .build();
        if let Some(p) = self.path.borrow().clone() {
            dialog.set_initial_file(Some(&gio::File::for_path(p)));
        }
        let Ok(file) = dialog.save_future(Some(&self.win)).await else {
            return false;
        };
        let Some(mut path) = file.path() else {
            return false;
        };
        if path.extension().is_none() {
            path.set_extension("cpp");
        }
        self.write_to(&path)
    }

    async fn save(&self) -> bool {
        let path = self.path.borrow().clone();
        let active_ok = match path {
            None => self.save_as().await,
            Some(p) => {
                if self.modified.get() { self.write_to(&p) } else { true }
            }
        };
        if !active_ok { return false; }

        let cached = self.unsaved_buffers.borrow().clone();
        for (path, contents) in cached {
            if let Err(e) = fs::write(&path, contents) {
                self.toast(&format!("Could not save {}: {e}", path.display()));
                return false;
            }
            self.unsaved_buffers.borrow_mut().remove(&path);
        }
        self.refresh_file_tabs();
        true
    }

    /// Returns true when it is safe to replace / close the current document.
    async fn resolve_unsaved(&self) -> bool {
        if !self.modified.get() && self.unsaved_buffers.borrow().is_empty() {
            return true;
        }
        let d = adw::MessageDialog::new(
            Some(&self.win),
            Some("Save changes?"),
            Some("Your sketch has unsaved changes."),
        );
        d.add_responses(&[("cancel", "_Cancel"), ("discard", "_Discard"), ("save", "_Save")]);
        d.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("save"));
        d.set_close_response("cancel");
        match d.choose_future().await.as_str() {
            "save" => self.save().await,
            "discard" => {
                self.unsaved_buffers.borrow_mut().clear();
                true
            }
            _ => false,
        }
    }

    async fn new_sketch(&self) {
        if self.resolve_unsaved().await {
            self.unsaved_buffers.borrow_mut().clear();
            *self.project_path.borrow_mut() = None;
            self.load_into_editor(NEW_SKETCH, None);
            self.refresh_file_tabs();
        }
    }

    async fn open(&self) {
        if !self.resolve_unsaved().await {
            return;
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Sketches and sources"));
        for ext in ["cpp", "ino", "c", "h", "hpp"] {
            filter.add_suffix(ext);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Open sketch")
            .accept_label("Open")
            .filters(&filters)
            .build();
        if let Ok(file) = dialog.open_future(Some(&self.win)).await {
            if let Some(path) = file.path() {
                self.open_path(&path);
            }
        }
    }

    // ── build / upload ──

    async fn build(&self, upload: bool) {
        if self.busy.get() {
            return;
        }
        if self.mgr_busy.get() {
            self.toast("The Board Manager is still installing — wait for it to finish");
            return;
        }
        let Some(base_fqbn) = self.selected_fqbn() else {
            self.toast("Select a board first");
            return;
        };
        let fqbn = compose_fqbn(&base_fqbn, &self.options.borrow(), &self.picks.borrow());
        let port = if upload {
            match self.selected_port() {
                Some(p) => Some(p),
                None => {
                    self.toast("Select a port first (use the refresh button if it is missing)");
                    return;
                }
            }
        } else {
            None
        };
        if !self.save().await {
            return;
        }
        let Some(path) = self.project_path.borrow().clone() else {
            return;
        };
        // A second build may have been started while a dialog was open.
        if self.busy.get() {
            return;
        }
        let prep = match prepare_sketch(&path) {
            Ok(p) => p,
            Err(e) => {
                self.stack.set_visible_child_name("output");
                self.log(&format!("Could not prepare sketch: {e}\n"));
                return;
            }
        };
        *self.last_build.borrow_mut() = Some(prep.build_dir.clone());
        clean_outputs(&prep.build_dir);
        if upload {
            self.stop_monitor();
        }

        let mut cmd = cli();
        cmd.arg("compile")
            .arg("--fqbn")
            .arg(&fqbn)
            .arg("--build-path")
            .arg(&prep.build_dir);
        if let Some(p) = &port {
            cmd.arg("--upload").arg("--port").arg(p);
        }
        cmd.arg(&prep.sketch_dir).current_dir(&prep.sketch_dir);

        self.log_view.buffer().set_text("");
        self.stack.set_visible_child_name("output");
        if let Some(n) = &prep.note {
            self.log(n);
        }
        let shown: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        self.log(&format!("$ arduino-cli {}\n\n", shown.join(" ")));

        self.set_busy(true);
        run_task(
            cmd,
            if upload { "Upload" } else { "Verify" },
            self.tx.clone(),
        );
    }

    // ── boards & ports ──

    fn refresh_boards(&self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut c = cli();
            c.args(["board", "listall", "--format", "json"]);
            let res = run_capture(c).and_then(|s| parse_boards(&s));
            let _ = tx.send_blocking(Msg::Boards(res));
        });
    }

    fn refresh_ports(&self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut c = cli();
            c.args(["board", "list", "--format", "json"]);
            let res = run_capture(c).and_then(|s| parse_ports(&s));
            let _ = tx.send_blocking(Msg::Ports(res));
        });
    }

    fn set_boards(&self, list: Vec<Board>) {
        let saved = load_saved_board();
        let idx = saved.and_then(|f| list.iter().position(|b| b.fqbn == f));
        let labels: Vec<String> = list
            .iter()
            .map(|b| format!("{}", b.name))
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let empty = list.is_empty();

        self.loading_boards.set(true);
        *self.boards.borrow_mut() = list;
        self.board_model.splice(0, self.board_model.n_items(), &refs);
        self.board_dd.set_selected(match idx {
            Some(i) => i as u32,
            None => gtk::INVALID_LIST_POSITION,
        });
        self.loading_boards.set(false);
        self.board_changed();

        if empty {
            self.log(
                "No boards found. Install a core first, e.g.:\n  arduino-cli core update-index\n  arduino-cli core install arduino:avr\nthen press the board refresh button.\n",
            );
        }
    }

    /// The board selection changed: reload that board's configurable options.
    fn board_changed(&self) {
        self.options.borrow_mut().clear();
        self.picks.borrow_mut().clear();
        self.clear_options_ui();
        let Some(fqbn) = self.selected_fqbn() else {
            return;
        };
        *self.picks.borrow_mut() = load_picks(&fqbn);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut c = cli();
            c.arg("board")
                .arg("details")
                .arg("--fqbn")
                .arg(&fqbn)
                .arg("--format")
                .arg("json");
            let res = run_capture(c).and_then(|s| parse_options(&s));
            let _ = tx.send_blocking(Msg::BoardOptions(fqbn, res));
        });
    }

    fn clear_options_ui(&self) {
        while let Some(c) = self.options_box.first_child() {
            self.options_box.remove(&c);
        }
        self.options_btn.set_sensitive(false);
    }

    fn persist_picks(&self) {
        if let Some(f) = self.selected_fqbn() {
            save_picks(&f, &self.picks.borrow());
        }
    }

    fn render_options(self: Rc<Self>) {
        self.clear_options_ui();
        let options = self.options.borrow().clone();
        if options.is_empty() {
            self.options_box
                .append(&gtk::Label::new(Some("This board has no configurable options.")));
            return;
        }
        self.options_btn.set_sensitive(true);
        for opt in options {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .build();
            let label = gtk::Label::builder()
                .label(opt.label.as_str())
                .xalign(0.0)
                .hexpand(true)
                .build();
            let names: Vec<&str> = opt.values.iter().map(|v| v.label.as_str()).collect();
            let dd = gtk::DropDown::new(Some(gtk::StringList::new(&names)), gtk::Expression::NONE);
            let current = self.picks.borrow().get(&opt.id).cloned();
            let idx = opt
                .values
                .iter()
                .position(|v| Some(&v.value) == current.as_ref())
                .or_else(|| opt.values.iter().position(|v| v.default))
                .unwrap_or(0);
            dd.set_selected(idx as u32);
            let app = self.clone();
            let id = opt.id.clone();
            let values = opt.values.clone();
            dd.connect_selected_notify(move |dd| {
                if let Some(v) = values.get(dd.selected() as usize) {
                    app.picks.borrow_mut().insert(id.clone(), v.value.clone());
                    app.persist_picks();
                }
            });
            row.append(&label);
            row.append(&dd);
            self.options_box.append(&row);
        }
        let reset = gtk::Button::with_label("Reset to defaults");
        let app = self.clone();
        reset.connect_clicked(move |_| {
            app.picks.borrow_mut().clear();
            app.persist_picks();
            let app = app.clone();
            glib::idle_add_local_once(move || app.render_options());
        });
        self.options_box.append(&reset);
    }

    // ── board manager ──

    fn mgr_log(&self, text: &str) {
        let m = self.manager.borrow().clone();
        if let Some(m) = m {
            append(&m.log, text, LOG_CAP);
        }
    }

    fn mgr_spinner(&self, on: bool) {
        let m = self.manager.borrow().clone();
        if let Some(m) = m {
            m.spinner.set_spinning(on);
            m.spinner.set_visible(on);
        }
    }

    fn refresh_manager_data(&self) {
        self.mgr_spinner(true);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut c = cli();
            c.args(["config", "dump", "--format", "json"]);
            let urls = run_capture(c).and_then(|s| parse_urls(&s));
            let _ = tx.send_blocking(Msg::Urls(urls));
            let mut c = cli();
            c.args(["core", "search", "--format", "json"]);
            let plats = run_capture(c).and_then(|s| parse_platforms(&s));
            let _ = tx.send_blocking(Msg::Platforms(plats));
        });
    }

    /// Enable or disable the Board Manager's action buttons (the window itself stays usable).
    fn set_mgr_controls(&self, enabled: bool) {
        let m = self.manager.borrow().clone();
        if let Some(m) = m {
            for w in &m.controls {
                w.set_sensitive(enabled);
            }
        }
        for b in self
            .mgr_url_buttons
            .borrow()
            .iter()
            .chain(self.mgr_plat_buttons.borrow().iter())
        {
            b.set_sensitive(enabled);
        }
    }

    fn mgr_run(&self, steps: Vec<Vec<String>>, label: String) {
        if self.mgr_busy.get() {
            return;
        }
        let m = self.manager.borrow().clone();
        let Some(m) = m else {
            return;
        };
        self.mgr_busy.set(true);
        self.set_mgr_controls(false);
        self.mgr_spinner(true);

        let token = Arc::new(CancelToken::default());
        *self.mgr_cancel.borrow_mut() = Some(token.clone());

        append(&m.log, &format!("\n== {label} ==\n"), LOG_CAP);
        m.op_label.set_text(&label);
        m.status_label.set_text("Starting…");
        m.cancel_btn.set_sensitive(true);
        m.progress.set_fraction(0.0);
        m.progress_box.set_visible(true);

        // Pulse the bar and show elapsed time until this job finishes.
        let seq = self.mgr_seq.get() + 1;
        self.mgr_seq.set(seq);
        self.mgr_tick.set(seq);
        let tick = self.mgr_tick.clone();
        let started = Instant::now();
        let (bar, title, text) = (m.progress.clone(), m.op_label.clone(), label.clone());
        glib::timeout_add_local(Duration::from_millis(200), move || {
            if tick.get() != seq {
                return glib::ControlFlow::Break;
            }
            bar.pulse();
            let secs = started.elapsed().as_secs();
            title.set_text(&format!("{text} — {}:{:02}", secs / 60, secs % 60));
            glib::ControlFlow::Continue
        });

        run_steps(steps, label, self.tx.clone(), token);
    }

    fn mgr_cancel_now(&self) {
        let token = self.mgr_cancel.borrow().clone();
        if let Some(t) = token {
            cancel_token(&t);
        }
        let m = self.manager.borrow().clone();
        if let Some(m) = m {
            m.cancel_btn.set_sensitive(false);
            m.status_label.set_text("Cancelling…");
        }
    }

    fn mgr_add_url(&self) {
        let m = self.manager.borrow().clone();
        let Some(m) = m else {
            return;
        };
        let url = m.url_entry.text().trim().to_owned();
        if url.is_empty() {
            return;
        }
        if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")) {
            self.mgr_log("A board manager URL must start with https:// (or http://, file://).\n");
            return;
        }
        if self.urls.borrow().contains(&url) {
            self.mgr_log("That URL is already in the list.\n");
            return;
        }
        m.url_entry.set_text("");
        self.mgr_run(
            vec![
                args(&["config", "add", "board_manager.additional_urls", &url]),
                args(&["core", "update-index"]),
            ],
            format!("Add URL {url}"),
        );
    }

    fn open_manager(self: Rc<Self>) {
        let existing = self.manager.borrow().clone();
        let m = match existing {
            Some(m) => m,
            None => {
                let m = self.clone().build_manager();
                *self.manager.borrow_mut() = Some(m.clone());
                m
            }
        };
        m.window.present();
        self.refresh_manager_data();
    }

    fn build_manager(self: Rc<Self>) -> Manager {
        let window = adw::Window::builder()
            .title("Board Manager")
            .transient_for(&self.win)
            .default_width(780)
            .default_height(760)
            .hide_on_close(true)
            .build();

        let heading = |t: &str| {
            gtk::Label::builder()
                .label(t)
                .xalign(0.0)
                .css_classes(["heading"])
                .build()
        };

        // URLs
        let urls_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        urls_list.set_placeholder(Some(&gtk::Label::builder()
            .label("No additional URLs — only Arduino's official boards are available.")
            .margin_top(10)
            .margin_bottom(10)
            .build()));
        let urls_scroll = gtk::ScrolledWindow::builder()
            .child(&urls_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(150)
            .build();
        let url_entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("https://example.com/package_index.json")
            .build();
        let add_btn = gtk::Button::builder()
            .label("Add")
            .css_classes(["suggested-action"])
            .build();
        let update_btn = gtk::Button::with_label("Update Index");
        let url_row = gtk::Box::builder().spacing(6).build();
        url_row.append(&url_entry);
        url_row.append(&add_btn);
        url_row.append(&update_btn);

        // Platforms
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Filter boards and platforms")
            .build();
        let platform_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_bottom(2)
            .build();
        platform_list.set_placeholder(Some(&gtk::Label::builder()
            .label("No platforms found. Add a URL or press Update Index.")
            .margin_top(14)
            .margin_bottom(14)
            .build()));
        {
            let search = search.clone();
            platform_list.set_filter_func(move |row| {
                let q = search.text().to_lowercase();
                q.is_empty() || row.widget_name().contains(q.as_str())
            });
        }
        {
            let list = platform_list.clone();
            search.connect_search_changed(move |_| list.invalidate_filter());
        }
        let platform_scroll = gtk::ScrolledWindow::builder()
            .child(&platform_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .min_content_height(200)
            .build();

        // Log
        let log = text_view();
        let log_scroll = gtk::ScrolledWindow::builder()
            .child(&log)
            .height_request(130)
            .build();

        // Progress / status / cancel (shown only while a job runs)
        let op_label = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let texts = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        texts.append(&op_label);
        texts.append(&status_label);
        let cancel_btn = gtk::Button::builder()
            .label("Cancel")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        let top = gtk::Box::builder().spacing(12).build();
        top.append(&texts);
        top.append(&cancel_btn);
        let progress = gtk::ProgressBar::builder().pulse_step(0.08).build();
        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .visible(false)
            .build();
        progress_box.append(&top);
        progress_box.append(&progress);

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_start(14)
            .margin_end(14)
            .margin_top(10)
            .margin_bottom(14)
            .build();
        body.append(&heading("Additional Boards Manager URLs"));
        body.append(&urls_scroll);
        body.append(&url_row);
        body.append(&heading("Platforms"));
        body.append(&search);
        body.append(&platform_scroll);
        body.append(&progress_box);
        body.append(&heading("Output"));
        body.append(&log_scroll);

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let header = adw::HeaderBar::new();
        header.pack_end(&spinner);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        window.set_content(Some(&toolbar));

        {
            let app = self.clone();
            cancel_btn.connect_clicked(move |_| app.mgr_cancel_now());
        }
        let controls: Vec<gtk::Widget> = vec![
            add_btn.clone().upcast::<gtk::Widget>(),
            update_btn.clone().upcast::<gtk::Widget>(),
            url_entry.clone().upcast::<gtk::Widget>(),
        ];
        {
            let app = self.clone();
            add_btn.connect_clicked(move |_| app.mgr_add_url());
        }
        {
            let app = self.clone();
            url_entry.connect_activate(move |_| app.mgr_add_url());
        }
        {
            let app = self.clone();
            update_btn.connect_clicked(move |_| {
                app.mgr_run(vec![args(&["core", "update-index"])], "Update index".into());
            });
        }

        Manager {
            window,
            controls,
            progress_box,
            op_label,
            status_label,
            progress,
            cancel_btn,
            urls_list,
            url_entry,
            platform_list,
            log,
            spinner,
        }
    }

    fn render_urls(self: Rc<Self>) {
        let m = self.manager.borrow().clone();
        let Some(m) = m else {
            return;
        };
        while let Some(c) = m.urls_list.first_child() {
            m.urls_list.remove(&c);
        }
        let urls = self.urls.borrow().clone();
        let mut made: Vec<gtk::Button> = Vec::new();
        for url in urls {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&url))
                .build();
            let rm = gtk::Button::builder()
                .label("Remove")
                .valign(gtk::Align::Center)
                .build();
            let app = self.clone();
            let u = url.clone();
            rm.connect_clicked(move |_| {
                app.mgr_run(
                    vec![
                        args(&["config", "remove", "board_manager.additional_urls", &u]),
                        args(&["core", "update-index"]),
                    ],
                    format!("Remove URL {u}"),
                );
            });
            rm.set_sensitive(!self.mgr_busy.get());
            made.push(rm.clone());
            row.add_suffix(&rm);
            m.urls_list.append(&row);
        }
        *self.mgr_url_buttons.borrow_mut() = made;
    }

    fn render_platforms(self: Rc<Self>) {
        let m = self.manager.borrow().clone();
        let Some(m) = m else {
            return;
        };
        while let Some(c) = m.platform_list.first_child() {
            m.platform_list.remove(&c);
        }
        let platforms = self.platforms.borrow().clone();
        let mut made: Vec<gtk::Button> = Vec::new();
        for p in platforms {
            let status = match &p.installed {
                Some(v) if !p.latest.is_empty() && *v != p.latest => {
                    format!("installed {v}, {} available", p.latest)
                }
                Some(v) => format!("installed {v}"),
                None => format!("latest {}", p.latest),
            };
            let mut parts = vec![p.id.clone()];
            if !p.maintainer.is_empty() {
                parts.push(p.maintainer.clone());
            }
            parts.push(status);
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&p.name))
                .subtitle(glib::markup_escape_text(&parts.join(" · ")))
                .build();
            row.set_widget_name(&format!("{} {} {}", p.id, p.name, p.maintainer).to_lowercase());

            let buttons = gtk::Box::builder()
                .spacing(6)
                .valign(gtk::Align::Center)
                .build();
            let button = |label: &str, css: &[&str]| {
                gtk::Button::builder().label(label).css_classes(css.to_vec()).build()
            };
            match &p.installed {
                None => {
                    let b = button("Install", &["suggested-action"]);
                    let app = self.clone();
                    let id = p.id.clone();
                    b.connect_clicked(move |_| {
                        app.mgr_run(
                            vec![args(&["core", "install", id.as_str()])],
                            format!("Install {id}"),
                        );
                    });
                    b.set_sensitive(!self.mgr_busy.get());
                    made.push(b.clone());
                    buttons.append(&b);
                }
                Some(v) => {
                    if !p.latest.is_empty() && *v != p.latest {
                        let b = button("Upgrade", &[]);
                        let app = self.clone();
                        let id = p.id.clone();
                        b.connect_clicked(move |_| {
                            app.mgr_run(
                                vec![args(&["core", "upgrade", id.as_str()])],
                                format!("Upgrade {id}"),
                            );
                        });
                        b.set_sensitive(!self.mgr_busy.get());
                    made.push(b.clone());
                    buttons.append(&b);
                    }
                    let b = button("Uninstall", &["destructive-action"]);
                    let app = self.clone();
                    let id = p.id.clone();
                    b.connect_clicked(move |_| {
                        app.mgr_run(
                            vec![args(&["core", "uninstall", id.as_str()])],
                            format!("Uninstall {id}"),
                        );
                    });
                    b.set_sensitive(!self.mgr_busy.get());
                    made.push(b.clone());
                    buttons.append(&b);
                }
            }
            row.add_suffix(&buttons);
            m.platform_list.append(&row);
        }
        *self.mgr_plat_buttons.borrow_mut() = made;
    }

    fn set_ports(&self, list: Vec<PortInfo>) {
        let prev = self.selected_port();
        let labels: Vec<String> = list.iter().map(|p| p.label.clone()).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let idx = prev
            .and_then(|p| list.iter().position(|x| x.address == p))
            .or(if list.is_empty() { None } else { Some(0) });
        *self.ports.borrow_mut() = list;
        self.port_model.splice(0, self.port_model.n_items(), &refs);
        self.port_dd.set_selected(match idx {
            Some(i) => i as u32,
            None => gtk::INVALID_LIST_POSITION,
        });
    }

    // ── serial monitor ──

    fn set_monitor_ui(&self, on: bool) {
        self.suppress_toggle.set(true);
        self.monitor_btn.set_active(on);
        self.monitor_btn.set_label(if on { "Disconnect" } else { "Connect" });
        self.suppress_toggle.set(false);
    }

    fn start_monitor(&self) -> bool {
        if self.busy.get() {
            self.toast("Wait for the current build or upload to finish");
            return false;
        }
        let Some(port) = self.selected_port() else {
            self.toast("Select a port first");
            return false;
        };
        self.stop_monitor();
        let idx = (self.baud_dd.selected() as usize).min(BAUDS.len() - 1);
        let baud = BAUDS[idx];

        let mut cmd = cli();
        cmd.arg("monitor")
            .arg("--port")
            .arg(&port)
            .arg("--config")
            .arg(format!("baudrate={baud}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match cmd.spawn() {
            Ok(mut child) => {
                let generation = self.monitor_gen.get() + 1;
                self.monitor_gen.set(generation);
                if let Some(o) = child.stdout.take() {
                    pump_monitor(o, self.tx.clone(), generation, true);
                }
                if let Some(e) = child.stderr.take() {
                    pump_monitor(e, self.tx.clone(), generation, false);
                }
                let stdin = child.stdin.take();
                *self.monitor.borrow_mut() = Some(Monitor { child, stdin });
                self.serial_view.buffer().set_text("");
                append(
                    &self.serial_view,
                    &format!("— connected to {port} @ {baud} baud —\n"),
                    SERIAL_CAP,
                );
                self.set_monitor_ui(true);
                true
            }
            Err(e) => {
                self.toast(&format!("Could not start serial monitor: {e}"));
                self.set_monitor_ui(false);
                false
            }
        }
    }

    fn stop_monitor(&self) {
        self.monitor_gen.set(self.monitor_gen.get() + 1);
        let m = self.monitor.borrow_mut().take();
        if let Some(m) = m {
            terminate(m);
        }
        self.set_monitor_ui(false);
    }

    fn send_serial(&self) {
        let mut data = self.serial_entry.text().to_string().into_bytes();
        data.extend_from_slice(line_ending(self.ending_dd.selected()));
        if data.is_empty() {
            return;
        }
        let mut guard = self.monitor.borrow_mut();
        match guard.as_mut().and_then(|m| m.stdin.as_mut()) {
            Some(si) => {
                let res = (|| -> std::io::Result<()> {
                    si.write_all(&data)?;
                    si.flush()
                })();
                match res {
                    Ok(()) => self.serial_entry.set_text(""),
                    Err(e) => self.toast(&format!("Send failed: {e}")),
                }
            }
            None => self.toast("Serial monitor is not connected"),
        }
    }

    // ── message pump ──

    fn handle(self: Rc<Self>, msg: Msg) {
        match msg {
            Msg::Log(s) => self.log(&s),
            Msg::TaskDone { ok, label } => {
                self.set_busy(false);
                if ok {
                    self.log(&format!("\n✔ {label} succeeded\n"));
                    self.toast(&format!("{label} succeeded"));
                } else {
                    self.log(&format!("\n✘ {label} failed\n"));
                    self.toast(&format!("{label} failed — see Output"));
                }
            }
            Msg::Boards(Ok(list)) => self.set_boards(list),
            Msg::Boards(Err(e)) => self.log(&format!("Could not load boards: {e}\n")),
            Msg::Ports(Ok(list)) => self.set_ports(list),
            Msg::Ports(Err(e)) => self.log(&format!("Could not list ports: {e}\n")),
            Msg::BoardOptions(fqbn, res) => {
                // Ignore answers for a board that is no longer selected.
                if self.selected_fqbn().as_deref() != Some(fqbn.as_str()) {
                    return;
                }
                match res {
                    Ok(opts) => {
                        self.picks.borrow_mut().retain(|k, v| {
                            opts.iter()
                                .any(|o| &o.id == k && o.values.iter().any(|x| &x.value == v))
                        });
                        *self.options.borrow_mut() = opts;
                        self.persist_picks();
                        self.render_options();
                    }
                    Err(e) => self.log(&format!("Could not load board options: {e}\n")),
                }
            }
            Msg::Urls(Ok(list)) => {
                *self.urls.borrow_mut() = list;
                self.render_urls();
            }
            Msg::Urls(Err(e)) => self.mgr_log(&format!("Could not read configuration: {e}\n")),
            Msg::Platforms(res) => {
                if !self.mgr_busy.get() {
                    self.mgr_spinner(false);
                }
                match res {
                    Ok(list) => {
                        *self.platforms.borrow_mut() = list;
                        self.render_platforms();
                    }
                    Err(e) => self.mgr_log(&format!("Could not list platforms: {e}\n")),
                }
            }
            Msg::MgrLog(s) => {
                self.mgr_log(&s);
                let cancelling = self
                    .mgr_cancel
                    .borrow()
                    .as_ref()
                    .map(|t| t.cancelled.load(Ordering::SeqCst))
                    .unwrap_or(false);
                if self.mgr_busy.get() && !cancelling {
                    if let Some(line) = s.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                        let m = self.manager.borrow().clone();
                        if let Some(m) = m {
                            m.status_label.set_text(line);
                        }
                    }
                }
            }
            Msg::MgrDone { ok, label, cancelled } => {
                self.mgr_busy.set(false);
                self.mgr_tick.set(0);
                *self.mgr_cancel.borrow_mut() = None;
                self.set_mgr_controls(true);
                let m = self.manager.borrow().clone();
                if let Some(m) = m {
                    m.progress_box.set_visible(false);
                }
                let mark = if ok {
                    "✔ done:"
                } else if cancelled {
                    "■ cancelled:"
                } else {
                    "✘ failed:"
                };
                self.mgr_log(&format!("{mark} {label}\n"));
                if cancelled {
                    self.mgr_log(
                        "A cancelled download can leave partial files behind. If anything looks off, run the action again.\n",
                    );
                }
                self.refresh_manager_data();
                if ok {
                    self.refresh_boards();
                }
            }
            Msg::Monitor(g, s) => {
                if g == self.monitor_gen.get() {
                    append(&self.serial_view, &s.replace('\r', ""), SERIAL_CAP);
                }
            }
            Msg::MonitorEnded(g) => {
                if g == self.monitor_gen.get() {
                    self.monitor_gen.set(g + 1);
                    let m = self.monitor.borrow_mut().take();
                    if let Some(m) = m {
                        terminate(m);
                    }
                    self.set_monitor_ui(false);
                    append(&self.serial_view, "\n— disconnected —\n", SERIAL_CAP);
                }
            }
        }
    }
}

// ───────────────────────────── window construction ─────────────────────────

fn build_ui(application: &adw::Application) {
    let (tx, rx) = async_channel::unbounded::<Msg>();

    // Editor
    let buffer = sourceview5::Buffer::builder()
        .highlight_syntax(true)
        .highlight_matching_brackets(true)
        .build();
    buffer.set_language(sourceview5::LanguageManager::default().language("cpp").as_ref());
    apply_scheme(&buffer);
    {
        let b = buffer.clone();
        adw::StyleManager::default().connect_dark_notify(move |_| apply_scheme(&b));
    }
    buffer.set_text(NEW_SKETCH);

    let editor = sourceview5::View::with_buffer(&buffer);
    editor.set_show_line_numbers(true);
    editor.set_highlight_current_line(true);
    editor.set_auto_indent(true);
    editor.set_indent_width(2);
    editor.set_tab_width(2);
    editor.set_insert_spaces_instead_of_tabs(true);
    editor.set_smart_backspace(true);
    editor.set_monospace(true);
    editor.set_top_margin(6);
    editor.set_bottom_margin(6);
    editor.set_left_margin(6);
    editor.set_right_margin(6);
    editor.add_css_class("editor");

    // Search (GtkSourceView does the matching and highlighting)
    let search_settings = sourceview5::SearchSettings::new();
    search_settings.set_wrap_around(true);
    let search_ctx = sourceview5::SearchContext::new(&buffer, Some(&search_settings));
    let find_entry = gtk::SearchEntry::builder()
        .hexpand(true)
        .placeholder_text("Find")
        .build();
    let find_prev_btn = gtk::Button::with_label("Previous");
    let find_next_btn = gtk::Button::with_label("Next");
    let replace_entry = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("Replace with")
        .build();
    let replace_btn = gtk::Button::with_label("Replace");
    let replace_all_btn = gtk::Button::with_label("Replace All");
    let find_row = gtk::Box::builder().spacing(6).build();
    find_row.append(&find_entry);
    find_row.append(&find_prev_btn);
    find_row.append(&find_next_btn);
    let replace_row = gtk::Box::builder().spacing(6).visible(false).build();
    replace_row.append(&replace_entry);
    replace_row.append(&replace_btn);
    replace_row.append(&replace_all_btn);
    let find_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    find_col.append(&find_row);
    find_col.append(&replace_row);
    let find_bar = gtk::SearchBar::builder()
        .child(&find_col)
        .show_close_button(true)
        .build();
    find_bar.connect_entry(&find_entry);
    let tabs_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .hexpand(false)
        .build();
    let tabs_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .min_content_height(34)
        .css_classes(["tabs-scroll"])
        .child(&tabs_box)
        .build();
    let reload_files_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reload project files")
        .action_name("win.reload-files")
        .build();
    let create_file_btn = gtk::Button::builder()
        .icon_name("document-new-symbolic")
        .tooltip_text("Create new project file")
        .action_name("win.create-file")
        .build();
    let tabs_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_start(6)
        .margin_end(6)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    tabs_bar.append(&tabs_scroll);
    tabs_bar.append(&reload_files_btn);
    tabs_bar.append(&create_file_btn);

    let editor_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    editor_box.append(&tabs_bar);
    editor_box.append(&find_bar);
    editor_box.append(&scrolled(&editor));

    // Output + serial pages
    let log_view = text_view();
    let serial_view = text_view();
    let serial_entry = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("Send to board")
        .build();
    let send_btn = gtk::Button::with_label("Send");
    let baud_model = gtk::StringList::new(&BAUDS);
    let baud_dd = string_dropdown(&baud_model, false);
    baud_dd.set_selected(0);
    let ending_model = gtk::StringList::new(&[
        "No line ending",
        "Newline (LF)",
        "Carriage return (CR)",
        "Both (CR+LF)",
    ]);
    let ending_dd = string_dropdown(&ending_model, false);
    ending_dd.set_selected(1);
    ending_dd.set_tooltip_text(Some("Line ending appended to what you send"));
    let monitor_btn = gtk::ToggleButton::with_label("Connect");

    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    controls.append(&serial_entry);
    controls.append(&ending_dd);
    controls.append(&send_btn);
    controls.append(&baud_dd);
    controls.append(&monitor_btn);

    let serial_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    serial_page.append(&scrolled(&serial_view));
    serial_page.append(&controls);

    let stack = adw::ViewStack::new();
    stack.add_titled(&scrolled(&log_view), Some("output"), "Output");
    stack.add_titled(&serial_page, Some("serial"), "Serial Monitor");
    // Plain text tabs: no icon names that a given icon theme might lack.
    let tab_output = gtk::ToggleButton::builder()
        .label("Output")
        .active(true)
        .build();
    let tab_serial = gtk::ToggleButton::builder()
        .label("Serial Monitor")
        .group(&tab_output)
        .build();
    {
        let st = stack.clone();
        tab_output.connect_toggled(move |b| {
            if b.is_active() {
                st.set_visible_child_name("output");
            }
        });
    }
    {
        let st = stack.clone();
        tab_serial.connect_toggled(move |b| {
            if b.is_active() {
                st.set_visible_child_name("serial");
            }
        });
    }
    {
        let (o, sr) = (tab_output.clone(), tab_serial.clone());
        stack.connect_visible_child_name_notify(move |st| match st.visible_child_name().as_deref() {
            Some("serial") => sr.set_active(true),
            _ => o.set_active(true),
        });
    }
    let switcher = gtk::Box::builder()
        //.css_classes(["linked"])
        .halign(gtk::Align::Start)
        .spacing(6)
        .margin_start(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    switcher.append(&tab_output);
    switcher.append(&tab_serial);
    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&switcher);
    bottom.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    bottom.append(&stack);

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .wide_handle(true)
        .vexpand(true)
        .start_child(&editor_box)
        .end_child(&bottom)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(450)
        .build();

    // Board / port bar
    //let board_model = gtk::StringList::new(&[]);
    //let board_dd = string_dropdown(&board_model, true);
    //board_dd.set_hexpand(true);
    let board_model = gtk::StringList::new(&[]);
    let board_dd = string_dropdown(&board_model, true);

    board_dd.set_width_request(300);
    board_dd.set_hexpand(false);
    board_dd.set_halign(gtk::Align::Start);
    
    let port_model = gtk::StringList::new(&[]);
    let port_dd = string_dropdown(&port_model, false);
    port_dd.set_width_request(150);
    let options_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_start(14)
        .margin_end(14)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    let options_scroll = gtk::ScrolledWindow::builder()
        .child(&options_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(420)
        .build();
    let options_popover = gtk::Popover::builder().child(&options_scroll).build();
    let options_btn = gtk::MenuButton::builder()
        .label("Options")
        .always_show_arrow(true)
        .tooltip_text("Board options (CPU speed, USB mode, …)")
        .popover(&options_popover)
        .sensitive(false)
        .build();
    let refresh_boards_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reload board list")
        .build();
    let refresh_ports_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh ports (F5)")
        .action_name("win.refresh-ports")
        .build();

    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    bar.append(&gtk::Label::new(Some("Board")));
    bar.append(&board_dd);
    bar.append(&refresh_boards_btn);
    bar.append(&options_btn);
    
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bar.append(&spacer);
    
    bar.append(&gtk::Label::new(Some("Port")));
    bar.append(&port_dd);
    bar.append(&refresh_ports_btn);

    // Header
    let title = adw::WindowTitle::new("Untitled", "");
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);

    let verify_btn = gtk::Button::builder()
        .child(&icon_label("object-select-symbolic", "Verify"))
        .action_name("win.verify")
        .tooltip_text("Compile (Ctrl+R)")
        .build();
    let upload_btn = gtk::Button::builder()
        .child(&icon_label("go-up-symbolic", "Upload"))
        .action_name("win.upload")
        .tooltip_text("Compile and upload (Ctrl+U)")
        .css_classes(["suggested-action"])
        .build();

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(
        &gtk::Button::builder()
            .label("New")
            .action_name("win.new")
            .tooltip_text("New sketch (Ctrl+N)")
            .build(),
    );
    for (icon, action, tip) in [
        ("document-open-symbolic", "win.open", "Open (Ctrl+O)"),
        ("document-save-symbolic", "win.save", "Save (Ctrl+S)"),
    ] {
        header.pack_start(
            &gtk::Button::builder()
                .icon_name(icon)
                .action_name(action)
                .tooltip_text(tip)
                .build(),
        );
    }
    header.pack_start(&verify_btn);
    header.pack_start(&upload_btn);
    let recent_menu = gio::Menu::new();
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main menu (F10)")
        .menu_model(&build_menu_model(&recent_menu))
        .build();
    header.pack_end(&menu_btn);
    header.pack_end(&spinner);
    header.pack_end(
        &gtk::Button::builder()
            .label("Board Manager")
            .action_name("win.board-manager")
            .tooltip_text("Install board packages and add custom URLs (Ctrl+Shift+B)")
            .build(),
    );

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&bar);
    toolbar.set_content(Some(&paned));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let win = adw::ApplicationWindow::builder()
        .application(application)
        .default_width(1100)
        .default_height(760)
        .content(&toasts)
        .build();

    // Actions that the state struct needs to own.
    let verify_action = gio::SimpleAction::new("verify", None);
    let upload_action = gio::SimpleAction::new("upload", None);
    win.add_action(&verify_action);
    win.add_action(&upload_action);

    let font_provider = gtk::CssProvider::new();
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &font_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let app = Rc::new(App {
        win: win.clone(),
        title,
        toasts,
        spinner,
        buffer: buffer.clone(),
        log_view,
        serial_view,
        serial_entry: serial_entry.clone(),
        stack,
        board_model,
        board_dd: board_dd.clone(),
        port_model,
        port_dd,
        options_box,
        options_btn,
        baud_dd: baud_dd.clone(),
        monitor_btn: monitor_btn.clone(),
        verify_action: verify_action.clone(),
        upload_action: upload_action.clone(),
        tx,
        path: RefCell::new(None),
        project_path: RefCell::new(None),
        modified: Cell::new(false),
        tabs_box: tabs_box.clone(),
        unsaved_buffers: RefCell::new(BTreeMap::new()),
        busy: Cell::new(false),
        loading_boards: Cell::new(false),
        suppress_toggle: Cell::new(false),
        boards: RefCell::new(Vec::new()),
        ports: RefCell::new(Vec::new()),
        ending_dd,
        editor: editor.clone(),
        bottom: bottom.clone(),
        find_bar: find_bar.clone(),
        find_entry: find_entry.clone(),
        replace_entry: replace_entry.clone(),
        replace_row: replace_row.clone(),
        search_ctx: search_ctx.clone(),
        search_settings: search_settings.clone(),
        recent_menu: recent_menu.clone(),
        recent: RefCell::new(load_recent()),
        settings: RefCell::new(load_settings()),
        font_provider: font_provider.clone(),
        last_build: RefCell::new(None),
        manager: RefCell::new(None),
        mgr_busy: Cell::new(false),
        mgr_cancel: RefCell::new(None),
        mgr_tick: Rc::new(Cell::new(0)),
        mgr_seq: Cell::new(0),
        mgr_url_buttons: RefCell::new(Vec::new()),
        mgr_plat_buttons: RefCell::new(Vec::new()),
        platforms: RefCell::new(Vec::new()),
        urls: RefCell::new(Vec::new()),
        options: RefCell::new(Vec::new()),
        picks: RefCell::new(BTreeMap::new()),
        monitor: RefCell::new(None),
        monitor_gen: Cell::new(0),
    });
    app.refresh_title();
    app.refresh_file_tabs();
    app.rebuild_recent_menu();
    {
        let st = app.settings.borrow().clone();
        editor.set_show_line_numbers(st.line_numbers);
        editor.set_wrap_mode(if st.wrap {
            gtk::WrapMode::WordChar
        } else {
            gtk::WrapMode::None
        });
        app.apply_theme(&st.theme);
        app.apply_font();
    }

    // Message pump
    {
        let app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(m) = rx.recv().await {
                app.clone().handle(m);
            }
        });
    }

    // Buffer change tracking
    {
        let app = app.clone();
        buffer.connect_changed(move |_| {
            if !app.modified.get() {
                app.modified.set(true);
                app.refresh_title();
                app.refresh_file_tabs();
            }
        });
    }

    // Async actions
    macro_rules! async_action {
        ($action:expr, |$a:ident| $body:expr) => {{
            let app = app.clone();
            $action.connect_activate(move |_, _| {
                let $a = app.clone();
                glib::spawn_future_local(async move {
                    $body.await;
                });
            });
        }};
    }

    let new_action = gio::SimpleAction::new("new", None);
    let open_action = gio::SimpleAction::new("open", None);
    let save_action = gio::SimpleAction::new("save", None);
    let save_as_action = gio::SimpleAction::new("save-as", None);
    let refresh_ports_action = gio::SimpleAction::new("refresh-ports", None);
    let manager_action = gio::SimpleAction::new("board-manager", None);
    for a in [
        &new_action,
        &open_action,
        &save_action,
        &save_as_action,
        &refresh_ports_action,
        &manager_action,
    ] {
        win.add_action(a);
    }
    async_action!(new_action, |a| a.new_sketch());
    async_action!(open_action, |a| a.open());
    async_action!(save_action, |a| a.save());
    async_action!(save_as_action, |a| a.save_as());
    async_action!(verify_action, |a| a.build(false));
    async_action!(upload_action, |a| a.build(true));
    {
        let app = app.clone();
        refresh_ports_action.connect_activate(move |_, _| app.refresh_ports());
    }
    {
        let app = app.clone();
        manager_action.connect_activate(move |_, _| app.clone().open_manager());
    }

    // F10 opens the main (hamburger) menu
    {
        let mb = menu_btn.clone();
        let act = gio::SimpleAction::new("main-menu", None);
        act.connect_activate(move |_, _| mb.popup());
        win.add_action(&act);
        application.set_accels_for_action("win.main-menu", &["F10"]);
    }

    // ── menu actions ──
    macro_rules! act {
        ($name:expr, |$a:ident| $body:expr) => {{
            let act = gio::SimpleAction::new($name, None);
            let app = app.clone();
            act.connect_activate(move |_, _| {
                let $a = &app;
                $body;
            });
            win.add_action(&act);
            act
        }};
    }
    macro_rules! async_act {
        ($name:expr, |$a:ident| $body:expr) => {{
            let act = gio::SimpleAction::new($name, None);
            let app = app.clone();
            act.connect_activate(move |_, _| {
                let $a = app.clone();
                glib::spawn_future_local(async move {
                    $body.await;
                });
            });
            win.add_action(&act);
            act
        }};
    }

    act!("quit", |a| a.win.close());
    let undo_action = act!("undo", |a| a.undo());
    let redo_action = act!("redo", |a| a.redo());
    undo_action.set_enabled(false);
    redo_action.set_enabled(false);
    {
        let u = undo_action.clone();
        buffer.connect_can_undo_notify(move |b| u.set_enabled(b.can_undo()));
        let r = redo_action.clone();
        buffer.connect_can_redo_notify(move |b| r.set_enabled(b.can_redo()));
    }
    act!("cut", |a| a.clipboard("cut-clipboard"));
    act!("copy", |a| a.clipboard("copy-clipboard"));
    act!("paste", |a| a.clipboard("paste-clipboard"));
    act!("select-all", |a| a.select_all());
    act!("find", |a| a.show_find(false));
    act!("replace", |a| a.show_find(true));
    act!("find-next", |a| a.find_next(true, false));
    act!("find-prev", |a| a.find_next(false, false));
    act!("comment", |a| a.toggle_comment());
    act!("format", |a| a.auto_format());
    async_act!("goto-line", |a| a.goto_line());
    async_act!("show-build-folder", |a| a.show_build_folder());
    async_act!("export-binaries", |a| a.export_binaries());
    async_act!("add-file", |a| a.add_file());
    async_act!("create-file", |a| a.create_project_file());
    act!("reload-files", |a| a.reload_project_files());
    {
        let app = app.clone();
        let open_tab = gio::SimpleAction::new("open-tab", Some(glib::VariantTy::STRING));
        open_tab.connect_activate(move |_, value| {
            let Some(path) = value.and_then(|v| v.get::<String>()) else { return };
            app.switch_to_tab(Path::new(&path));
        });
        win.add_action(&open_tab);
    }
    act!("board-options", |a| {
        if a.options_btn.is_sensitive() {
            a.options_btn.popup();
        } else {
            a.toast("Select a board first (or this board has no options)");
        }
    });
    act!("reload-boards", |a| a.refresh_boards());
    act!("serial-monitor", |a| a.show_serial());
    act!("zoom-in", |a| a.zoom(1));
    act!("zoom-out", |a| a.zoom(-1));
    act!("zoom-reset", |a| a.zoom(0));
    act!("shortcuts", |a| a.show_shortcuts());
    act!("cli-version", |a| a.show_cli_version());
    act!("about", |a| a.show_about());

    // Open Recent (takes the file path as a string parameter)
    {
        let open_recent = gio::SimpleAction::new("open-recent", Some(glib::VariantTy::STRING));
        let app = app.clone();
        open_recent.connect_activate(move |_, p| {
            let Some(path) = p.and_then(|v| v.get::<String>()) else {
                return;
            };
            let app = app.clone();
            glib::spawn_future_local(async move {
                if app.resolve_unsaved().await {
                    app.open_path(Path::new(&path));
                }
            });
        });
        win.add_action(&open_recent);
    }

    // View toggles keep their state (shown as check marks in the menu) and are remembered.
    {
        let st = app.settings.borrow().clone();

        let wrap = gio::SimpleAction::new_stateful("wrap", None, &st.wrap.to_variant());
        let a = app.clone();
        wrap.connect_change_state(move |act, v| {
            let Some(v) = v else { return };
            let Some(on) = v.get::<bool>() else { return };
            act.set_state(v);
            a.editor.set_wrap_mode(if on {
                gtk::WrapMode::WordChar
            } else {
                gtk::WrapMode::None
            });
            a.settings.borrow_mut().wrap = on;
            a.persist_settings();
        });
        win.add_action(&wrap);

        let numbers = gio::SimpleAction::new_stateful("line-numbers", None, &st.line_numbers.to_variant());
        let a = app.clone();
        numbers.connect_change_state(move |act, v| {
            let Some(v) = v else { return };
            let Some(on) = v.get::<bool>() else { return };
            act.set_state(v);
            a.editor.set_show_line_numbers(on);
            a.settings.borrow_mut().line_numbers = on;
            a.persist_settings();
        });
        win.add_action(&numbers);

        let panel = gio::SimpleAction::new_stateful("bottom-panel", None, &true.to_variant());
        let a = app.clone();
        panel.connect_change_state(move |act, v| {
            let Some(v) = v else { return };
            let Some(on) = v.get::<bool>() else { return };
            act.set_state(v);
            a.bottom.set_visible(on);
        });
        win.add_action(&panel);

        let theme = gio::SimpleAction::new_stateful(
            "theme",
            Some(glib::VariantTy::STRING),
            &st.theme.to_variant(),
        );
        let a = app.clone();
        theme.connect_change_state(move |act, v| {
            let Some(v) = v else { return };
            let Some(name) = v.get::<String>() else { return };
            act.set_state(v);
            a.apply_theme(&name);
            a.settings.borrow_mut().theme = name;
            a.persist_settings();
        });
        win.add_action(&theme);
    }

    // Find bar widgets
    {
        let a = app.clone();
        find_entry.connect_search_changed(move |e| {
            let t = e.text();
            a.search_settings
                .set_search_text(if t.is_empty() { None } else { Some(t.as_str()) });
            a.find_next(true, true);
        });
        let a = app.clone();
        find_entry.connect_activate(move |_| a.find_next(true, false));
        let a = app.clone();
        find_entry.connect_next_match(move |_| a.find_next(true, false));
        let a = app.clone();
        find_entry.connect_previous_match(move |_| a.find_next(false, false));
        let a = app.clone();
        find_next_btn.connect_clicked(move |_| a.find_next(true, false));
        let a = app.clone();
        find_prev_btn.connect_clicked(move |_| a.find_next(false, false));
        let a = app.clone();
        replace_btn.connect_clicked(move |_| a.replace_one());
        let a = app.clone();
        replace_entry.connect_activate(move |_| a.replace_one());
        let a = app.clone();
        replace_all_btn.connect_clicked(move |_| a.replace_all());
    }

    application.set_accels_for_action("win.quit", &["<Primary>q"]);
    application.set_accels_for_action("win.find", &["<Primary>f"]);
    application.set_accels_for_action("win.replace", &["<Primary>h"]);
    application.set_accels_for_action("win.find-next", &["F3", "<Primary>g"]);
    application.set_accels_for_action("win.find-prev", &["<Shift>F3", "<Primary><Shift>g"]);
    application.set_accels_for_action("win.goto-line", &["<Primary>l"]);
    application.set_accels_for_action("win.comment", &["<Primary>slash"]);
    application.set_accels_for_action("win.format", &["<Primary>t"]);
    application.set_accels_for_action("win.serial-monitor", &["<Primary><Shift>m"]);
    application.set_accels_for_action("win.zoom-in", &["<Primary>equal", "<Primary>plus"]);
    application.set_accels_for_action("win.zoom-out", &["<Primary>minus"]);
    application.set_accels_for_action("win.zoom-reset", &["<Primary>0"]);
    application.set_accels_for_action("win.shortcuts", &["F1"]);
    application.set_accels_for_action("win.new", &["<Primary>n"]);
    application.set_accels_for_action("win.open", &["<Primary>o"]);
    application.set_accels_for_action("win.save", &["<Primary>s"]);
    application.set_accels_for_action("win.save-as", &["<Primary><Shift>s"]);
    application.set_accels_for_action("win.verify", &["<Primary>r"]);
    application.set_accels_for_action("win.upload", &["<Primary>u"]);
    application.set_accels_for_action("win.refresh-ports", &["F5"]);
    application.set_accels_for_action("win.board-manager", &["<Primary><Shift>b"]);

    // Board / reload buttons
    {
        let app = app.clone();
        refresh_boards_btn.connect_clicked(move |_| app.refresh_boards());
    }
    {
        let app = app.clone();
        board_dd.connect_selected_notify(move |_| {
            if app.loading_boards.get() {
                return;
            }
            if let Some(f) = app.selected_fqbn() {
                save_board(&f);
            }
            app.board_changed();
        });
    }

    // Serial monitor controls
    {
        let app = app.clone();
        monitor_btn.connect_toggled(move |btn| {
            if app.suppress_toggle.get() {
                return;
            }
            if btn.is_active() {
                app.start_monitor();
            } else {
                app.stop_monitor();
            }
        });
    }
    {
        let app = app.clone();
        baud_dd.connect_selected_notify(move |_| {
            if app.monitor.borrow().is_some() {
                app.start_monitor();
            }
        });
    }
    {
        let app = app.clone();
        send_btn.connect_clicked(move |_| app.send_serial());
    }
    {
        let app = app.clone();
        serial_entry.connect_activate(move |_| app.send_serial());
    }

    // Close handling
    {
        let app = app.clone();
        win.connect_close_request(move |_| {
            if !app.modified.get() {
                app.stop_monitor();
                return glib::Propagation::Proceed;
            }
            let app = app.clone();
            glib::spawn_future_local(async move {
                if app.resolve_unsaved().await {
                    app.modified.set(false);
                    app.win.close();
                }
            });
            glib::Propagation::Stop
        });
    }

    win.present();

    if let Some(arg) = std::env::args().nth(1) {
        app.open_path(Path::new(&arg));
    }
    app.refresh_boards();
    app.refresh_ports();
}

fn main() -> glib::ExitCode {
    if let Err(e) = adw::init() {
        eprintln!("Failed to initialise GTK/libadwaita: {e}");
        return glib::ExitCode::FAILURE;
    }
    sourceview5::init();

    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
    application.run_with_args(&[std::env::args().next().unwrap_or_default()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("blink"), "blink");
        assert_eq!(sanitize("my sketch.v2"), "my_sketch_v2");
        assert_eq!(sanitize("-x"), "_-x");
        assert_eq!(sanitize(""), "sketch");
        assert_eq!(sanitize(&"a".repeat(100)).len(), 63);
    }

    #[test]
    fn parses_boards_v1_and_legacy() {
        let v1 = r#"{"boards":[{"name":"Arduino Uno","fqbn":"arduino:avr:uno"},{"name":"Hidden","fqbn":"x:y:z","hidden":true},{"name":"NoFqbn"}]}"#;
        let b = parse_boards(v1).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].fqbn, "arduino:avr:uno");
        let legacy = r#"[{"name":"B","fqbn":"a:b:c"},{"name":"A","fqbn":"a:b:d"}]"#;
        let b = parse_boards(legacy).unwrap();
        assert_eq!(b[0].name, "A");
        assert!(parse_boards("nonsense").is_err());
    }

    #[test]
    fn parses_options_and_composes_fqbn() {
        let j = r#"{"config_options":[
            {"option":"cpu","option_label":"CPU Speed","values":[
                {"value":"16","value_label":"16 MHz","selected":true},
                {"value":"8","value_label":"8 MHz"}]},
            {"option":"usb","option_label":"USB","values":[
                {"value":"cdc","value_label":"CDC","selected":true}]},
            {"option":"empty","option_label":"Nothing","values":[]}]}"#;
        let o = parse_options(j).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].label, "CPU Speed");
        assert!(o[0].values[0].default && !o[0].values[1].default);
        assert!(parse_options("{}").unwrap().is_empty());

        let mut picks = BTreeMap::new();
        assert_eq!(compose_fqbn("a:b:c", &o, &picks), "a:b:c");
        picks.insert("usb".to_owned(), "cdc".to_owned());
        picks.insert("cpu".to_owned(), "8".to_owned());
        assert_eq!(compose_fqbn("a:b:c", &o, &picks), "a:b:c:cpu=8,usb=cdc");
        // options not loaded yet: picks still apply
        assert_eq!(compose_fqbn("a:b:c", &[], &picks), "a:b:c:cpu=8,usb=cdc");
    }

    #[test]
    fn parses_platforms_and_urls() {
        let v1 = r#"{"platforms":[
            {"id":"x:y","latest_version":"1.0","releases":{"1.0":{"name":"Zed Boards"}}},
            {"id":"arduino:avr","maintainer":"Arduino","installed_version":"1.8.5","latest_version":"1.8.6","releases":{"1.8.6":{"name":"Arduino AVR Boards"}}},
            {"id":"old:dep","deprecated":true,"latest_version":"1"}]}"#;
        let p = parse_platforms(v1).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].id, "arduino:avr"); // installed first
        assert_eq!(p[0].name, "Arduino AVR Boards");
        assert_eq!(p[0].installed.as_deref(), Some("1.8.5"));
        assert_eq!(p[1].name, "Zed Boards");
        assert!(p[1].installed.is_none());
        let legacy = r#"[{"id":"a:b","installed":"1.0","latest":"1.1","name":"Alpha","maintainer":"M"}]"#;
        let p = parse_platforms(legacy).unwrap();
        assert_eq!((p[0].latest.as_str(), p[0].maintainer.as_str()), ("1.1", "M"));
        assert!(parse_platforms("{}").unwrap().is_empty());

        let cfg = r#"{"config":{"board_manager":{"additional_urls":["https://a/x.json","https://b/y.json"]}}}"#;
        assert_eq!(parse_urls(cfg).unwrap().len(), 2);
        assert!(parse_urls(r#"{"board_manager":{}}"#).unwrap().is_empty());
    }

    #[test]
    fn line_endings() {
        assert_eq!(line_ending(0), b"");
        assert_eq!(line_ending(1), b"\n");
        assert_eq!(line_ending(2), b"\r");
        assert_eq!(line_ending(3), b"\r\n");
    }

    #[test]
    fn toggles_comments() {
        let src: Vec<String> = ["  int a;", "", "\tint b;"].iter().map(|s| s.to_string()).collect();
        let commented = toggle_comment_lines(&src);
        assert_eq!(commented, vec!["  // int a;", "", "\t// int b;"]);
        assert_eq!(toggle_comment_lines(&commented), src);
        // a mixed block gets commented, not uncommented
        let mixed: Vec<String> = ["// x", "y"].iter().map(|s| s.to_string()).collect();
        assert_eq!(toggle_comment_lines(&mixed), vec!["// // x", "// y"]);
        // `//no-space` comments are handled too
        let tight: Vec<String> = vec!["//z".to_string()];
        assert_eq!(toggle_comment_lines(&tight), vec!["z"]);
        assert_eq!(toggle_comment_lines(&["".to_string()]), vec![""]);
    }

    #[test]
    fn settings_roundtrip_and_validation() {
        let s = Settings {
            theme: "dark".into(),
            font_pt: 14,
            wrap: true,
            line_numbers: false,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
        let bad = Settings::parse("theme=purple\nfont_pt=999\nwrap=maybe\nline_numbers=\ngarbage");
        assert_eq!(bad, Settings { wrap: false, ..Settings::default() });
        assert_eq!(Settings::parse(""), Settings::default());
    }

    #[test]
    fn recent_list_is_deduped_and_capped() {
        let mut l: Vec<String> = Vec::new();
        for n in 0..10 {
            push_recent(&mut l, &format!("/f{n}"), 8);
        }
        push_recent(&mut l, "/f5", 8);
        assert_eq!(l.len(), 8);
        assert_eq!(l[0], "/f5");
        assert_eq!(l.iter().filter(|p| *p == "/f5").count(), 1);
    }

    #[test]
    fn parses_ports() {
        let v1 = r#"{"detected_ports":[{"port":{"address":"/dev/ttyACM0","protocol":"serial"},"matching_boards":[{"name":"Arduino Uno","fqbn":"arduino:avr:uno"}]},{"port":{"address":"192.168.1.5","protocol":"network"}}]}"#;
        let p = parse_ports(v1).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].address, "/dev/ttyACM0");
        assert!(p[0].label.contains("Arduino Uno"));
        assert!(parse_ports("{}").unwrap().is_empty());
        assert!(parse_ports("[]").unwrap().is_empty());
    }

    #[test]
    fn prepares_cpp_sketch() {
        let dir = std::env::temp_dir().join("arduino-lite-test").join("proj");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("proj.cpp"), "void setup(){}\nvoid loop(){}\n").unwrap();
        fs::write(dir.join("util.h"), "// h\n").unwrap();
        let p = prepare_sketch(&dir.join("proj.cpp")).unwrap();
        assert!(p.sketch_dir.join("proj.cpp").exists());
        assert!(p.sketch_dir.join("proj.ino").exists());
        assert!(p.sketch_dir.join("util.h").exists());
        assert!(p.note.is_none());
    }
}
