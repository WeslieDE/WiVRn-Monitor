//! Shared protocol between the WiVRn-Monitor layer (client, in the game process) and the viewer
//! (server, listens on `~/.local/share/wivrn-monitor/monitor.sock`).
//!
//! Transport: `AF_UNIX`/`SOCK_SEQPACKET` (message boundaries preserved, EOF on peer exit). Every
//! message is exactly [`MSG_SIZE`] bytes. Frame pixels never travel over the socket: the layer sends
//! a memfd (SCM_RIGHTS) once per ring ("Setup") and afterwards only small `Frame`/`Release` messages.
//!
//! Flow: layer->viewer `Hello`; `Setup`(+memfd) whenever size/format changes; `Frame{slot}` after the
//! slot was filled; viewer->layer `Release{slot}` once it has consumed the slot. `gen` is the ring
//! generation (increments with every Setup) so stale Release messages can be ignored.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

pub const MAGIC: u32 = 0x4e56_4d57; // "WMVN"
pub const VERSION: u32 = 1;
pub const MSG_SIZE: usize = 48;
pub const MAX_SLOTS: u32 = 4;
pub const MAX_DIM: u32 = 8192;
pub const MAX_RING_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes/pixel, R,G,B,A byte order, sRGB-encoded values.
    Rgba8Srgb = 1,
    /// 4 bytes/pixel, R,G,B,A byte order, values stored as-is (treated as display-encoded).
    Rgba8Unorm = 2,
}

impl PixelFormat {
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Rgba8Srgb),
            2 => Some(Self::Rgba8Unorm),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Hello { version: u32, pid: u32 },
    Setup(RingInfo),
    Frame { gen: u32, slot: u32, frame_nr: u64, timestamp_ns: u64 },
    Release { gen: u32, slot: u32 },
}

/// Describes a shared-memory ring (`slots` slots of `slot_size` bytes each at `i * slot_size`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingInfo {
    pub gen: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub slots: u32,
    pub slot_size: u64,
}

impl RingInfo {
    pub fn new(gen: u32, width: u32, height: u32, format: PixelFormat, slots: u32) -> Self {
        let stride = width * 4;
        RingInfo { gen, width, height, stride, format, slots, slot_size: stride as u64 * height as u64 }
    }

    pub fn total_size(&self) -> u64 {
        self.slot_size * self.slots as u64
    }

    /// Checks all fields; must be called by the receiver before trusting/mapping a Setup.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.width == 0 || self.height == 0 || self.width > MAX_DIM || self.height > MAX_DIM {
            return Err("bad dimensions");
        }
        if self.slots == 0 || self.slots > MAX_SLOTS {
            return Err("bad slot count");
        }
        if self.stride < self.width * 4 || self.stride > MAX_DIM * 4 {
            return Err("bad stride");
        }
        if self.slot_size != self.stride as u64 * self.height as u64 {
            return Err("slot_size mismatch");
        }
        if self.total_size() > MAX_RING_BYTES {
            return Err("ring too large");
        }
        Ok(())
    }

    pub fn slot_range(&self, slot: u32) -> Option<std::ops::Range<usize>> {
        if slot >= self.slots {
            return None;
        }
        let start = slot as usize * self.slot_size as usize;
        Some(start..start + self.slot_size as usize)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Magic,
    Kind,
    Field,
}

const K_HELLO: u32 = 1;
const K_SETUP: u32 = 2;
const K_FRAME: u32 = 3;
const K_RELEASE: u32 = 4;

impl Message {
    pub fn encode(&self) -> [u8; MSG_SIZE] {
        let (kind, a, u): (u32, [u32; 6], [u64; 2]) = match *self {
            Message::Hello { version, pid } => (K_HELLO, [version, pid, 0, 0, 0, 0], [0, 0]),
            Message::Setup(r) => (
                K_SETUP,
                [r.gen, r.width, r.height, r.stride, r.format as u32, r.slots],
                [r.slot_size, 0],
            ),
            Message::Frame { gen, slot, frame_nr, timestamp_ns } => {
                (K_FRAME, [gen, slot, 0, 0, 0, 0], [frame_nr, timestamp_ns])
            }
            Message::Release { gen, slot } => (K_RELEASE, [gen, slot, 0, 0, 0, 0], [0, 0]),
        };
        let mut b = [0u8; MSG_SIZE];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&kind.to_le_bytes());
        for (i, v) in a.iter().enumerate() {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[32..40].copy_from_slice(&u[0].to_le_bytes());
        b[40..48].copy_from_slice(&u[1].to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; MSG_SIZE]) -> Result<Message, DecodeError> {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        if u32_at(0) != MAGIC {
            return Err(DecodeError::Magic);
        }
        let a = |i: usize| u32_at(8 + i * 4);
        match u32_at(4) {
            K_HELLO => Ok(Message::Hello { version: a(0), pid: a(1) }),
            K_SETUP => Ok(Message::Setup(RingInfo {
                gen: a(0),
                width: a(1),
                height: a(2),
                stride: a(3),
                format: PixelFormat::from_u32(a(4)).ok_or(DecodeError::Field)?,
                slots: a(5),
                slot_size: u64_at(32),
            })),
            K_FRAME => Ok(Message::Frame { gen: a(0), slot: a(1), frame_nr: u64_at(32), timestamp_ns: u64_at(40) }),
            K_RELEASE => Ok(Message::Release { gen: a(0), slot: a(1) }),
            _ => Err(DecodeError::Kind),
        }
    }
}

/// Which ring slots are currently owned by the viewer (bit set = busy). Lock-free.
#[derive(Default)]
pub struct SlotMask(AtomicU32);

impl SlotMask {
    pub const fn new() -> Self {
        SlotMask(AtomicU32::new(0))
    }
    /// Reserves the lowest free slot `< slots`; None if all are busy.
    pub fn try_acquire(&self, slots: u32) -> Option<u32> {
        let mut cur = self.0.load(Ordering::Acquire);
        loop {
            let slot = (0..slots.min(32)).find(|s| cur & (1 << s) == 0)?;
            match self.0.compare_exchange_weak(cur, cur | (1 << slot), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(slot),
                Err(v) => cur = v,
            }
        }
    }
    pub fn release(&self, slot: u32) {
        if slot < 32 {
            self.0.fetch_and(!(1 << slot), Ordering::AcqRel);
        }
    }
    pub fn reset(&self) {
        self.0.store(0, Ordering::Release);
    }
}

/// Target size for the mirrored image: width capped at `max_w`, aspect ratio of the source rect kept.
pub fn scaled_size(src_w: u32, src_h: u32, max_w: u32) -> Option<(u32, u32)> {
    if src_w == 0 || src_h == 0 || max_w == 0 {
        return None;
    }
    let w = src_w.min(max_w);
    let h = ((w as u64 * src_h as u64 + src_w as u64 / 2) / src_w as u64).max(1) as u32;
    if h > MAX_DIM {
        return None;
    }
    Some((w, h))
}

/// Target size for a centered crop to `num:den` (e.g. 16:9) followed by a scale to width `max_w`
/// (never upscaled: the width is capped at the crop width). `num == 0` means "native" (no crop,
/// same as [`scaled_size`]). Returns (target_w, target_h, crop_w, crop_h).
pub fn cropped_size(src_w: u32, src_h: u32, max_w: u32, num: u32, den: u32) -> Option<(u32, u32, u32, u32)> {
    if src_w == 0 || src_h == 0 || max_w == 0 {
        return None;
    }
    if num == 0 || den == 0 {
        let (w, h) = scaled_size(src_w, src_h, max_w)?;
        return Some((w, h, src_w, src_h));
    }
    let (num, den) = (num as u64, den as u64);
    let (sw, sh) = (src_w as u64, src_h as u64);
    let (cw, ch) = if sw * den > sh * num { (((sh * num) / den).max(1), sh) } else { (sw, ((sw * den) / num).max(1)) };
    let tw = cw.min(max_w as u64);
    let th = ((tw * den + num / 2) / num).max(1);
    if th > MAX_DIM as u64 {
        return None;
    }
    Some((tw as u32, th as u32, cw as u32, ch as u32))
}

/// Largest rect with aspect `img_w:img_h` that fits (centered) into `win_w x win_h`: (x, y, w, h).
pub fn fit_rect(img_w: u32, img_h: u32, win_w: u32, win_h: u32) -> (u32, u32, u32, u32) {
    if img_w == 0 || img_h == 0 || win_w == 0 || win_h == 0 {
        return (0, 0, 0, 0);
    }
    // Compare img_w/img_h with win_w/win_h without floats.
    let (w, h) = if (img_w as u64) * (win_h as u64) >= (win_w as u64) * (img_h as u64) {
        (win_w, ((win_w as u64 * img_h as u64) / img_w as u64).max(1) as u32)
    } else {
        (((win_h as u64 * img_w as u64) / img_h as u64).max(1) as u32, win_h)
    };
    ((win_w - w.min(win_w)) / 2, (win_h - h.min(win_h)) / 2, w.min(win_w), h.min(win_h))
}

// ------------------------------------------------------------------ sockets

/// Directory shared between host and Proton/pressure-vessel containers.
/// `$XDG_RUNTIME_DIR` is NOT shared with the container (it gets a private
/// tmpfs), but `$HOME` is, so the socket and log live under `$HOME`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("wivrn-monitor");
    }
    match std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        Some(h) => PathBuf::from(h).join(".local/share/wivrn-monitor"),
        // SAFETY: getuid has no preconditions.
        None => PathBuf::from(format!("/tmp/wivrn-monitor-{}", unsafe { libc::getuid() })),
    }
}

/// Socket path; override with `WIVRN_MONITOR_SOCK`.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("WIVRN_MONITOR_SOCK").filter(|d| !d.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => data_dir().join("monitor.sock"),
    }
}

fn sockaddr(path: &std::path::Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain data; all-zero is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let b = path.as_os_str().as_bytes();
    if b.len() >= addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }
    for (i, c) in b.iter().enumerate() {
        addr.sun_path[i] = *c as libc::c_char;
    }
    Ok((addr, std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn new_socket(nonblock: bool) -> io::Result<OwnedFd> {
    let flags = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | if nonblock { libc::SOCK_NONBLOCK } else { 0 };
    // SAFETY: plain syscall; the fd is wrapped immediately.
    let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, flags, 0) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Connects (never blocks: a full backlog yields EAGAIN). The returned socket is blocking.
pub fn connect(path: &std::path::Path) -> io::Result<OwnedFd> {
    let fd = new_socket(true)?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: addr/len describe a valid sockaddr_un.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) })?;
    set_nonblocking(fd.as_raw_fd(), false)?;
    Ok(fd)
}

pub fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
    // SAFETY: fcntl on a valid fd.
    unsafe {
        let fl = cvt(libc::fcntl(fd, libc::F_GETFL))?;
        let fl = if on { fl | libc::O_NONBLOCK } else { fl & !libc::O_NONBLOCK };
        cvt(libc::fcntl(fd, libc::F_SETFL, fl))?;
    }
    Ok(())
}

/// Creates the listening socket (non-blocking). A stale socket file is replaced; if a live viewer
/// already owns it, `AddrInUse` is returned.
pub fn listen(path: &std::path::Path) -> io::Result<OwnedFd> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        match connect(path) {
            Ok(_) => return Err(io::Error::new(io::ErrorKind::AddrInUse, "another viewer is already running")),
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let fd = new_socket(true)?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: valid fd and sockaddr.
    cvt(unsafe { libc::bind(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) })?;
    cvt(unsafe { libc::listen(fd.as_raw_fd(), 8) })?;
    Ok(fd)
}

/// Non-blocking accept; `Ok(None)` if nothing is pending. The new socket is blocking.
pub fn accept(listener: RawFd) -> io::Result<Option<OwnedFd>> {
    // SAFETY: null addr pointers are allowed.
    let r = unsafe { libc::accept4(listener, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(e),
        };
    }
    let fd = unsafe { OwnedFd::from_raw_fd(r) };
    Ok(Some(fd))
}

/// Sends one message (optionally with an fd via SCM_RIGHTS). `dontwait`: fail with WouldBlock
/// instead of blocking when the peer is slow.
pub fn send_msg(fd: RawFd, msg: &Message, pass_fd: Option<RawFd>, dontwait: bool) -> io::Result<()> {
    let buf = msg.encode();
    let mut iov = libc::iovec { iov_base: buf.as_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: msghdr is plain data; pointers stay valid for the sendmsg call.
    unsafe {
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        if let Some(f) = pass_fd {
            hdr.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            hdr.msg_controllen = libc::CMSG_SPACE(4) as _;
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::copy_nonoverlapping(&f as *const i32 as *const u8, libc::CMSG_DATA(c), 4);
        }
        let flags = libc::MSG_NOSIGNAL | if dontwait { libc::MSG_DONTWAIT } else { 0 };
        loop {
            let n = libc::sendmsg(fd, &hdr, flags);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return if n as usize == MSG_SIZE { Ok(()) } else { Err(io::Error::from(io::ErrorKind::WriteZero)) };
        }
    }
}

/// Receives one message; `Ok(None)` on EOF. Blocks unless the socket is non-blocking.
pub fn recv_msg(fd: RawFd) -> io::Result<Option<(Message, Option<OwnedFd>)>> {
    let mut buf = [0u8; MSG_SIZE];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: as in send_msg; received fds are wrapped in OwnedFd right away (no leaks).
    unsafe {
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        let n = loop {
            let n = libc::recvmsg(fd, &mut hdr, libc::MSG_CMSG_CLOEXEC);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            break n as usize;
        };
        let mut got: Option<OwnedFd> = None;
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let cnt = bytes / 4;
                for i in 0..cnt {
                    let mut raw: i32 = -1;
                    std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c).add(i * 4), &mut raw as *mut i32 as *mut u8, 4);
                    let o = OwnedFd::from_raw_fd(raw);
                    if got.is_none() {
                        got = Some(o);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
        if n == 0 {
            return Ok(None);
        }
        if n != MSG_SIZE || hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated message"));
        }
        let m = Message::decode(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
        Ok(Some((m, got)))
    }
}

/// Waits until one of `fds` is readable (or hung up). Returns per-fd readiness.
pub fn poll_readable(fds: &[RawFd], timeout_ms: i32) -> io::Result<Vec<bool>> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    loop {
        // SAFETY: p is a valid pollfd array.
        let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        return Ok(p.iter().map(|x| x.revents != 0).collect());
    }
}

pub fn shutdown(fd: RawFd) {
    // SAFETY: shutdown on any fd value is harmless (errors ignored).
    unsafe {
        libc::shutdown(fd, libc::SHUT_RDWR);
    }
}

// ------------------------------------------------------------------ shared memory

/// Creates a sealed-size memfd of `size` bytes.
pub fn create_memfd(name: &str, size: u64) -> io::Result<OwnedFd> {
    let cname = std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid C string; fd wrapped immediately.
    let fd = cvt(unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) })?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) })?;
    Ok(fd)
}

pub fn fd_size(fd: RawFd) -> io::Result<u64> {
    // SAFETY: stat is plain data.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st.st_size as u64)
}

/// A shared, writable mapping of a memfd (unmapped on drop).
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory; callers coordinate access via the slot protocol.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn new(fd: RawFd, len: usize, writable: bool) -> io::Result<Mapping> {
        let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        // SAFETY: mapping a fd with a checked length; result checked against MAP_FAILED.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: p as *mut u8, len })
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// # Safety
    /// The peer may write concurrently; only use for slots the protocol hands to this side.
    pub unsafe fn slice(&self, r: std::ops::Range<usize>) -> &[u8] {
        assert!(r.end <= self.len);
        std::slice::from_raw_parts(self.ptr.add(r.start), r.end - r.start)
    }
    /// # Safety
    /// Only for slots owned by this side (acquired via [`SlotMask`]).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self, r: std::ops::Range<usize>) -> &mut [u8] {
        assert!(r.end <= self.len);
        std::slice::from_raw_parts_mut(self.ptr.add(r.start), r.end - r.start)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let ms = [
            Message::Hello { version: VERSION, pid: 42 },
            Message::Setup(RingInfo::new(7, 1280, 1342, PixelFormat::Rgba8Srgb, 3)),
            Message::Frame { gen: 7, slot: 2, frame_nr: u64::MAX - 3, timestamp_ns: 123456789012 },
            Message::Release { gen: 7, slot: 1 },
        ];
        for m in ms {
            assert_eq!(Message::decode(&m.encode()), Ok(m));
        }
        let mut b = Message::Hello { version: 1, pid: 1 }.encode();
        b[0] ^= 1;
        assert_eq!(Message::decode(&b), Err(DecodeError::Magic));
        let mut b = Message::Hello { version: 1, pid: 1 }.encode();
        b[4] = 99;
        assert_eq!(Message::decode(&b), Err(DecodeError::Kind));
    }

    #[test]
    fn ring_validate() {
        let ok = RingInfo::new(1, 1280, 1342, PixelFormat::Rgba8Srgb, 3);
        assert!(ok.validate().is_ok());
        assert_eq!(ok.slot_range(1), Some(ok.slot_size as usize..2 * ok.slot_size as usize));
        assert_eq!(ok.slot_range(3), None);
        assert!(RingInfo { slots: 9, ..ok }.validate().is_err());
        assert!(RingInfo { width: 0, ..ok }.validate().is_err());
        assert!(RingInfo { slot_size: 5, ..ok }.validate().is_err());
        assert!(RingInfo { width: 8192, stride: 8192 * 4, height: 8192, slot_size: 8192 * 4 * 8192, ..ok }.validate().is_err());
    }

    #[test]
    fn slot_mask() {
        let m = SlotMask::new();
        assert_eq!(m.try_acquire(3), Some(0));
        assert_eq!(m.try_acquire(3), Some(1));
        assert_eq!(m.try_acquire(3), Some(2));
        assert_eq!(m.try_acquire(3), None);
        m.release(1);
        assert_eq!(m.try_acquire(3), Some(1));
        m.reset();
        assert_eq!(m.try_acquire(2), Some(0));
    }

    #[test]
    fn scaling() {
        assert_eq!(scaled_size(3096, 3243, 1280), Some((1280, 1341)));
        assert_eq!(scaled_size(640, 480, 1280), Some((640, 480)));
        assert_eq!(scaled_size(0, 10, 1280), None);
        assert_eq!(scaled_size(10000, 1, 1280), Some((1280, 1)));
    }

    #[test]
    fn cropping() {
        assert_eq!(cropped_size(3096, 3243, 1280, 16, 9), Some((1280, 720, 3096, 1741)));
        assert_eq!(cropped_size(3096, 3243, 1280, 0, 0), Some((1280, 1341, 3096, 3243)));
        assert_eq!(cropped_size(3000, 1000, 1280, 16, 9), Some((1280, 720, 1777, 1000)));
        assert_eq!(cropped_size(0, 5, 1280, 16, 9), None);
    }

    #[test]
    fn fit() {
        assert_eq!(fit_rect(100, 100, 200, 100), (50, 0, 100, 100));
        assert_eq!(fit_rect(100, 100, 100, 200), (0, 50, 100, 100));
        assert_eq!(fit_rect(200, 100, 200, 100), (0, 0, 200, 100));
        assert_eq!(fit_rect(0, 1, 5, 5), (0, 0, 0, 0));
    }

    #[test]
    fn socket_and_fd_passing() {
        let dir = std::env::temp_dir().join(format!("wvm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sock");
        let l = listen(&path).unwrap();
        let c = connect(&path).unwrap();
        let s = loop {
            if let Some(s) = accept(l.as_raw_fd()).unwrap() {
                break s;
            }
        };
        assert!(listen(&path).is_err(), "live socket must not be replaced");
        let mfd = create_memfd("t", 4096).unwrap();
        let w = Mapping::new(mfd.as_raw_fd(), 4096, true).unwrap();
        unsafe { w.slice_mut(0..4)[..].copy_from_slice(&[1, 2, 3, 4]) };
        let info = RingInfo::new(1, 1, 1, PixelFormat::Rgba8Unorm, 1);
        send_msg(c.as_raw_fd(), &Message::Setup(info), Some(mfd.as_raw_fd()), true).unwrap();
        let (m, fd) = recv_msg(s.as_raw_fd()).unwrap().unwrap();
        assert_eq!(m, Message::Setup(info));
        let fd = fd.unwrap();
        assert_eq!(fd_size(fd.as_raw_fd()).unwrap(), 4096);
        let r = Mapping::new(fd.as_raw_fd(), 4096, false).unwrap();
        assert_eq!(unsafe { r.slice(0..4) }, &[1, 2, 3, 4]);
        drop(c);
        assert!(recv_msg(s.as_raw_fd()).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
