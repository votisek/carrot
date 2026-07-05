// the logind session - TakeControl once, TakeDevice per device, pause/resume
// signals around vt switches.
// pause handshake: logind waits for PauseDeviceComplete only on ty=="pause";
// "force" already revoked access, "gone" means the hardware left. acking the
// wrong ones is a protocol error.

use super::{Arg, DbusConn, DbusError, SignalMsg};
use crate::engine::Engine;
use crate::uring::Ring;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::rc::Rc;

const DEST: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const IF_MANAGER: &str = "org.freedesktop.login1.Manager";
const IF_SESSION: &str = "org.freedesktop.login1.Session";
const IF_SEAT: &str = "org.freedesktop.login1.Seat";
const IF_PROPS: &str = "org.freedesktop.DBus.Properties";

pub enum DeviceEvent {
    /// access suspended (vt switch)
    Pause { dev: u64 },
    /// hardware gone, close and forget
    Gone { dev: u64 },
    /// access back; input swaps to the new fd, drm keeps its own
    Resume { dev: u64, fd: Rc<OwnedFd> },
}

pub struct LogindSession {
    conn: Rc<DbusConn>,
    pub session_path: String,
    pub seat_path: String,
    pub vtnr: u32,
    /// per-device handler, keyed by dev_t. runs synchronously in signal
    /// delivery; for ty=="pause" that is BEFORE the PauseDeviceComplete ack,
    /// so display teardown still happens while we hold drm master.
    handlers: Rc<RefCell<HashMap<u64, Rc<dyn Fn(DeviceEvent)>>>>,
}

impl LogindSession {
    pub async fn take_control(
        eng: &Rc<Engine>,
        ring: &Rc<Ring>,
    ) -> Result<Rc<LogindSession>, DbusError> {
        let conn = DbusConn::connect(eng, ring).await?;

        let reply = match std::env::var("XDG_SESSION_ID") {
            Ok(id) => {
                conn.call(DEST, MANAGER_PATH, IF_MANAGER, "GetSession", "s", &[Arg::Str(&id)])
                    .await?
            }
            // pid 0 = this process's own session
            Err(_) => {
                conn.call(
                    DEST,
                    MANAGER_PATH,
                    IF_MANAGER,
                    "GetSessionByPID",
                    "u",
                    &[Arg::U32(0)],
                )
                .await?
            }
        };
        if reply.sig != "o" {
            return Err(DbusError::BadReply("GetSession"));
        }
        let session_path = reply.rd().str()?;

        let reply = conn
            .call(
                DEST,
                &session_path,
                IF_PROPS,
                "Get",
                "ss",
                &[Arg::Str(IF_SESSION), Arg::Str("Seat")],
            )
            .await?;
        if reply.sig != "v" {
            return Err(DbusError::BadReply("Seat property"));
        }
        let mut rd = reply.rd();
        if rd.sig()? != "(so)" {
            return Err(DbusError::BadReply("Seat property shape"));
        }
        rd.align(8)?;
        let _seat_name = rd.str()?;
        let seat_path = rd.str()?;

        // our own vt, so switch-to-self can be a no-op
        let reply = conn
            .call(
                DEST,
                &session_path,
                IF_PROPS,
                "Get",
                "ss",
                &[Arg::Str(IF_SESSION), Arg::Str("VTNr")],
            )
            .await?;
        let mut rd = reply.rd();
        if rd.sig()? != "u" {
            return Err(DbusError::BadReply("VTNr property shape"));
        }
        let vtnr = rd.u32()?;

        conn.call(
            DEST,
            &session_path,
            IF_SESSION,
            "TakeControl",
            "b",
            &[Arg::Bool(false)],
        )
        .await?;
        conn.call_noreply(
            DEST,
            &session_path,
            IF_SESSION,
            "SetType",
            "s",
            &[Arg::Str("wayland")],
        );

        let handlers: Rc<RefCell<HashMap<u64, Rc<dyn Fn(DeviceEvent)>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let dispatch = |handlers: &RefCell<HashMap<u64, Rc<dyn Fn(DeviceEvent)>>>,
                        dev: u64,
                        ev: DeviceEvent| {
            // clone out so the borrow ends before the call - a handler
            // may register or drop others
            let cb = handlers.borrow().get(&dev).cloned();
            if let Some(cb) = cb {
                cb(ev);
            }
        };

        let h = handlers.clone();
        let ack = conn.clone();
        let ack_path = session_path.clone();
        conn.subscribe(
            IF_SESSION,
            "PauseDevice",
            DEST,
            &session_path,
            Box::new(move |s: &SignalMsg| {
                let mut rd = s.rd();
                let Ok(major) = rd.u32() else { return };
                let Ok(minor) = rd.u32() else { return };
                let Ok(ty) = rd.str() else { return };
                let dev = rustix::fs::makedev(major, minor);
                match ty.as_str() {
                    "pause" => {
                        // teardown first, while master is still ours; the ack
                        // lets logind finish the switch
                        dispatch(&h, dev, DeviceEvent::Pause { dev });
                        ack.call_noreply(
                            DEST,
                            &ack_path,
                            IF_SESSION,
                            "PauseDeviceComplete",
                            "uu",
                            &[Arg::U32(major), Arg::U32(minor)],
                        );
                    }
                    "gone" => dispatch(&h, dev, DeviceEvent::Gone { dev }),
                    _ => dispatch(&h, dev, DeviceEvent::Pause { dev }),
                }
            }),
        )
        .await?;

        let h = handlers.clone();
        conn.subscribe(
            IF_SESSION,
            "ResumeDevice",
            DEST,
            &session_path,
            Box::new(move |s: &SignalMsg| {
                let mut rd = s.rd();
                let Ok(major) = rd.u32() else { return };
                let Ok(minor) = rd.u32() else { return };
                let Ok(fd) = rd.fd() else { return };
                let dev = rustix::fs::makedev(major, minor);
                dispatch(&h, dev, DeviceEvent::Resume { dev, fd });
            }),
        )
        .await?;

        Ok(Rc::new(LogindSession {
            conn,
            session_path,
            seat_path,
            vtnr,
            handlers,
        }))
    }

    /// route this device's pause/resume/gone to `cb`
    pub fn on_device(&self, dev: u64, cb: Rc<dyn Fn(DeviceEvent)>) {
        self.handlers.borrow_mut().insert(dev, cb);
    }

    pub fn forget_device(&self, dev: u64) {
        self.handlers.borrow_mut().remove(&dev);
    }

    /// (fd, inactive). an inactive input fd must not be read until its
    /// ResumeDevice; drm ignores the flag.
    pub async fn take_device(&self, dev: u64) -> Result<(Rc<OwnedFd>, bool), DbusError> {
        let (major, minor) = (rustix::fs::major(dev), rustix::fs::minor(dev));
        let reply = self
            .conn
            .call(
                DEST,
                &self.session_path,
                IF_SESSION,
                "TakeDevice",
                "uu",
                &[Arg::U32(major), Arg::U32(minor)],
            )
            .await?;
        if reply.sig != "hb" {
            return Err(DbusError::BadReply("TakeDevice"));
        }
        let mut rd = reply.rd();
        let fd = rd.fd()?;
        let inactive = rd.bool()?;
        Ok((fd, inactive))
    }

    pub fn release_device(&self, dev: u64) {
        let (major, minor) = (rustix::fs::major(dev), rustix::fs::minor(dev));
        self.conn.call_noreply(
            DEST,
            &self.session_path,
            IF_SESSION,
            "ReleaseDevice",
            "uu",
            &[Arg::U32(major), Arg::U32(minor)],
        );
    }

    pub fn switch_vt(&self, vt: u32) {
        self.conn
            .call_noreply(DEST, &self.seat_path, IF_SEAT, "SwitchTo", "u", &[Arg::U32(vt)]);
    }

    pub fn clear(&self) {
        self.handlers.borrow_mut().clear();
        self.conn.clear();
    }
}

/// dev diagnostic (`carrot dbus-probe`): full logind bring-up. inside a live
/// session TakeControl fails busy but still proves the wire path; from a tty
/// it runs through TakeDevice on card0.
pub(super) fn probe() -> i32 {
    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let status = Rc::new(std::cell::Cell::new(1));
    let st = status.clone();
    let eng = engine.clone();
    let rng = ring.clone();
    let task = engine.spawn("logind probe", async move {
        match probe_run(&eng, &rng).await {
            Ok(()) => st.set(0),
            Err(e) => eprintln!("FAIL: {e}"),
        }
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    status.get() as i32
}

/// same sequence as take_control, one printed step at a time so a hang
/// names its own culprit
async fn probe_run(eng: &Rc<Engine>, ring: &Rc<Ring>) -> Result<(), DbusError> {
    println!("-> connect");
    let conn = DbusConn::connect(eng, ring).await?;
    if let Ok(f) = std::fs::File::create("/tmp/carrot-dbus-probe.log") {
        conn.set_dump(f);
        println!("   (wire transcript: /tmp/carrot-dbus-probe.log)");
    }
    println!("bus ok, unique name {}", conn.unique_name.borrow());

    println!("-> GetSession");
    let reply = match std::env::var("XDG_SESSION_ID") {
        Ok(id) => {
            println!("   (XDG_SESSION_ID = {id})");
            conn.call(DEST, MANAGER_PATH, IF_MANAGER, "GetSession", "s", &[Arg::Str(&id)])
                .await?
        }
        Err(_) => {
            println!("   (no XDG_SESSION_ID, using GetSessionByPID)");
            conn.call(
                DEST,
                MANAGER_PATH,
                IF_MANAGER,
                "GetSessionByPID",
                "u",
                &[Arg::U32(0)],
            )
            .await?
        }
    };
    let session_path = reply.rd().str()?;
    println!("session {session_path}");

    println!("-> Get Seat property");
    let reply = conn
        .call(
            DEST,
            &session_path,
            IF_PROPS,
            "Get",
            "ss",
            &[Arg::Str(IF_SESSION), Arg::Str("Seat")],
        )
        .await?;
    let mut rd = reply.rd();
    let vsig = rd.sig()?;
    if vsig != "(so)" {
        return Err(DbusError::BadReply("Seat property shape"));
    }
    rd.align(8)?;
    let _ = rd.str()?;
    let seat_path = rd.str()?;
    println!("seat    {seat_path}");

    println!("-> TakeControl");
    println!("   (on success logind flips the vt to graphics mode - the");
    println!("    console goes dark; results land in the transcript)");
    match conn
        .call(
            DEST,
            &session_path,
            IF_SESSION,
            "TakeControl",
            "b",
            &[Arg::Bool(false)],
        )
        .await
    {
        Ok(_) => println!("TakeControl ok"),
        Err(e @ DbusError::Call { .. }) => {
            println!("TakeControl: {e}");
            println!("(expected inside a live session - the running compositor holds control)");
            conn.clear();
            return Ok(());
        }
        Err(e) => return Err(e),
    }

    println!("-> SetType (no reply expected)");
    conn.call_noreply(
        DEST,
        &session_path,
        IF_SESSION,
        "SetType",
        "s",
        &[Arg::Str("wayland")],
    );

    println!("-> AddMatch PauseDevice");
    conn.subscribe(
        IF_SESSION,
        "PauseDevice",
        DEST,
        &session_path,
        Box::new(|_| {}),
    )
    .await?;
    println!("-> AddMatch ResumeDevice");
    conn.subscribe(
        IF_SESSION,
        "ResumeDevice",
        DEST,
        &session_path,
        Box::new(|_| {}),
    )
    .await?;
    println!("subscriptions ok");

    if let Ok(st) = rustix::fs::stat("/dev/dri/card0") {
        println!("-> TakeDevice card0");
        let reply = conn
            .call(
                DEST,
                &session_path,
                IF_SESSION,
                "TakeDevice",
                "uu",
                &[
                    Arg::U32(rustix::fs::major(st.st_rdev)),
                    Arg::U32(rustix::fs::minor(st.st_rdev)),
                ],
            )
            .await?;
        let mut rd = reply.rd();
        let fd = rd.fd()?;
        let inactive = rd.bool()?;
        {
            use std::os::fd::AsRawFd;
            let line = format!("TakeDevice card0: fd {} inactive {}", fd.as_raw_fd(), inactive);
            println!("{line}");
            conn.dump_line(&line);
        }
        println!("-> ReleaseDevice card0");
        conn.call_noreply(
            DEST,
            &session_path,
            IF_SESSION,
            "ReleaseDevice",
            "uu",
            &[
                Arg::U32(rustix::fs::major(st.st_rdev)),
                Arg::U32(rustix::fs::minor(st.st_rdev)),
            ],
        );
    }
    println!("PASS");
    conn.dump_line("PASS");
    conn.clear();
    Ok(())
}
