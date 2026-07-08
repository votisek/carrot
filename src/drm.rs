// atomic kms. per-connector state, double-buffered scanout, hw cursor on its
// own plane flushed independent of flips. EBUSY rebuilds the whole commit
// (fence + cursor) and retries - never a partial. OUT_FENCE_PTR on every
// commit, even after a failure. hotplug is a raw netlink uevent socket.

pub mod atomic;
pub mod connector;
pub mod device;
pub mod sys;
pub mod uevent;

/// obj ids and prop ids travel together; typing them apart turns a
/// transposition into a compile error, not a silent garbage commit. sys.rs
/// stays raw u32 at the abi seam.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ObjId(pub u32);

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct PropId(pub u32);

impl std::fmt::Display for ObjId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
