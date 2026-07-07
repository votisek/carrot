// int/term arrive on a signalfd and stop the ring; the normal run()
// epilogue is the graceful shutdown path. rustix wraps neither signalfd4
// nor a plain sigprocmask, so those two go through a raw syscall shim.

use crate::state::State;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;

const SIGINT: u32 = 2;
const SIGTERM: u32 = 15;

// x86_64 numbers; the kernel sigset is a plain u64 bitmask, bit signo-1
const SYS_RT_SIGPROCMASK: i64 = 14;
const SYS_SIGNALFD4: i64 = 289;
const SIG_BLOCK: i64 = 0;
const SIG_UNBLOCK: i64 = 1;
const SFD_CLOEXEC: i64 = 0o2000000;

unsafe fn syscall4(nr: i64, a: i64, b: i64, c: i64, d: i64) -> i64 {
    let ret: i64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

fn mask() -> u64 {
    (1 << (SIGINT - 1)) | (1 << (SIGTERM - 1))
}

/// block int/term and hand back the signalfd. must run before the first
/// thread spawns so the whole process inherits the mask and delivery
/// lands only on the fd. sigpipe stays on std's process-wide ignore.
pub fn install() -> Result<OwnedFd, String> {
    let set = mask();
    let r = unsafe {
        syscall4(SYS_RT_SIGPROCMASK, SIG_BLOCK, &set as *const u64 as i64, 0, 8)
    };
    if r < 0 {
        return Err(format!("sigprocmask: errno {}", -r));
    }
    let fd = unsafe { syscall4(SYS_SIGNALFD4, -1, &set as *const u64 as i64, 8, SFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("signalfd4: errno {}", -fd));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// child side of pre_exec: the blocked mask survives fork+exec, so spawned
/// apps must get a clean one. async-signal-safe - one raw syscall, no
/// allocation.
pub fn unblock_all_in_child() {
    let set: u64 = !0;
    unsafe {
        syscall4(SYS_RT_SIGPROCMASK, SIG_UNBLOCK, &set as *const u64 as i64, 0, 8);
    }
}

pub fn run(state: &Rc<State>, fd: OwnedFd) -> crate::engine::SpawnedFuture<()> {
    let st = state.clone();
    state.eng.spawn("signals", async move {
        let fd = Rc::new(fd);
        loop {
            // signalfd_siginfo is 128 bytes, ssi_signo the leading u32
            let buf = vec![0u8; 128];
            let (buf, n) = match st.ring.read(&fd, buf).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("carrot: signalfd read failed: {e:?}");
                    return;
                }
            };
            if n < 4 {
                continue;
            }
            let signo = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if signo == SIGINT || signo == SIGTERM {
                eprintln!("carrot: signal {signo}, shutting down");
                st.ring.stop();
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // proves the raw shim without touching the test process's mask
    #[test]
    fn signalfd_opens_via_the_shim() {
        const SFD_NONBLOCK: i64 = 0o4000;
        let set = mask();
        let fd = unsafe {
            syscall4(
                SYS_SIGNALFD4,
                -1,
                &set as *const u64 as i64,
                8,
                SFD_CLOEXEC | SFD_NONBLOCK,
            )
        };
        assert!(fd >= 0, "signalfd4 failed: errno {}", -fd);
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // nothing pending and nothing blocked - reads must EAGAIN, not hang
        let mut buf = [0u8; 128];
        let err = rustix::io::read(&fd, &mut buf).unwrap_err();
        assert_eq!(err, rustix::io::Errno::AGAIN);
    }

    #[test]
    fn the_mask_covers_exactly_int_and_term() {
        assert_eq!(mask(), (1 << 1) | (1 << 14));
    }
}
