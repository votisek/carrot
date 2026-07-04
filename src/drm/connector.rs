// per-output state and the two commit paths: full modeset and steady-state
// flip. EBUSY is a soft-fail (not-presented; next trigger rebuilds the commit).
// EACCES means the master went away (vt switch) - the caller's problem.

use crate::drm::atomic::{self, Change};
use crate::drm::device::{Crtc, DrmDevice, DrmError, Plane, PlaneType, PropSet};
use crate::drm::sys::{self, FlipComplete, ModeInfo};
use crate::drm::{ObjId, PropId};
use crate::format::{ARGB8888, XRGB8888};
use crate::util::AsyncEvent;
use rustix::io::Errno;
use std::cell::{Cell, RefCell};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::rc::Rc;

pub struct Connector {
    pub id: ObjId,
    prop_crtc_id: PropId,
    pub connected: Cell<bool>,
    pub modes: RefCell<Vec<ModeInfo>>,
    pub pipe: RefCell<Option<Pipe>>,
    pub flip_pending: Cell<bool>,
    /// fires on every flip completion; the present loop hangs off this
    pub vblank: AsyncEvent,
    pub sequence: Cell<u32>,
    out_fence: Cell<Option<OwnedFd>>,
    /// reused commit buffer; vecs keep capacity across frames
    change: RefCell<Change>,
}

/// connector wired to hardware: crtc + planes + chosen mode
pub struct Pipe {
    pub crtc: Rc<Crtc>,
    pub primary: Rc<Plane>,
    pub cursor: Option<CursorPlane>,
    pub mode: ModeInfo,
    mode_blob: Cell<u32>,
    pub active: Cell<bool>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FlipResult {
    Queued,
    /// nothing reached the screen and nothing is pending; retry next trigger
    NotPresented,
}

impl Connector {
    pub fn probe(dev: &Rc<DrmDevice>, raw_id: u32) -> Result<Rc<Connector>, DrmError> {
        let id = ObjId(raw_id);
        let info = sys::connector(dev.fd.as_fd(), raw_id, true)
            .map_err(|e| DrmError::ObjOp("connector probe", raw_id, e))?;
        let props = PropSet::of(dev.fd.as_fd(), id, sys::OBJECT_CONNECTOR)?;
        Ok(Rc::new(Connector {
            id,
            prop_crtc_id: props.require("CRTC_ID", id)?,
            connected: Cell::new(info.connection == 1),
            modes: RefCell::new(info.modes),
            pipe: RefCell::new(None),
            flip_pending: Cell::new(false),
            vblank: AsyncEvent::default(),
            sequence: Cell::new(0),
            out_fence: Cell::new(None),
            change: RefCell::new(Change::default()),
        }))
    }

    /// greedy first-fit: crtc + primary plane + cursor plane if available.
    /// an unsatisfiable connector fails bring-up rather than coming up headless
    pub fn assign_pipe(self: &Rc<Self>, dev: &DrmDevice) -> Result<(), DrmError> {
        if !self.connected.get() || self.pipe.borrow().is_some() {
            return Ok(());
        }
        let Some(&mode) = self.modes.borrow().first() else {
            // connected but modeless; treat like disconnected
            return Ok(());
        };

        let info = sys::connector(dev.fd.as_fd(), self.id.0, false)
            .map_err(|e| DrmError::ObjOp("connector", self.id.0, e))?;
        let mut crtc_mask = 0u32;
        for &enc in &info.encoders {
            crtc_mask |= sys::encoder_possible_crtcs(dev.fd.as_fd(), enc)
                .map_err(|e| DrmError::ObjOp("encoder", enc, e))?;
        }
        let crtc = dev
            .crtcs
            .iter()
            .find(|c| crtc_mask & (1 << c.idx) != 0 && c.connector.get() == ObjId(0))
            .cloned()
            .ok_or(DrmError::NoCrtc(self.id))?;

        let take_plane = |ty: PlaneType, fourcc: u32| {
            dev.planes
                .iter()
                .find(|p| {
                    p.ty == ty
                        && p.possible_crtcs & (1 << crtc.idx) != 0
                        && p.crtc.get() == ObjId(0)
                        && p.supports(fourcc)
                })
                .cloned()
        };
        let primary = take_plane(PlaneType::Primary, XRGB8888.drm)
            .ok_or(DrmError::NoPrimaryPlane(self.id))?;
        primary.crtc.set(crtc.id);
        // cursor is best-effort; an output without one still works
        let cursor = take_plane(PlaneType::Cursor, ARGB8888.drm).and_then(|p| {
            p.crtc.set(crtc.id);
            match CursorPlane::new(dev, p.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("carrot: cursor plane setup failed, continuing without: {e}");
                    p.crtc.set(ObjId(0));
                    None
                }
            }
        });

        crtc.connector.set(self.id);
        *self.pipe.borrow_mut() = Some(Pipe {
            crtc,
            primary,
            cursor,
            mode,
            mode_blob: Cell::new(0),
            active: Cell::new(false),
        });
        Ok(())
    }

    /// full modeset onto `fb`; blocking commit, ALLOW_MODESET
    pub fn modeset(&self, dev: &DrmDevice, fb: u32) -> Result<(), DrmError> {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return Ok(());
        };
        let old_blob = pipe.mode_blob.get();
        let blob = create_mode_blob(dev, &pipe.mode)?;
        pipe.mode_blob.set(blob);

        let mut out_fd: i32 = -1;
        let mut ch = self.change.borrow_mut();
        ch.clear();
        ch.set(self.id, self.prop_crtc_id, pipe.crtc.id.0 as u64);
        ch.set(pipe.crtc.id, pipe.crtc.props.active, 1);
        ch.set(pipe.crtc.id, pipe.crtc.props.mode_id, blob as u64);
        ch.set(
            pipe.crtc.id,
            pipe.crtc.props.out_fence_ptr,
            (&raw mut out_fd) as usize as u64,
        );
        set_plane(&mut ch, &pipe.primary, pipe.crtc.id, fb, &pipe.mode);
        if let Some(cur) = &pipe.cursor {
            cur.apply(&mut ch, pipe.crtc.id);
        }
        match ch.commit(dev.fd.as_fd(), atomic::ALLOW_MODESET, 0) {
            Ok(()) => {
                self.take_out_fence(out_fd);
                if let Some(cur) = &pipe.cursor {
                    cur.commit_done();
                }
                pipe.active.set(true);
                if old_blob != 0 {
                    let _ = sys::destroy_blob(dev.fd.as_fd(), old_blob);
                }
                Ok(())
            }
            Err(e) => {
                pipe.mode_blob.set(old_blob);
                let _ = sys::destroy_blob(dev.fd.as_fd(), blob);
                Err(commit_error(e))
            }
        }
    }

    /// steady-state page flip, one attempt. EBUSY is not-presented; the next
    /// trigger rebuilds the whole commit, so nothing partial survives
    pub fn flip(
        &self,
        dev: &DrmDevice,
        fb: u32,
        in_fence: Option<i32>,
    ) -> Result<FlipResult, DrmError> {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return Ok(FlipResult::NotPresented);
        };
        if self.flip_pending.get() {
            return Ok(FlipResult::NotPresented);
        }
        let mut out_fd: i32 = -1;
        let mut ch = self.change.borrow_mut();
        ch.clear();
        ch.set(
            pipe.crtc.id,
            pipe.crtc.props.out_fence_ptr,
            (&raw mut out_fd) as usize as u64,
        );
        ch.set(pipe.primary.id, pipe.primary.props.fb_id, fb as u64);
        if let (Some(prop), Some(fence)) = (pipe.primary.props.in_fence_fd, in_fence) {
            ch.set(pipe.primary.id, prop, fence as u64);
        }
        if let Some(cur) = &pipe.cursor {
            cur.apply(&mut ch, pipe.crtc.id);
        }
        match ch.commit(dev.fd.as_fd(), atomic::NONBLOCK | atomic::PAGE_FLIP_EVENT, 0) {
            Ok(()) => {
                self.take_out_fence(out_fd);
                if let Some(cur) = &pipe.cursor {
                    cur.commit_done();
                }
                self.flip_pending.set(true);
                Ok(FlipResult::Queued)
            }
            Err(Errno::BUSY) => Ok(FlipResult::NotPresented),
            Err(e) => Err(commit_error(e)),
        }
    }

    /// rip the cursor off the plane on vt-away while master is still held, so
    /// the next compositor never sees our arrow. commit BLOCKS: a flip is nearly
    /// always in flight and a nonblock attempt would EBUSY, leaving the arrow.
    pub fn cursor_hide(&self, dev: &DrmDevice) {
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return;
        };
        let Some(cur) = &pipe.cursor else {
            return;
        };
        // unconditional: soft state may disagree with the kernel's view, which
        // is what actually bleeds through
        let p = &cur.plane;
        let mut ch = self.change.borrow_mut();
        ch.clear();
        ch.set(p.id, p.props.fb_id, 0);
        ch.set(p.id, p.props.crtc_id, 0);
        if let Err(e) = ch.commit(dev.fd.as_fd(), 0, 0) {
            eprintln!("carrot: cursor teardown failed, expect a stray arrow: {e}");
        }
        cur.set_enabled(false);
        cur.commit_done();
    }

    /// cursor-only update between flips; a pending flip already carries the
    /// change. no out-fence: nothing new gets scanned out
    pub fn cursor_commit(&self, dev: &DrmDevice) {
        if self.flip_pending.get() {
            return;
        }
        let pipe = self.pipe.borrow();
        let Some(pipe) = pipe.as_ref() else {
            return;
        };
        if !pipe.active.get() {
            return;
        }
        let Some(cur) = &pipe.cursor else {
            return;
        };
        if !cur.changed() {
            return;
        }
        let mut ch = self.change.borrow_mut();
        ch.clear();
        cur.apply(&mut ch, pipe.crtc.id);
        match ch.commit(dev.fd.as_fd(), atomic::NONBLOCK, 0) {
            Ok(()) => cur.commit_done(),
            // contention: next motion retries
            Err(Errno::BUSY) => {}
            Err(e) => crate::trace!("cursor commit: {e}"),
        }
    }

    pub fn flip_done(&self, ev: &FlipComplete) {
        self.flip_pending.set(false);
        self.sequence.set(ev.sequence);
        self.vblank.trigger();
    }

    /// previous frame's scanout fence; the renderer waits on it before
    /// drawing into the buffer being replaced
    pub fn take_scanout_fence(&self) -> Option<OwnedFd> {
        self.out_fence.take()
    }

    fn take_out_fence(&self, out_fd: i32) {
        if out_fd != -1 {
            self.out_fence
                .set(Some(unsafe { OwnedFd::from_raw_fd(out_fd) }));
        }
    }
}

fn set_plane(ch: &mut Change, plane: &Plane, crtc_id: ObjId, fb: u32, mode: &ModeInfo) {
    let w = mode.hdisplay as u64;
    let h = mode.vdisplay as u64;
    ch.set(plane.id, plane.props.crtc_id, crtc_id.0 as u64);
    ch.set(plane.id, plane.props.fb_id, fb as u64);
    ch.set(plane.id, plane.props.src_x, 0);
    ch.set(plane.id, plane.props.src_y, 0);
    ch.set(plane.id, plane.props.src_w, w << 16);
    ch.set(plane.id, plane.props.src_h, h << 16);
    ch.set(plane.id, plane.props.crtc_x, 0);
    ch.set(plane.id, plane.props.crtc_y, 0);
    ch.set(plane.id, plane.props.crtc_w, w);
    ch.set(plane.id, plane.props.crtc_h, h);
}

fn create_mode_blob(dev: &DrmDevice, mode: &ModeInfo) -> Result<u32, DrmError> {
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (mode as *const ModeInfo) as *const u8,
            std::mem::size_of::<ModeInfo>(),
        )
    };
    sys::create_blob(dev.fd.as_fd(), bytes).map_err(|e| DrmError::Op("mode blob", e))
}

fn commit_error(e: Errno) -> DrmError {
    if e == Errno::ACCESS {
        DrmError::LostMaster
    } else {
        DrmError::Op("atomic commit", e)
    }
}

// -- cursor --

/// double-buffered dumb buffers, cpu-written, ARGB. changes ride along in
/// whatever commit goes out next; a bare move without content damage still
/// needs a flip-shaped commit from the caller
pub struct CursorPlane {
    pub plane: Rc<Plane>,
    bufs: [CursorBuf; 2],
    back: Cell<usize>,
    pub width: u32,
    pub height: u32,
    x: Cell<i32>,
    y: Cell<i32>,
    enabled: Cell<bool>,
    changed: Cell<bool>,
    /// back buffer got new content, rotate on next commit
    swap: Cell<bool>,
    /// subtracted from the position so the click point sits under the tip
    pub hotspot: Cell<(i32, i32)>,
}

struct CursorBuf {
    fd: Rc<OwnedFd>,
    handle: u32,
    fb: u32,
    pitch: u32,
    size: u64,
    map: *mut u8,
}

impl CursorPlane {
    pub fn new(dev: &DrmDevice, plane: Rc<Plane>) -> Result<CursorPlane, DrmError> {
        let (w, h) = dev.cursor_size;
        let mk = || -> Result<CursorBuf, DrmError> {
            let db = sys::create_dumb(dev.fd.as_fd(), w, h, 32)
                .map_err(|e| DrmError::Op("create cursor dumb", e))?;
            let offset = sys::map_dumb(dev.fd.as_fd(), db.handle)
                .map_err(|e| DrmError::Op("map cursor dumb", e))?;
            let map = unsafe {
                rustix::mm::mmap(
                    std::ptr::null_mut(),
                    db.size as usize,
                    rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                    rustix::mm::MapFlags::SHARED,
                    dev.fd.as_fd(),
                    offset,
                )
                .map_err(|e| DrmError::Op("mmap cursor", e))?
            } as *mut u8;
            let fb = sys::addfb2(
                dev.fd.as_fd(),
                w,
                h,
                ARGB8888.drm,
                &[db.handle],
                &[db.pitch],
                &[0],
                None,
            )
            .map_err(|e| DrmError::Op("cursor addfb2", e))?;
            Ok(CursorBuf {
                fd: dev.fd.clone(),
                handle: db.handle,
                fb,
                pitch: db.pitch,
                size: db.size,
                map,
            })
        };
        Ok(CursorPlane {
            plane,
            bufs: [mk()?, mk()?],
            back: Cell::new(0),
            width: w,
            height: h,
            x: Cell::new(0),
            y: Cell::new(0),
            enabled: Cell::new(false),
            changed: Cell::new(false),
            swap: Cell::new(false),
            hotspot: Cell::new((0, 0)),
        })
    }

    /// copy tightly-packed ARGB rows into the back buffer, cleared first so a
    /// smaller cursor never shows stale edges
    pub fn write(&self, pixels: &[u8], w: u32, h: u32) {
        let w = w.min(self.width);
        let h = h.min(self.height);
        let buf = &self.bufs[self.back.get()];
        let src_stride = (w * 4) as usize;
        unsafe {
            std::ptr::write_bytes(buf.map, 0, buf.size as usize);
            for row in 0..h as usize {
                let src = &pixels[row * src_stride..][..src_stride];
                let dst = buf.map.add(row * buf.pitch as usize);
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src_stride);
            }
        }
        self.swap.set(true);
        self.changed.set(true);
    }

    pub fn set_position(&self, x: i32, y: i32) {
        if self.x.get() != x || self.y.get() != y {
            self.x.set(x);
            self.y.set(y);
            self.changed.set(true);
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.get() != enabled {
            self.enabled.set(enabled);
            self.changed.set(true);
        }
    }

    pub fn changed(&self) -> bool {
        self.changed.get()
    }

    fn apply(&self, ch: &mut Change, crtc_id: ObjId) {
        if !self.changed.get() {
            return;
        }
        let p = &self.plane;
        if self.enabled.get() {
            // fresh content in the back buffer shows that, else the front one
            let idx = if self.swap.get() {
                self.back.get()
            } else {
                1 - self.back.get()
            };
            let w = self.width as u64;
            let h = self.height as u64;
            ch.set(p.id, p.props.crtc_id, crtc_id.0 as u64);
            ch.set(p.id, p.props.fb_id, self.bufs[idx].fb as u64);
            ch.set(p.id, p.props.src_x, 0);
            ch.set(p.id, p.props.src_y, 0);
            ch.set(p.id, p.props.src_w, w << 16);
            ch.set(p.id, p.props.src_h, h << 16);
            ch.set(p.id, p.props.crtc_x, self.x.get() as i64 as u64);
            ch.set(p.id, p.props.crtc_y, self.y.get() as i64 as u64);
            ch.set(p.id, p.props.crtc_w, w);
            ch.set(p.id, p.props.crtc_h, h);
        } else {
            ch.set(p.id, p.props.fb_id, 0);
            ch.set(p.id, p.props.crtc_id, 0);
        }
    }

    fn commit_done(&self) {
        if self.swap.get() {
            self.back.set(1 - self.back.get());
            self.swap.set(false);
        }
        self.changed.set(false);
    }
}

impl Drop for CursorBuf {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.map.cast(), self.size as usize);
        }
        let _ = sys::rmfb(self.fd.as_fd(), self.fb);
        let _ = sys::destroy_dumb(self.fd.as_fd(), self.handle);
    }
}
