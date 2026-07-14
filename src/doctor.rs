// `carrot doctor`: one run, the whole gpu bring-up story - staged,
// flushed line by line to stderr and a numbered report file, so a
// tester's single launch carries everything a remote fix needs. no
// session, no drm master, no modesetting: the riskiest thing here is
// creating a vulkan device, the same as vulkaninfo.

use std::io::Write;

struct Report {
    file: Option<std::fs::File>,
    path: Option<std::path::PathBuf>,
    /// a second copy right in $HOME: one obvious file to attach
    home: Option<std::fs::File>,
    home_path: Option<std::path::PathBuf>,
}

impl Report {
    fn open() -> Report {
        let mut r = Report { file: None, path: None, home: None, home_path: None };
        if let Some(dir) = crate::crash_dir() {
            if std::fs::create_dir_all(&dir).is_ok() {
                let n = crate::next_report_number(&dir, "carrotDoctor");
                let path = dir.join(format!("carrotDoctor{n}.log"));
                if let Ok(f) = std::fs::File::create(&path) {
                    r.file = Some(f);
                    r.path = Some(path);
                }
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = std::path::PathBuf::from(home).join("carrotDoctor.log");
            if let Ok(f) = std::fs::File::create(&path) {
                r.home = Some(f);
                r.home_path = Some(path);
            }
        }
        r
    }

    fn say(&mut self, line: &str) {
        eprintln!("{line}");
        for f in [&mut self.file, &mut self.home].into_iter().flatten() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// every distinct mapped .so, flagging anything from a glibc: a leak
/// means a soname the stub family misses on this system
fn maps_sweep(r: &mut Report) {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        r.say("doctor:   (cannot read /proc/self/maps)");
        return;
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut leaks = 0;
    for line in maps.lines() {
        let Some(path) = line.split_whitespace().nth(5) else { continue };
        if !path.contains(".so") || seen.contains(&path) {
            continue;
        }
        seen.push(path);
        if path.contains("glibc") {
            leaks += 1;
            r.say(&format!("doctor:   GLIBC LEAK: {path}"));
        }
    }
    r.say(&format!(
        "doctor:   {} libraries mapped, {leaks} glibc leaks{}",
        seen.len(),
        if leaks == 0 { " (good)" } else { " - a stub soname is missing, list above" },
    ));
}

fn card_stages(r: &mut Report, path: &std::path::Path) {
    use std::os::fd::AsFd;
    r.say(&format!("doctor: == {}", path.display()));
    let card = match rustix::fs::open(path, rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC, rustix::fs::Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.say(&format!("doctor:   open failed: {e} (not in the video group?)"));
            return;
        }
    };
    match crate::render::loader::kernel_driver(card.as_fd()) {
        Ok(d) => r.say(&format!("doctor:   kernel driver: {d}")),
        Err(e) => r.say(&format!("doctor:   kernel driver unknown: {e}")),
    }
    r.say("doctor:   [1/3] driver closure (dlopen, gpu-free)");
    let entry = match crate::render::loader::entry_for(card.as_fd()) {
        Ok(e) => e,
        Err(e) => {
            r.say(&format!("doctor:   [1/3] FAILED: {e:?}"));
            return;
        }
    };
    drop(entry);
    r.say("doctor:   [1/3] ok");
    r.say("doctor:   [2/3] mapped closure sweep");
    maps_sweep(r);
    r.say("doctor:   [3/3] vulkan instance + device");
    match crate::render::vulkan::VkCore::new(card.as_fd()) {
        Ok(core) => {
            r.say(&format!(
                "doctor:   [3/3] ok: \"{}\" queue family {}",
                core.device_name, core.queue_family
            ));
            drop(core);
        }
        Err(e) => r.say(&format!("doctor:   [3/3] FAILED: {e:?}")),
    }
}

pub fn run() -> i32 {
    let mut r = Report::open();
    r.say(&format!(
        "carrot doctor {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    r.say("doctor: if a stage hangs: cat /proc/<pid>/maps, then kill it, send both");
    if let Ok(u) = std::fs::read_to_string("/proc/version") {
        r.say(&format!("doctor: kernel: {}", u.trim()));
    }

    r.say("doctor: cards:");
    let mut cards: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("card") && !name.contains('-') {
                let driver = std::fs::read_link(e.path().join("device/driver"))
                    .ok()
                    .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "?".into());
                r.say(&format!("doctor:   {name}: {driver}"));
                cards.push(std::path::PathBuf::from("/dev/dri").join(name));
            }
        }
    }
    cards.sort();

    r.say("doctor: vulkan icds discovered:");
    for icd in crate::render::loader::all_icd_libraries() {
        r.say(&format!("doctor:   {}", icd.display()));
    }

    r.say("doctor: taproot family:");
    for (name, env) in [("libc.so.6", "CARROT_LIBC"), ("libm.so.6", "CARROT_LIBM")] {
        match crate::render::loader::taproot_lib(name, env) {
            Ok(p) => r.say(&format!("doctor:   {name}: {}", p.display())),
            Err(_) => r.say(&format!("doctor:   {name}: MISSING (fatal for gpu init)")),
        }
    }
    for name in crate::render::loader::STUB_SONAMES {
        match crate::render::loader::taproot_lib(name, "CARROT_STUB_UNSET") {
            Ok(p) => r.say(&format!("doctor:   {name}: {}", p.display())),
            Err(_) => r.say(&format!("doctor:   {name}: missing (glibc may leak in)")),
        }
    }

    for card in &cards {
        card_stages(&mut r, card);
    }
    if cards.is_empty() {
        r.say("doctor: no /dev/dri cards found");
    }

    match (r.home_path.clone(), r.path.clone()) {
        (Some(h), _) => {
            let line = format!("doctor: report written to {} - send that file", h.display());
            r.say(&line);
        }
        (None, Some(p)) => {
            let line = format!("doctor: report written to {}", p.display());
            r.say(&line);
        }
        (None, None) => r.say("doctor: report file could not be written; copy this output"),
    }
    0
}
