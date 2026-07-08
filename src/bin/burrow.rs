// burrow - talk to the compositor over its json socket.
//
//   burrow workspace 3          burrow windows
//   burrow spawn "foot"         burrow workspaces
//   burrow toggle-fullscreen    burrow reload
//   burrow close-window         burrow subscribe
//
// one request per line in, one json reply per line out. replies render as
// expanded key: value blocks; --json keeps the wire form, and subscribe
// always streams raw ndjson for scripts.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn socket_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let dir = std::path::PathBuf::from(dir);
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        let p = dir.join(format!("carrot.{display}.sock"));
        if p.exists() {
            return Some(p);
        }
    }
    // no display in the env: take whatever carrot socket is around
    let mut found = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with("carrot.") && name.ends_with(".sock") {
            found = Some(e.path());
        }
    }
    found
}

fn usage() -> ! {
    std::process::exit(2)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let n_arg = |i: usize| -> u64 {
        let n: u64 = args.get(i).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
        // 1-based on the cli, 0-based inside
        n.saturating_sub(1)
    };
    let request = match args[0].as_str() {
        // a signed argument means a relative jump
        "workspace" if args.get(1).is_some_and(|a| a.starts_with(['+', '-'])) => {
            let d: i64 = args[1].parse().unwrap_or_else(|_| usage());
            format!("{{\"workspace-rel\":{d}}}")
        }
        "workspace" => format!("{{\"workspace\":{}}}", n_arg(1)),
        "workspace-rel" => {
            let d: i64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
            format!("{{\"workspace-rel\":{d}}}")
        }
        "send-to-workspace" => format!("{{\"send-to-workspace\":{}}}", n_arg(1)),
        "move-to-workspace" => format!("{{\"move-to-workspace\":{}}}", n_arg(1)),
        "spawn" => {
            if args.len() < 2 {
                usage();
            }
            serde_json::json!({ "spawn": args[1..].join(" ") }).to_string()
        }
        "split-ratio" => {
            let d: f64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or_else(|| usage());
            serde_json::json!({ "split-ratio": d }).to_string()
        }
        cmd @ ("focus-left" | "focus-right" | "focus-up" | "focus-down") => {
            format!("{{\"focus-dir\":\"{}\"}}", &cmd["focus-".len()..])
        }
        cmd @ ("swap-left" | "swap-right" | "swap-up" | "swap-down") => {
            format!("{{\"swap-dir\":\"{}\"}}", &cmd["swap-".len()..])
        }
        cmd @ ("toggle-fullscreen" | "toggle-floating" | "close-window" | "focus-next"
        | "focus-prev" | "quit" | "monitors" | "workspaces" | "windows" | "clients" | "reload"
        | "subscribe" | "dump-shadow" | "dpms-on" | "dpms-off") => {
            serde_json::json!(cmd).to_string()
        }
        _ => usage(),
    };
    let Some(path) = socket_path() else {
        eprintln!("burrow: no carrot socket found (is carrot running?)");
        std::process::exit(1);
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("burrow: {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let reader = BufReader::new(stream);
    let streaming = args[0] == "subscribe";
    for line in reader.lines() {
        let Ok(l) = line else { break };
            println!("{l}");
        if !streaming {
            break;
        }
    }
}
