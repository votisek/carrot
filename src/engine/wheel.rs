// timer wheel - coarse millisecond timeouts (kill deadlines, animation ticks).
// precision consumers (key repeat, presentation) take a raw absolute-ns ring
// timeout instead.
//
// one timerfd armed to the earliest deadline, a min-heap of expirations, one
// dispatcher task reading the fd through the ring. cancellation drops the future:
// the waiter is deregistered, its heap entry goes stale and is skipped at the
// next fire. deadlines round up to whole ms so nearby timers coalesce.

use crate::engine::{Engine, SpawnedFuture};
use crate::uring::Ring;
use crate::util::{IdHashMap, NumCell, Time};
use rustix::io::Errno;
use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};
use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

// -- errors --

#[derive(Debug)]
pub enum WheelError {
    CreateFailed(Errno),
    ArmFailed(Errno),
    Destroyed,
}

impl fmt::Display for WheelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WheelError::CreateFailed(e) => write!(f, "creating the timerfd failed: {e}"),
            WheelError::ArmFailed(e) => write!(f, "arming the timerfd failed: {e}"),
            WheelError::Destroyed => write!(f, "the wheel is shut down"),
        }
    }
}

impl std::error::Error for WheelError {}

// -- wheel --

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    at: Time,
    id: u64,
}

pub struct Wheel {
    data: Rc<WheelData>,
}

struct WheelData {
    destroyed: Cell<bool>,
    eng: Rc<Engine>,
    fd: Rc<OwnedFd>,
    /// per-wheel, never reused; 0 is the dead sentinel
    next_id: NumCell<u64>,
    /// deadline the timerfd is currently set to
    armed: Cell<Option<Time>>,
    /// liveness lives here; the heap only orders deadlines and may hold stale ids
    waiters: RefCell<IdHashMap<u64, Rc<TimeoutData>>>,
    heap: RefCell<BinaryHeap<Reverse<Entry>>>,
    dispatcher: Cell<Option<SpawnedFuture<()>>>,
}

impl Wheel {
    pub fn new(eng: &Rc<Engine>, ring: &Rc<Ring>) -> Result<Wheel, WheelError> {

        let fd = timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::CLOEXEC)
            .map_err(WheelError::CreateFailed)?;
        let data = Rc::new(WheelData {
            destroyed: Cell::new(false),
            eng: eng.clone(),
            fd: Rc::new(fd),
            next_id: NumCell::new(1),
            armed: Cell::new(None),
            waiters: RefCell::new(IdHashMap::default()),
            heap: RefCell::new(BinaryHeap::new()),
            dispatcher: Cell::new(None),
        });
        let task_data = data.clone();
        let ring = ring.clone();
        data.dispatcher.set(Some(eng.spawn("wheel", async move {
            task_data.dispatch_loop(&ring).await;
        })));
        Ok(Wheel { data })
    }

    /// one-shot. drop the future to cancel; loop for periodic.
    pub fn timeout(&self, ms: u64) -> WheelTimeout {
        self.data.timeout(ms)
    }

    pub fn clear(&self) {
        self.data.kill();
    }
}

impl Drop for Wheel {
    fn drop(&mut self) {
        self.data.kill();
    }
}

impl WheelData {
    fn timeout(self: &Rc<Self>, ms: u64) -> WheelTimeout {
        let data = Rc::new(TimeoutData {
            id: Cell::new(0),
            done: Cell::new(None),
            waker: Cell::new(None),
            wheel: self.clone(),
        });
        if self.destroyed.get() {
            data.done.set(Some(Err(WheelError::Destroyed)));
            return WheelTimeout { data };
        }
        let id = self.next_id.fetch_add(1);
        data.id.set(id);
        let deadline = (self.eng.now() + Duration::from_millis(ms)).round_up_ms();
        let need_arm = match self.armed.get() {
            // only a strictly earlier deadline re-arms; later ones ride the
            // pending fire and get picked up at re-arm time
            Some(at) => deadline < at,
            None => true,
        };
        if need_arm {
            if let Err(e) = self.arm(deadline) {
                data.done.set(Some(Err(WheelError::ArmFailed(e))));
                return WheelTimeout { data };
            }
            self.armed.set(Some(deadline));
        }
        self.heap.borrow_mut().push(Reverse(Entry { at: deadline, id }));
        self.waiters.borrow_mut().insert(id, data.clone());
        WheelTimeout { data }
    }

    fn arm(&self, at: Time) -> Result<(), Errno> {
        let ns = at.nsec();
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: (ns / 1_000_000_000) as i64,
                tv_nsec: (ns % 1_000_000_000) as i64,
            },
        };
        timerfd_settime(&*self.fd, TimerfdTimerFlags::ABSTIME, &spec).map(drop)
    }

    async fn dispatch_loop(self: &Rc<Self>, ring: &Rc<Ring>) {
        let mut buf = vec![0u8; 8];
        loop {
            match ring.read(&self.fd, buf).await {
                Ok((b, _)) => {
                    buf = b;
                    self.fire();
                }
                Err(_e) => {
                    // a dead timerfd read kills every outstanding timeout; waiters see Destroyed
                    crate::trace!("wheel read failed: {}", _e);
                    self.kill();
                    return;
                }
            }
        }
    }

    fn fire(&self) {
        let now = self.eng.now();
        loop {
            let expired = {
                let mut heap = self.heap.borrow_mut();
                match heap.peek() {
                    Some(&Reverse(e)) if e.at <= now => {
                        heap.pop();
                        Some(e)
                    }
                    _ => None,
                }
            };
            let Some(e) = expired else { break };
            // stale ids (cancelled waiters) fall through
            if let Some(w) = self.waiters.borrow_mut().remove(&e.id) {
                w.complete(Ok(()));
            }
        }
        self.armed.set(None);
        // re-arm to the earliest live entry, shedding stale ones
        loop {
            let next = {
                let mut heap = self.heap.borrow_mut();
                loop {
                    match heap.peek() {
                        Some(&Reverse(e)) if !self.waiters.borrow().contains_key(&e.id) => {
                            heap.pop();
                        }
                        Some(&Reverse(e)) => break Some(e),
                        None => break None,
                    }
                }
            };
            let Some(e) = next else { break };
            match self.arm(e.at) {
                Ok(()) => {
                    self.armed.set(Some(e.at));
                    break;
                }
                Err(err) => {
                    if let Some(w) = self.waiters.borrow_mut().remove(&e.id) {
                        w.complete(Err(WheelError::ArmFailed(err)));
                    }
                    self.heap.borrow_mut().pop();
                }
            }
        }
    }

    fn kill(&self) {
        if self.destroyed.replace(true) {
            return;
        }
        // dropping the dispatcher aborts its in-flight read
        self.dispatcher.take();
        let waiters: Vec<_> = self.waiters.borrow_mut().drain().map(|(_, w)| w).collect();
        for w in waiters {
            w.complete(Err(WheelError::Destroyed));
        }
        self.heap.borrow_mut().clear();
    }
}

// -- the future --

struct TimeoutData {
    id: Cell<u64>,
    done: Cell<Option<Result<(), WheelError>>>,
    waker: Cell<Option<Waker>>,
    wheel: Rc<WheelData>,
}

impl TimeoutData {
    fn complete(&self, r: Result<(), WheelError>) {
        self.done.set(Some(r));
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

pub struct WheelTimeout {
    data: Rc<TimeoutData>,
}

impl Future for WheelTimeout {
    type Output = Result<(), WheelError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.data.done.take() {
            Some(r) => Poll::Ready(r),
            None => {
                self.data.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

impl Drop for WheelTimeout {
    fn drop(&mut self) {
        self.data
            .wheel
            .waiters
            .borrow_mut()
            .remove(&self.data.id.get());
        self.data.waker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let wheel = Wheel::new(&eng, &ring).unwrap();
        let t = wheel.timeout(5);
        let ok = Rc::new(Cell::new(false));
        let k = ok.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            if t.await.is_ok() {
                k.set(true);
            }
            r.stop();
        });
        ring.run().unwrap();
        assert!(ok.get());
    }

    #[test]
    fn cancel_leaves_others_alone() {
        let eng = Engine::new();
        let ring = Ring::new(&eng, 32).unwrap();
        let wheel = Wheel::new(&eng, &ring).unwrap();
        // the earlier timer is dropped - its stale fire must not disturb the later one
        let doomed = wheel.timeout(1);
        let kept = wheel.timeout(20);
        let ok = Rc::new(Cell::new(false));
        let k = ok.clone();
        let r = ring.clone();
        let _root = eng.spawn("test", async move {
            drop(doomed);
            if kept.await.is_ok() {
                k.set(true);
            }
            r.stop();
        });
        ring.run().unwrap();
        assert!(ok.get());
    }
}
