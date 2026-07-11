// a portal screencast: one pipewire client-node per session, fed from the
// present tail. frames re-compose via screencopy, so a cast keeps working
// even where the live path scans out on a hardware plane.

use crate::engine::SpawnedFuture;
use crate::pipewire::client_node::SourceNode;
use crate::pipewire::{PwConn, PwError};
use crate::state::State;
use crate::util::Time;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const PROXY_ID: u32 = 2;
const COOKIE: i32 = 0x0ca5;

pub struct Cast {
    /// the portal session handle path; Session.Close tears us down by it
    pub session: String,
    /// the daemon-side global; Start hands this to the app
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub pos: (i32, i32),
    output_name: String,
    /// paint the pointer into the frames (portal cursor mode EMBEDDED)
    cursor: bool,
    node: Rc<RefCell<SourceNode>>,
    /// presents can outpace the negotiated rate; feed() gates on this
    frame_ns: u64,
    last: Cell<u64>,
    /// the pump lost the daemon; the present tail sweeps us out
    dead: Rc<Cell<bool>>,
    _pump: SpawnedFuture<()>,
}

/// connect, create the node, wait for the daemon to bind it, register with
/// the state. the returned cast is already live at the present tail.
pub async fn start(state: &Rc<State>, session: String, cursor: bool) -> Result<Rc<Cast>, PwError> {
    let (output_name, width, height, fps, pos) =
        pick_output(state).ok_or(PwError::Env("no output to cast"))?;
    let con = Rc::new(PwConn::connect(&state.ring)?);
    con.hello().await?;
    let node = Rc::new(RefCell::new(
        SourceNode::create(con.clone(), PROXY_ID, width, height, fps).await?,
    ));
    con.sync(COOKIE).await?;
    crate::pipewire::pump_until_done(&con, &node, COOKIE).await?;
    let node_id = node
        .borrow()
        .bound_global
        .ok_or(PwError::Env("the daemon never bound the node"))?;
    let dead = Rc::new(Cell::new(false));
    let pump = state.eng.spawn("cast pump", {
        let con = con.clone();
        let node = node.clone();
        let dead = dead.clone();
        async move {
            if let Err(e) = crate::pipewire::pump_node(&con, &node).await {
                eprintln!("carrot: cast: {e}");
            }
            dead.set(true);
        }
    });
    let cast = Rc::new(Cast {
        session,
        node_id,
        width,
        height,
        pos,
        output_name,
        cursor,
        node,
        frame_ns: 1_000_000_000 / fps as u64,
        last: Cell::new(0),
        dead,
        _pump: pump,
    });
    state.casts.borrow_mut().push(cast.clone());
    Ok(cast)
}

fn pick_output(state: &Rc<State>) -> Option<(String, u32, u32, u32, (i32, i32))> {
    let d = state.display.borrow();
    let outs = d.as_ref()?.outputs.borrow();
    let out = outs.get(state.focused_output.get()).or_else(|| outs.first())?;
    let fps = out
        .conn
        .pipe
        .borrow()
        .as_ref()
        .map(|p| p.mode.vrefresh)
        .unwrap_or(60)
        .max(1);
    Some((out.conn.name.clone(), out.width, out.height, fps, out.pos.get()))
}

/// present tail: the frame just shown is what casts of this output stream
pub fn output_presented(state: &Rc<State>, name: &str) {
    if state.casts.borrow().is_empty() {
        return;
    }
    let casts: Vec<Rc<Cast>> = state.casts.borrow().clone();
    let mut sweep = false;
    for c in &casts {
        if c.dead.get() {
            sweep = true;
            continue;
        }
        if c.output_name == name {
            c.feed(state);
        }
    }
    if sweep {
        state.casts.borrow_mut().retain(|c| !c.dead.get());
    }
}

impl Cast {
    fn feed(&self, state: &Rc<State>) {
        if !self.node.borrow().ready() {
            return;
        }
        let now = Time::now().nsec();
        if now.saturating_sub(self.last.get()) < self.frame_ns - self.frame_ns / 10 {
            return;
        }
        let Some((idx, w, h)) = crate::output::output_geometry(state, &self.output_name) else {
            return;
        };
        // a mode change under a live cast: hold off; renegotiation is later
        if w != self.width || h != self.height {
            return;
        }
        let Some(region) = crate::rect::Rect::new_sized(0, 0, w as i32, h as i32) else {
            return;
        };
        let Some(px) = crate::output::screencopy(state, idx, region, self.cursor) else {
            return;
        };
        self.node.borrow_mut().produce(|dst, _| {
            let n = px.len().min(dst.len());
            dst[..n].copy_from_slice(&px[..n]);
        });
        self.last.set(now);
    }
}
