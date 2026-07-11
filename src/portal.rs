// the xdg-desktop-portal backend, in-process: carrot claims
// org.freedesktop.impl.portal.desktop.carrot on the session bus and serves
// ScreenCast itself - no external backend, no fork. Start spins up a
// pipewire client-node fed from the present tail and replies with its
// global id once the daemon binds it.

pub mod cast;

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
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
const VERSION: u32 = 2;
// portal response codes
const R_ENDED: u32 = 2;

struct Session {
    cursor_mode: Cell<u32>,
    /// the in-flight Start; dropping it cancels the cast setup
    starting: Cell<Option<SpawnedFuture<()>>>,
}

impl Default for Session {
    fn default() -> Session {
        Session {
            // the spec default: no pointer in the frames
            cursor_mode: Cell::new(CURSOR_HIDDEN),
            starting: Cell::new(None),
        }
    }
}

type Sessions = Rc<RefCell<HashMap<String, Session>>>;

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
        "AvailableSourceTypes" => b.put_variant("u", |b| b.put_u32(SOURCE_MONITOR)),
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

/// the streams a(ua{sv}) result the app receives: node id plus geometry
fn put_streams(b: &mut MsgBuilder, cast: &cast::Cast) {
    b.put_u32(0);
    b.put_array(8, |b| {
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
                    b.put_variant("u", |b| b.put_u32(SOURCE_MONITOR));
                });
            });
        });
    });
}

fn serve_screencast(conn: &Rc<DbusConn>, sessions: Sessions, state: Rc<State>) {
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
                let opts = rd.u32_dict().unwrap_or_default();
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
                if let Some((_, m)) = opts.iter().find(|(k, _)| k == "cursor_mode") {
                    s.cursor_mode.set(*m);
                }
                drop(map);
                reply_response(c, call, 0);
            }
            "Start" => {
                let mut rd = call.rd();
                let _request = rd.str().unwrap_or_default();
                let session = rd.str().unwrap_or_default();
                let cursor = {
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
                    s.cursor_mode.get() == CURSOR_EMBEDDED
                };
                // the node id comes from the daemon; the cast task replies
                let (serial, dest) = (call.serial, call.sender.clone());
                let me = me.clone();
                let st = state.clone();
                let sess = session.clone();
                let task = state.eng.spawn("cast start", async move {
                    match cast::start(&st, sess, cursor).await {
                        Ok(cast) => me.reply_to(serial, &dest, "ua{sv}", |b| put_streams(b, &cast)),
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
    serve_properties(&conn);
    serve_screencast(&conn, sessions.clone(), state.clone());
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
