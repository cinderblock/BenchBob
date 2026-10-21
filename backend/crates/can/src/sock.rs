//! A SocketCAN raw socket (cansock.ts's `CanSocket`), through libc: socket, setsockopt,
//! bind, recvmsg. No candump child to parse.
//!
//! The TypeScript drained the socket with non-blocking recvmsg() on a 5–20 ms timer (Bun had
//! no cheap way to wait on an fd). Here a tokio task waits for readiness and drains up to
//! 8192 datagrams per wakeup, handing them over in one batch. recvmsg also gives the kernel
//! RX timestamp, the MSG_CONFIRM flag on our own echoed frames and the drop counter.
//!
//! Timestamps use SO_TIMESTAMP_NEW (Linux 5.1+: a 64-bit timeval on every architecture, so
//! the 32-bit ARM builds read them right), falling back to the old SO_TIMESTAMP.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::io::unix::AsyncFd;

use crate::frame::{CAN_ERR_MASK, CANFD_MTU, CanFrame, encode_frame, now_ms_f64, split_frames};

const AF_CAN: libc::c_int = 29;
const CAN_RAW: libc::c_int = 1;
const SOL_CAN_RAW: libc::c_int = 101;
const CAN_RAW_ERR_FILTER: libc::c_int = 2;
const CAN_RAW_LOOPBACK: libc::c_int = 3;
const CAN_RAW_RECV_OWN_MSGS: libc::c_int = 4;
const CAN_RAW_FD_FRAMES: libc::c_int = 5;
const SO_TIMESTAMP_OLD: libc::c_int = 29;
const SO_TIMESTAMP_NEW: libc::c_int = 63;
const SO_RXQ_OVFL: libc::c_int = 40;
/// Set on frames looped back from another local socket.
const MSG_DONTROUTE: libc::c_int = 0x4;
/// Set on our own frames echoed back (CAN_RAW_RECV_OWN_MSGS).
const MSG_CONFIRM: libc::c_int = 0x800;

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Receive (and allow sending) CAN FD frames.
    pub fd_frames: bool,
    /// CAN_ERR_* classes to receive as error frames; 0 = none.
    pub err_mask: u32,
    /// Also receive frames this socket sent (after the controller confirms them).
    pub recv_own: bool,
    /// Let other local sockets see our frames (kernel default: on).
    pub no_loopback: bool,
    /// SO_RCVBUF bytes (capped by the kernel at net.core.rmem_max).
    pub rcvbuf: Option<u32>,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn strerror(e: i32) -> String {
    // "No such device (os error 19)" → "No such device"
    let s = std::io::Error::from_raw_os_error(e).to_string();
    match s.rfind(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn errno_msg(what: &str) -> String {
    let e = errno();
    format!("{what}: {} (errno {e})", strerror(e))
}

/// Node's `e.code` for the errors a CAN write can hit.
pub fn errno_code(e: i32) -> String {
    match e {
        libc::EAGAIN => "EAGAIN",
        libc::ENOBUFS => "ENOBUFS",
        libc::ENETDOWN => "ENETDOWN",
        libc::EINVAL => "EINVAL",
        libc::ENXIO => "ENXIO",
        libc::ENODEV => "ENODEV",
        libc::EMSGSIZE => "EMSGSIZE",
        libc::EBADF => "EBADF",
        libc::EIO => "EIO",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::EINTR => "EINTR",
        _ => return strerror(e),
    }
    .to_string()
}

fn setopt(fd: libc::c_int, level: libc::c_int, name: libc::c_int, v: u32) -> bool {
    // SAFETY: a 4-byte option value that lives across the call.
    unsafe { libc::setsockopt(fd, level, name, (&v as *const u32).cast(), 4) >= 0 }
}

/// The socket's shared half: the fd (for sending) and the counters.
pub struct Shared {
    fd: AsyncFd<OwnedFd>,
    pub iface: String,
    pub fd_frames: bool,
    closed: AtomicBool,
    /// Frames the kernel dropped because our receive queue was full (SO_RXQ_OVFL, cumulative).
    drops: AtomicU32,
    ts_new: bool,
}

/// A bound raw CAN socket. Frames arrive in batches through `on_frames`; `on_close` runs once
/// when the read side failed (ENETDOWN when the interface goes down), with the error.
pub struct CanSocket {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

pub type OnFrames = Box<dyn FnMut(Vec<CanFrame>) + Send>;
pub type OnClose = Box<dyn FnOnce(String) + Send>;

impl CanSocket {
    /// Needs a tokio runtime.
    pub fn open(iface: &str, opts: &Options, mut on_frames: OnFrames, on_close: Option<OnClose>) -> Result<CanSocket, String> {
        let cname = CString::new(iface).map_err(|_| format!("No such interface: {iface}"))?;
        // SAFETY: a NUL-terminated name.
        let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        if ifindex == 0 {
            return Err(format!("No such interface: {iface}"));
        }
        // SAFETY: plain socket(2).
        let raw = unsafe { libc::socket(AF_CAN, libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, CAN_RAW) };
        if raw < 0 {
            return Err(errno_msg("socket(AF_CAN)"));
        }
        // SAFETY: we own the fresh fd; OwnedFd closes it on every error path below.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let opt = |level, name, v: u32, label: &str| if setopt(raw, level, name, v) { Ok(()) } else { Err(errno_msg(label)) };
        if opts.fd_frames {
            opt(SOL_CAN_RAW, CAN_RAW_FD_FRAMES, 1, "CAN_RAW_FD_FRAMES")?;
        }
        if opts.err_mask != 0 {
            opt(SOL_CAN_RAW, CAN_RAW_ERR_FILTER, opts.err_mask & CAN_ERR_MASK, "CAN_RAW_ERR_FILTER")?;
        }
        if opts.recv_own {
            opt(SOL_CAN_RAW, CAN_RAW_RECV_OWN_MSGS, 1, "CAN_RAW_RECV_OWN_MSGS")?;
        }
        if opts.no_loopback {
            opt(SOL_CAN_RAW, CAN_RAW_LOOPBACK, 0, "CAN_RAW_LOOPBACK")?;
        }
        let ts_new = setopt(raw, libc::SOL_SOCKET, SO_TIMESTAMP_NEW, 1);
        if !ts_new {
            opt(libc::SOL_SOCKET, SO_TIMESTAMP_OLD, 1, "SO_TIMESTAMP")?;
        }
        opt(libc::SOL_SOCKET, SO_RXQ_OVFL, 1, "SO_RXQ_OVFL")?;
        // Best effort: SO_RCVBUF is silently capped at net.core.rmem_max, never an error.
        if let Some(b) = opts.rcvbuf {
            setopt(raw, libc::SOL_SOCKET, libc::SO_RCVBUF, b);
        }
        // struct sockaddr_can: family@0 ifindex@4, 24 bytes.
        let mut sa = [0u8; 24];
        sa[0..2].copy_from_slice(&(AF_CAN as u16).to_ne_bytes());
        sa[4..8].copy_from_slice(&(ifindex as i32).to_ne_bytes());
        // SAFETY: a 24-byte sockaddr_can.
        if unsafe { libc::bind(raw, sa.as_ptr().cast(), 24) } < 0 {
            return Err(errno_msg(&format!("bind({iface})")));
        }
        // SAFETY: the OwnedFd is open and moves into the AsyncFd, which owns it from here on.
        let fd = unsafe { AsyncFd::register_with_interest(owned, tokio::io::Interest::READABLE) }
            .map_err(|_| format!("epoll({iface}) failed"))?;
        let shared = Arc::new(Shared {
            fd,
            iface: iface.to_string(),
            fd_frames: opts.fd_frames,
            closed: AtomicBool::new(false),
            drops: AtomicU32::new(0),
            ts_new,
        });
        let sh = shared.clone();
        let task = tokio::spawn(async move {
            let mut on_close = on_close;
            let err = loop {
                let mut guard = match sh.fd.readable().await {
                    Ok(g) => g,
                    Err(e) => break e.to_string(),
                };
                let mut frames = Vec::new();
                match sh.drain(&mut frames) {
                    Ok(true) => guard.clear_ready(),
                    Ok(false) => {}
                    Err(e) => {
                        if !frames.is_empty() {
                            on_frames(frames);
                        }
                        break e;
                    }
                }
                drop(guard);
                if !frames.is_empty() {
                    on_frames(frames);
                }
            };
            if !sh.closed.swap(true, Ordering::SeqCst)
                && let Some(cb) = on_close.take()
            {
                cb(err);
            }
        });
        Ok(CanSocket { shared, task })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
    pub fn drops(&self) -> u32 {
        self.shared.drops.load(Ordering::Relaxed)
    }
    pub fn send(&self, f: &CanFrame) -> Result<(), String> {
        self.shared.send(f)
    }
    /// Close without calling on_close.
    pub fn close(self) {}
}

impl Drop for CanSocket {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

impl Shared {
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Fails (ENOBUFS when the TX queue is full, ENETDOWN, EINVAL for FD on a classic socket…).
    pub fn send(&self, f: &CanFrame) -> Result<(), String> {
        if self.closed() {
            return Err(format!("{}: socket closed", self.iface));
        }
        if f.fd && !self.fd_frames {
            return Err(format!("{} is not in CAN FD mode", self.iface));
        }
        let b = encode_frame(f);
        // SAFETY: write(2) of a buffer we own.
        let r = unsafe { libc::write(self.fd.get_ref().as_raw_fd(), b.as_ptr().cast(), b.len()) };
        if r < 0 {
            let e = errno();
            let hint = match e {
                libc::ENOBUFS | libc::EAGAIN => " (TX queue full: is anything ACKing on the bus?)",
                libc::ENETDOWN => " (interface is down)",
                _ => "",
            };
            return Err(format!("{}: send failed: {}{hint}", self.iface, errno_code(e)));
        }
        Ok(())
    }

    /// Read until EAGAIN (Ok(true)) or 8192 datagrams (Ok(false)); Err on a hard error.
    fn drain(&self, frames: &mut Vec<CanFrame>) -> Result<bool, String> {
        let fd = self.fd.get_ref().as_raw_fd();
        let mut buf = [0u8; CANFD_MTU];
        // Room for a 64-bit timestamp and the overflow counter, u64-aligned.
        let mut ctrl = [0u64; 16];
        for _ in 0..8192 {
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
            // SAFETY: an all-zero msghdr is valid; we fill the pointers next.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ctrl.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&ctrl) as _;
            // SAFETY: msg points at buffers that outlive the call.
            let r = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT) };
            if r < 0 {
                let e = errno();
                if e == libc::EAGAIN || e == libc::EINTR {
                    return Ok(true);
                }
                return Err(format!("{}: {} (errno {e})", self.iface, strerror(e)));
            }
            let r = r as usize;
            if r < crate::frame::CAN_MTU {
                continue;
            }
            let mut ts = 0f64;
            // SAFETY: walking the control messages the kernel just wrote, with libc's macros.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    let h = &*c;
                    let data = libc::CMSG_DATA(c);
                    if h.cmsg_level == libc::SOL_SOCKET {
                        if h.cmsg_type == SO_TIMESTAMP_NEW && self.ts_new {
                            let s = std::ptr::read_unaligned(data.cast::<i64>());
                            let us = std::ptr::read_unaligned(data.add(8).cast::<i64>());
                            ts = s as f64 * 1000.0 + us as f64 / 1000.0;
                        } else if h.cmsg_type == SO_TIMESTAMP_OLD && !self.ts_new {
                            let tv = std::ptr::read_unaligned(data.cast::<libc::timeval>());
                            ts = tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0;
                        } else if h.cmsg_type == SO_RXQ_OVFL {
                            self.drops.store(std::ptr::read_unaligned(data.cast::<u32>()), Ordering::Relaxed);
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            let at = frames.len();
            split_frames(&buf[..r], if ts != 0.0 { ts } else { now_ms_f64() }, frames);
            for f in &mut frames[at..] {
                if msg.msg_flags & MSG_CONFIRM != 0 {
                    f.own = true;
                } else if msg.msg_flags & MSG_DONTROUTE != 0 {
                    f.local = true;
                }
            }
        }
        Ok(false)
    }
}

/// Integration tests on a real vcan: set BENCHBOB_CAN_TEST_VCAN to an interface that is up
/// (e.g. `ip link add dev vcanbbtest0 type vcan && ip link set vcanbbtest0 mtu 72 up`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameSpec, Payload, make_frame};
    use tokio::sync::mpsc;

    fn iface() -> Option<String> {
        std::env::var("BENCHBOB_CAN_TEST_VCAN").ok().filter(|s| !s.is_empty())
    }

    #[test]
    fn missing_interface() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let e = CanSocket::open("nosuchcan0", &Options::default(), Box::new(|_| {}), None).err().unwrap();
            assert_eq!(e, "No such interface: nosuchcan0");
        });
    }

    #[tokio::test]
    async fn vcan_send_receive_own_and_local() {
        let Some(name) = iface() else { return eprintln!("BENCHBOB_CAN_TEST_VCAN not set: skipped") };
        let _g = crate::VCAN_LOCK.lock().await;
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let a = CanSocket::open(
            &name,
            &Options { fd_frames: true, recv_own: true, err_mask: CAN_ERR_MASK, ..Default::default() },
            Box::new(move |f| tx_a.send(f).unwrap()),
            None,
        )
        .unwrap();
        let b = CanSocket::open(&name, &Options::default(), Box::new(move |f| tx_b.send(f).unwrap()), None).unwrap();
        let f = make_frame(&FrameSpec { id: 291.0, data: Payload::Hex("DEADBEEF".into()), ..Default::default() }).unwrap();
        a.send(&f).unwrap();
        let got = rx_a.recv().await.unwrap();
        assert_eq!((got[0].id, got[0].own, got[0].data.clone()), (0x123, true, vec![0xde, 0xad, 0xbe, 0xef]));
        assert!((got[0].ts - now_ms_f64()).abs() < 5000.0, "kernel timestamp {}", got[0].ts);
        let got = rx_b.recv().await.unwrap();
        assert_eq!((got[0].id, got[0].local, got[0].own), (0x123, true, false));
        // FD on a classic socket is refused before the kernel sees it.
        let fd = make_frame(&FrameSpec { id: 16.0, fd: true, data: Payload::Hex("00".repeat(12)), ..Default::default() }).unwrap();
        assert!(b.send(&fd).unwrap_err().contains("not in CAN FD mode"));
        a.send(&fd).unwrap();
        let got = rx_a.recv().await.unwrap();
        assert!(got[0].fd && got[0].len == 12);
        // b didn't ask for FD frames: it never sees one.
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.recv()).await.is_err());
    }
}
