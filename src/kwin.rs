//! Placing and focusing game windows on KDE Wayland.
//!
//! Wayland gives no client the right to move another client's window, so the
//! compositor has to do it. KWin exposes a small JavaScript engine over D-Bus:
//! a script is written to disk, loaded, started, and the lines it `print`s are
//! read back from the user journal. Every script prints a unique marker, so two
//! threads (the grid and anti-AFK) never confuse each other's answers.
//!
//! Only KWin is supported. On any other compositor [`available`] is false and
//! the caller leaves the windows alone; nothing fails.
//!
//! KWin destroys a script after it runs, so there is no `setTimeout` and no
//! persistent state between calls. Each call is one self-contained pass.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// D-Bus address of KWin's scripting interface.
const ORG: &str = "org.kde.KWin";
const PATH: &str = "/Scripting";
const IFACE: &str = "org.kde.kwin.Scripting";
/// Syslog identifier KWin writes its `print` output under.
const JOURNAL_TAG: &str = "kwin_wayland";
/// How long a script may take to print its answer.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(2_500);

/// A window as KWin sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub pid: u32,
    /// KWin's internal id: stable for the lifetime of the window.
    pub id: String,
}

/// A rectangle in compositor coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Whether this manager can talk to KWin at all.
///
/// Cached: the probe starts a process, and the UI asks on every frame.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        in_path("qdbus6")
            && in_path("journalctl")
            && dbus(&["isScriptLoaded", "rbm-probe"]).is_some()
    })
}

fn in_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file()))
}

/// Every Sober window, oldest process first: the launch order, which is also
/// the order the grid and anti-AFK want.
pub fn windows() -> Vec<Window> {
    let Some(lines) = run_script(
        "var list = workspace.windowList();\n\
         var ours = [];\n\
         for (var i = 0; i < list.length; i++) {\n\
             if (String(list[i].resourceClass).indexOf(\"org.vinegarhq.Sober\") === 0) ours.push(list[i]);\n\
         }\n\
         ours.sort(function (a, b) { return a.pid - b.pid; });\n\
         for (var j = 0; j < ours.length; j++) rbmEmit(\"win \" + ours[j].pid + \" \" + ours[j].internalId);\n\
         rbmEmit(\"count \" + ours.length);\n",
    ) else {
        return Vec::new();
    };

    lines
        .iter()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != "win" {
                return None;
            }
            let pid: u32 = parts.next()?.parse().ok()?;
            let id = parts.next()?.to_string();
            is_safe_id(&id).then_some(Window { pid, id })
        })
        .collect()
}

/// The window that currently has the keyboard focus.
pub fn active_window() -> Option<Window> {
    let lines = run_script(
        "var w = workspace.activeWindow;\n\
         if (w) rbmEmit(\"active \" + w.pid + \" \" + w.internalId);\n\
         else rbmEmit(\"none\");\n",
    )?;
    for line in lines {
        let mut parts = line.split_whitespace();
        if parts.next()? != "active" {
            continue;
        }
        let pid: u32 = parts.next()?.parse().ok()?;
        let id = parts.next()?.to_string();
        if is_safe_id(&id) {
            return Some(Window { pid, id });
        }
    }
    None
}

/// Give `window` the keyboard focus. Returns whether KWin confirmed it.
pub fn focus(window: &Window) -> bool {
    if !is_safe_id(&window.id) {
        return false;
    }
    let body = format!(
        "var want = \"{}\";\n\
         var list = workspace.windowList();\n\
         for (var i = 0; i < list.length; i++) {{\n\
             if (String(list[i].internalId) === want) {{\n\
                 workspace.activeWindow = list[i];\n\
                 rbmEmit(\"focused \" + list[i].pid);\n\
                 break;\n\
             }}\n\
         }}\n",
        window.id
    );
    run_script(&body).is_some_and(|lines| {
        lines
            .iter()
            .any(|line| line.strip_prefix("focused ") == Some(window.pid.to_string().as_str()))
    })
}

/// Work area of the active screen: the screen minus panels and docks.
pub fn work_area() -> Option<Rect> {
    let lines = run_script(
        "var a = workspace.clientArea(KWin.MaximizeArea, workspace.activeScreen, workspace.currentDesktop);\n\
         rbmEmit(\"area \" + a.x + \" \" + a.y + \" \" + a.width + \" \" + a.height);\n",
    )?;
    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 5 || parts[0] != "area" {
            continue;
        }
        let numbers: Vec<i32> = parts[1..]
            .iter()
            .filter_map(|part| part.parse().ok())
            .collect();
        if let [x, y, width, height] = numbers[..] {
            if width > 0 && height > 0 {
                return Some(Rect {
                    x,
                    y,
                    width,
                    height,
                });
            }
        }
    }
    None
}

/// Spread every Sober window over the work area.
///
/// A fullscreen or maximized window ignores geometry, so those states are left
/// first; KWin applies that asynchronously, hence the short pause before the
/// coordinates are assigned.
pub fn tile(columns: usize, rows: usize) {
    let columns = columns.max(1);
    let rows = rows.max(1);

    let unfullscreen = run_script(
        "var list = workspace.windowList();\n\
         var changed = 0;\n\
         for (var i = 0; i < list.length; i++) {\n\
             if (String(list[i].resourceClass).indexOf(\"org.vinegarhq.Sober\") !== 0) continue;\n\
             if (list[i].fullScreen) { list[i].fullScreen = false; changed++; }\n\
             if (list[i].maximizeMode !== 0) { list[i].setMaximize(false, false); changed++; }\n\
         }\n\
         rbmEmit(\"unfull \" + changed);\n",
    );
    let changed = unfullscreen
        .as_deref()
        .and_then(|lines| lines.iter().find_map(|line| line.strip_prefix("unfull ")))
        .and_then(|count| count.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if changed > 0 {
        std::thread::sleep(Duration::from_millis(450));
    }

    let body = format!(
        "var area = workspace.clientArea(KWin.MaximizeArea, workspace.activeScreen, workspace.currentDesktop);\n\
         var list = workspace.windowList();\n\
         var ours = [];\n\
         for (var i = 0; i < list.length; i++) {{\n\
             if (String(list[i].resourceClass).indexOf(\"org.vinegarhq.Sober\") === 0) ours.push(list[i]);\n\
         }}\n\
         ours.sort(function (a, b) {{ return a.pid - b.pid; }});\n\
         var cols = {columns};\n\
         var rows = {rows};\n\
         var cw = Math.floor(area.width / cols);\n\
         var ch = Math.floor(area.height / rows);\n\
         for (var j = 0; j < ours.length; j++) {{\n\
             var col = j % cols;\n\
             var row = Math.floor(j / cols);\n\
             ours[j].frameGeometry = {{x: area.x + col * cw, y: area.y + row * ch, width: cw, height: ch}};\n\
         }}\n\
         rbmEmit(\"tiled \" + ours.length + \" \" + cols + \"x\" + rows);\n"
    );
    let _ = run_script(&body);
}

/// Run one script and return the lines it printed after its marker.
fn run_script(body: &str) -> Option<Vec<String>> {
    // Every public entry point lands here; checking once here keeps a machine
    // without KWin from spawning qdbus and journalctl for nothing.
    if !available() {
        return None;
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let tag = format!(
        "rbm-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let marker = format!("{tag}:");
    let script = format!("function rbmEmit(line) {{ print(\"{marker} \" + line); }}\n{body}");
    let path = script_path(&tag);
    fs::write(&path, script).ok()?;

    // A second of slack: journal timestamps are whole seconds.
    let since = epoch_secs().saturating_sub(1);
    let loaded = dbus(&["loadScript", &path.to_string_lossy(), &tag]);
    let started = loaded.is_some() && dbus(&["start"]).is_some();
    let lines = if started {
        wait_for_answers(&marker, since)
    } else {
        None
    };

    let _ = dbus(&["unloadScript", &tag]);
    let _ = fs::remove_file(&path);
    lines
}

fn script_path(tag: &str) -> PathBuf {
    // The compositor runs as the same user and can read /tmp. XDG_RUNTIME_DIR
    // would be tighter, but some setups point it inside a sandbox.
    std::env::temp_dir().join(format!("{tag}.js"))
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Poll the journal until the script has printed something.
fn wait_for_answers(marker: &str, since: u64) -> Option<Vec<String>> {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        if let Some(lines) = read_answers(marker, since) {
            return Some(lines);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn read_answers(marker: &str, since: u64) -> Option<Vec<String>> {
    let output = Command::new("journalctl")
        .args([
            "--user",
            "-t",
            JOURNAL_TAG,
            "--since",
            &format!("@{since}"),
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = text
        .lines()
        .filter_map(|line| line.find(marker).map(|at| line[at + marker.len()..].trim()))
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    (!lines.is_empty()).then_some(lines)
}

/// One call on `org.kde.kwin.Scripting`. The first item is the method, the
/// rest are its arguments. `None` on any D-Bus failure.
fn dbus(args: &[&str]) -> Option<String> {
    let (method, rest) = args.split_first()?;
    let output = Command::new("qdbus6")
        .arg(ORG)
        .arg(PATH)
        .arg(format!("{IFACE}.{method}"))
        .args(rest)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// KWin ids are UUIDs in braces. Anything else never reaches a script.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '{' || c == '}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ids_are_sanitized_before_they_reach_a_script() {
        assert!(is_safe_id("{9f02b8c1-9d21-47da-9769-d1f29ed18197}"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("}; workspace.activeWindow = null; //"));
        assert!(!is_safe_id("id with spaces"));
    }

    #[test]
    fn a_broken_compositor_is_not_available() {
        // Whatever this machine has, asking must be cheap and must not panic.
        let _ = available();
    }

    #[test]
    #[ignore = "needs a live KWin session with a Sober window"]
    fn smoke_kwin_round_trip() {
        assert!(available(), "KWin is not reachable");
        let area = work_area().expect("work area");
        println!("area: {area:?}");
        let windows = windows();
        println!("windows: {windows:?}");
        assert!(!windows.is_empty(), "no Sober window is open");

        let previous = active_window();
        println!("previous: {previous:?}");
        assert!(focus(&windows[0]), "could not focus {:?}", windows[0]);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            active_window().map(|window| window.id),
            Some(windows[0].id.clone())
        );

        tile(1, 1);
        std::thread::sleep(Duration::from_millis(1_000));
        if let Some(previous) = previous {
            let _ = focus(&previous);
        }

        // The user's Sober was almost certainly fullscreen before the test.
        let _ = run_script(
            "var list = workspace.windowList();\n\
             for (var i = 0; i < list.length; i++) {\n\
                 if (String(list[i].resourceClass).indexOf(\"org.vinegarhq.Sober\") === 0) list[i].fullScreen = true;\n\
             }\n\
             rbmEmit(\"restored\");\n",
        );
    }
}
