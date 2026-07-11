// the xdg-desktop-portal backend, in-process: carrot claims
// org.freedesktop.impl.portal.desktop.carrot on the session bus and serves
// ScreenCast itself - no external backend, no fork. Start spins up a
// pipewire client-node fed from the present tail and replies with its
// global id once the daemon binds it.

pub mod cast;
mod picker;

use crate::dbus::{DbusConn, DbusError, MethodCall, MsgBuilder};
use crate::engine::{Engine, SpawnedFuture};
use crate::state::State;
use crate::uring::Ring;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.carrot";
const IF_SCREENCAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const IF_PROPS: &str = "org.freedesktop.DBus.Properties";
const IF_SESSION: &str = "org.freedesktop.impl.portal.Session";
const IF_REQUEST: &str = "org.freedesktop.impl.portal.Request";

const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
// version 4: restore_token/persist_mode understood
const VERSION: u32 = 4;
// portal response codes
const R_CANCELLED: u32 = 1;
const R_ENDED: u32 = 2;

struct Session {
    cursor_mode: Cell<u32>,
    types: Cell<u32>,
    /// requested persistence; we grant at most 1 (compositor lifetime)
    persist: Cell<u32>,
    /// what a presented restore_token resolved to
    restore: RefCell<Option<cast::RestoreData>>,
    /// the in-flight Start; dropping it cancels the cast setup
    starting: Cell<Option<SpawnedFuture<()>>>,
}

impl Default for Session {
    fn default() -> Session {
        Session {
            // the spec defaults: no pointer in the frames, monitors only
            cursor_mode: Cell::new(CURSOR_HIDDEN),
            types: Cell::new(SOURCE_MONITOR),
            persist: Cell::new(0),
            restore: RefCell::new(None),
            starting: Cell::new(None),
        }
    }
}

type Sessions = Rc<RefCell<HashMap<String, Session>>>;

struct Token {
    data: cast::RestoreData,
    /// survives restarts (persist mode 2); mirrored to the state file
    disk: bool,
}

type Tokens = Rc<RefCell<HashMap<String, Token>>>;

fn token_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("carrot").join("screencast-tokens.json"))
}

fn load_tokens() -> HashMap<String, Token> {
    let Some(path) = token_path() else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, cast::RestoreData>>(&text) {
        Ok(m) => m
            .into_iter()
            .map(|(k, data)| (k, Token { data, disk: true }))
            .collect(),
        Err(e) => {
            eprintln!("carrot: portal: {}: {e}", path.display());
            HashMap::new()
        }
    }
}

fn save_tokens(tokens: &HashMap<String, Token>) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = token_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let disk: HashMap<&String, &cast::RestoreData> = tokens
        .iter()
        .filter(|(_, t)| t.disk)
        .map(|(k, t)| (k, &t.data))
        .collect();
    let Ok(text) = serde_json::to_string(&disk) else { return };
    // tokens are standing consent: owner-only
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if let Err(e) = res {
        eprintln!("carrot: portal: {}: {e}", path.display());
    }
}

/// a click-to-select in flight; the seat answers it from the next click
pub struct PendingPick {
    pub types: u32,
    pub done: crate::util::AsyncEvent,
    pub result: RefCell<Option<cast::RestoreData>>,
}

/// what a consent click at the pointer lands on: a window when the session
/// allows windows, else the output under the cursor
pub fn cast_pick_at(state: &Rc<State>, types: u32, x: f64, y: f64) -> Option<cast::RestoreData> {
    if types & SOURCE_WINDOW != 0 {
        if let Some((s, _, _)) = crate::tree::surface_at(state, x as i32, y as i32) {
            if let Some(w) = crate::tree::window_for_surface(state, &s.get_root()) {
                return Some(cast::RestoreData::Window {
                    ident: w.ident,
                    app_id: w.app_id(),
                    title: w.title(),
                });
            }
        }
    }
    if types & SOURCE_MONITOR != 0 {
        let d = state.display.borrow();
        let outs = d.as_ref()?.outputs.borrow().clone();
        let out = outs
            .iter()
            .find(|o| o.rect().contains(x as i32, y as i32))
            .or_else(|| outs.get(state.focused_output.get()))?;
        return Some(cast::RestoreData::Output { name: out.conn.name.clone() });
    }
    None
}

/// zero-config consent: arm the seat and wait for a click (Escape or any
/// non-left button cancels). no seat means nobody can answer - cancel now
async fn seat_pick(state: &Rc<State>, types: u32) -> Option<cast::RestoreData> {
    if state.seat.borrow().is_none() {
        return None;
    }
    let pending = Rc::new(PendingPick {
        types,
        done: Default::default(),
        result: RefCell::new(None),
    });
    *state.cast_pick.borrow_mut() = Some(pending.clone());
    let watchdog = state.eng.spawn("pick watchdog", {
        let p = pending.clone();
        let ring = state.ring.clone();
        async move {
            let deadline = crate::util::Time::from_nsec(
                crate::util::Time::now().nsec() + 120 * 1_000_000_000,
            );
            if ring.timeout(deadline).await.is_ok() {
                p.done.trigger();
            }
        }
    });
    pending.done.triggered().await;
    drop(watchdog);
    *state.cast_pick.borrow_mut() = None;
    pending.result.borrow_mut().take()
}

fn reply_response(c: &DbusConn, call: &MethodCall, code: u32) {
    c.reply(call, "ua{sv}", |b| {
        b.put_u32(code);
        b.put_array(8, |_| {});
    });
}

fn response_to(c: &DbusConn, serial: u32, dest: &str, code: u32) {
    c.reply_to(serial, dest, "ua{sv}", |b| {
        b.put_u32(code);
        b.put_array(8, |_| {});
    });
}

fn prop_variant(b: &mut MsgBuilder, prop: &str) -> bool {
    match prop {
        "version" => b.put_variant("u", |b| b.put_u32(VERSION)),
        "AvailableSourceTypes" => {
            b.put_variant("u", |b| b.put_u32(SOURCE_MONITOR | SOURCE_WINDOW))
        }
        "AvailableCursorModes" => {
            b.put_variant("u", |b| b.put_u32(CURSOR_HIDDEN | CURSOR_EMBEDDED))
        }
        _ => return false,
    }
    true
}

fn serve_properties(conn: &Rc<DbusConn>) {
    conn.serve(IF_PROPS, Box::new(|c, call| match call.member.as_str() {
        "Get" => {
            let mut rd = call.rd();
            let iface = rd.str().unwrap_or_default();
            let prop = rd.str().unwrap_or_default();
            if iface != IF_SCREENCAST {
                c.reply_err(
                    call,
                    "org.freedesktop.DBus.Error.UnknownInterface",
                    "only screencast here",
                );
                return;
            }
            let mut ok = false;
            c.reply(call, "v", |b| ok = prop_variant(b, &prop));
            if !ok {
                // the reply already went out; unknown props answer as u 0,
                // which the frontend treats as absent
            }
        }
        "GetAll" => {
            c.reply(call, "a{sv}", |b| {
                b.put_array(8, |b| {
                    for p in ["version", "AvailableSourceTypes", "AvailableCursorModes"] {
                        b.align(8);
                        b.put_str(p);
                        prop_variant(b, p);
                    }
                });
            });
        }
        _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
    }));
}

/// the streams a(ua{sv}) results entry: node id plus geometry
fn put_streams_entry(b: &mut MsgBuilder, cast: &cast::Cast) {
    let source_type = match cast.restore_data() {
        // workspace casts present as monitor streams
        cast::RestoreData::Output { .. } | cast::RestoreData::Workspace { .. } => SOURCE_MONITOR,
        cast::RestoreData::Window { .. } => SOURCE_WINDOW,
    };
    b.align(8);
    b.put_str("streams");
    b.put_variant("a(ua{sv})", |b| {
        b.put_array(8, |b| {
            b.align(8);
            b.put_u32(cast.node_id);
            b.put_array(8, |b| {
                b.align(8);
                b.put_str("size");
                b.put_variant("(ii)", |b| {
                    b.align(8);
                    b.put_i32(cast.width as i32);
                    b.put_i32(cast.height as i32);
                });
                b.align(8);
                b.put_str("position");
                b.put_variant("(ii)", |b| {
                    b.align(8);
                    b.put_i32(cast.pos.0);
                    b.put_i32(cast.pos.1);
                });
                b.align(8);
                b.put_str("source_type");
                b.put_variant("u", |b| b.put_u32(source_type));
            });
        });
    });
}

fn serve_screencast(conn: &Rc<DbusConn>, sessions: Sessions, tokens: Tokens, state: Rc<State>) {
    use crate::dbus::wire::SvVal;
    let me = conn.clone();
    conn.serve(IF_SCREENCAST, Box::new(move |c, call| {
        match call.member.as_str() {
            "CreateSession" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                sessions.borrow_mut().insert(session, Session::default());
                c.reply(call, "ua{sv}", |b| {
                    b.put_u32(0);
                    b.put_array(8, |b| {
                        // the session id result key is required by the spec
                        b.align(8);
                        b.put_str("session_id");
                        b.put_variant("s", |b| b.put_str("carrot"));
                    });
                });
            }
            "SelectSources" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                let _app = rd.str();
                let opts = rd.sv_dict().unwrap_or_default();
                let map = sessions.borrow();
                let Some(s) = map.get(&session) else {
                    drop(map);
                    c.reply_err(
                        call,
                        "org.freedesktop.DBus.Error.Failed",
                        "no such session",
                    );
                    return;
                };
                for (k, v) in &opts {
                    match (k.as_str(), v) {
                        ("cursor_mode", SvVal::U(m)) => s.cursor_mode.set(*m),
                        ("types", SvVal::U(t)) => s.types.set(*t),
                        ("persist_mode", SvVal::U(p)) => s.persist.set(*p),
                        ("restore_token", SvVal::S(t)) => {
                            *s.restore.borrow_mut() =
                                tokens.borrow().get(t).map(|tok| tok.data.clone());
                        }
                        _ => {}
                    }
                }
                drop(map);
                reply_response(c, call, 0);
            }
            "Start" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                let (cursor, persist, types, restore) = {
                    let map = sessions.borrow();
                    let Some(s) = map.get(&session) else {
                        drop(map);
                        c.reply_err(
                            call,
                            "org.freedesktop.DBus.Error.Failed",
                            "no such session",
                        );
                        return;
                    };
                    (
                        s.cursor_mode.get() == CURSOR_EMBEDDED,
                        s.persist.get(),
                        s.types.get(),
                        s.restore.borrow().clone(),
                    )
                };
                let picker_cmd = state.config.borrow().picker.clone();
                // the node id comes from the daemon; the cast task replies
                let (serial, dest) = (call.serial, call.sender.clone());
                let me = me.clone();
                let st = state.clone();
                let toks = tokens.clone();
                let sess = session.clone();
                let task = state.eng.spawn("cast start", async move {
                    let pick = if let Some(r) = restore {
                        // a token is prior consent; it skips the picker
                        cast::Pick::Restored(r)
                    } else if let Some(cmd) = &picker_cmd {
                        match picker::pick(&st, cmd, types).await {
                            Some(picker::Choice::Output(name)) => {
                                cast::Pick::Restored(cast::RestoreData::Output { name })
                            }
                            Some(picker::Choice::Window(ident)) => cast::Pick::Ident(ident),
                            Some(picker::Choice::Workspace(index)) => {
                                cast::Pick::Restored(cast::RestoreData::Workspace { index })
                            }
                            None => {
                                response_to(&me, serial, &dest, R_CANCELLED);
                                return;
                            }
                        }
                    } else {
                        // no picker configured: the next click is the consent
                        match seat_pick(&st, types).await {
                            Some(r) => cast::Pick::Restored(r),
                            None => {
                                response_to(&me, serial, &dest, R_CANCELLED);
                                return;
                            }
                        }
                    };
                    match cast::start(&st, sess, cursor, pick).await {
                        Ok(cast) => {
                            let token = (persist > 0).then(|| {
                                let t = format!(
                                    "carrot:{:x}:{:x}",
                                    st.next_uid(),
                                    crate::util::Time::now().nsec()
                                );
                                let disk = persist >= 2;
                                toks.borrow_mut().insert(
                                    t.clone(),
                                    Token { data: cast.restore_data(), disk },
                                );
                                if disk {
                                    save_tokens(&toks.borrow());
                                }
                                t
                            });
                            me.reply_to(serial, &dest, "ua{sv}", |b| {
                                b.put_u32(0);
                                b.put_array(8, |b| {
                                    put_streams_entry(b, &cast);
                                    if let Some(t) = &token {
                                        b.align(8);
                                        b.put_str("restore_token");
                                        b.put_variant("s", |b| b.put_str(t));
                                        b.align(8);
                                        b.put_str("persist_mode");
                                        b.put_variant("u", |b| b.put_u32(persist.min(2)));
                                    }
                                });
                            });
                        }
                        Err(e) => {
                            eprintln!("carrot: portal: cast failed: {e}");
                            response_to(&me, serial, &dest, R_ENDED);
                        }
                    }
                });
                if let Some(s) = sessions.borrow().get(&session) {
                    s.starting.set(Some(task));
                }
            }
            "OpenPipeWireRemote" => {
                // a fresh daemon connection for the app; ours stays control-only
                match crate::pipewire::open_socket() {
                    Ok(fd) => c.reply_fds(call, "h", vec![Rc::new(fd)], |b| b.put_u32(0)),
                    Err(e) => c.reply_err(
                        call,
                        "org.freedesktop.DBus.Error.Failed",
                        &e.to_string(),
                    ),
                }
            }
            _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
        }
    }));
}

async fn run_inner(
    eng: &Rc<Engine>,
    ring: &Rc<Ring>,
    state: Rc<State>,
) -> Result<(), DbusError> {
    let conn = DbusConn::connect_session(eng, ring).await?;
    let sessions: Sessions = Rc::new(RefCell::new(HashMap::new()));
    let tokens: Tokens = Rc::new(RefCell::new(load_tokens()));
    serve_properties(&conn);
    serve_screencast(&conn, sessions.clone(), tokens, state.clone());
    conn.serve(IF_SESSION, Box::new({
        let sessions = sessions.clone();
        let state = state.clone();
        move |c, call| match call.member.as_str() {
            "Close" => {
                sessions.borrow_mut().remove(&call.path);
                state.casts.borrow_mut().retain(|x| x.session != call.path);
                c.reply(call, "", |_| {});
            }
            _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
        }
    }));
    conn.serve(IF_REQUEST, Box::new(|c, call| match call.member.as_str() {
        "Close" => c.reply(call, "", |_| {}),
        _ => c.reply_err(call, "org.freedesktop.DBus.Error.UnknownMethod", "no such method"),
    }));
    conn.request_name(PORTAL_NAME).await?;
    eprintln!("carrot: portal: serving {PORTAL_NAME}");
    std::future::pending::<()>().await;
    Ok(())
}

pub async fn run(eng: Rc<Engine>, ring: Rc<Ring>, state: Rc<State>) {
    if let Err(e) = run_inner(&eng, &ring, state).await {
        eprintln!("carrot: portal: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_round_trips() {
        let mut m = HashMap::new();
        m.insert(
            "carrot:1:2".to_string(),
            cast::RestoreData::Window { ident: 7, app_id: "foot".into(), title: "~".into() },
        );
        m.insert(
            "carrot:3:4".to_string(),
            cast::RestoreData::Output { name: "DP-1".into() },
        );
        m.insert(
            "carrot:5:6".to_string(),
            cast::RestoreData::Workspace { index: 2 },
        );
        let text = serde_json::to_string(&m).unwrap();
        let back: HashMap<String, cast::RestoreData> = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            &back["carrot:1:2"],
            cast::RestoreData::Window { ident: 7, .. }
        ));
        assert!(
            matches!(&back["carrot:3:4"], cast::RestoreData::Output { name } if name == "DP-1")
        );
        assert!(matches!(
            &back["carrot:5:6"],
            cast::RestoreData::Workspace { index: 2 }
        ));
    }
}

/// `carrot portal-probe [secs]`: serve the portal standalone so busctl and
/// the xdg-desktop-portal frontend can be tested without a compositor
pub fn probe() -> i32 {
    let secs: u64 = std::env::args()
        .skip_while(|a| a != "portal-probe")
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(30);
    let engine = Engine::new();
    let ring = match Ring::new(&engine, 32) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ring: {e}");
            return 1;
        }
    };
    let eng = engine.clone();
    let rng = ring.clone();
    let task = engine.spawn("portal probe", async move {
        let state = crate::state::State::new(&eng, &rng, match crate::engine::Wheel::new(&eng, &rng) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("wheel: {e}");
                rng.stop();
                return;
            }
        });
        let served = eng.spawn("portal", run(eng.clone(), rng.clone(), state));
        let deadline = crate::util::Time::from_nsec(
            crate::util::Time::now().nsec() + secs * 1_000_000_000,
        );
        let _ = rng.timeout(deadline).await;
        drop(served);
        rng.stop();
    });
    let _ = ring.run();
    drop(task);
    engine.clear();
    0
}
