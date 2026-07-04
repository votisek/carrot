// atomic commit building. a Change is four parallel arrays mirroring
// drm_mode_atomic; consecutive props for one object merge into one entry.

use super::sys;
use super::{ObjId, PropId};
use rustix::io::Errno;
use std::os::fd::BorrowedFd;

pub const PAGE_FLIP_EVENT: u32 = 0x01;
#[allow(dead_code)]
pub const PAGE_FLIP_ASYNC: u32 = 0x02;
#[allow(dead_code)]
pub const TEST_ONLY: u32 = 0x100;
#[allow(dead_code)]
pub const NONBLOCK: u32 = 0x200;
pub const ALLOW_MODESET: u32 = 0x400;

#[derive(Default)]
pub struct Change {
    objects: Vec<u32>,
    lengths: Vec<u32>,
    props: Vec<u32>,
    values: Vec<u64>,
}

impl Change {
    pub fn set(&mut self, object: ObjId, prop: PropId, value: u64) {
        if self.objects.last() != Some(&object.0) {
            self.objects.push(object.0);
            self.lengths.push(0);
        }
        *self.lengths.last_mut().unwrap() += 1;
        self.props.push(prop.0);
        self.values.push(value);
    }

    pub fn commit(&self, fd: BorrowedFd<'_>, flags: u32, user_data: u64) -> Result<(), Errno> {
        sys::atomic(
            fd,
            flags,
            &self.objects,
            &self.lengths,
            &self.props,
            &self.values,
            user_data,
        )
    }

    pub fn clear(&mut self) {
        self.objects.clear();
        self.lengths.clear();
        self.props.clear();
        self.values.clear();
    }
}
