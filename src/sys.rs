//! Thin wrappers around the few libc calls std does not expose:
//! fd passing (SCM_RIGHTS), memfd/mmap for shared-memory buffers, pipes and poll.
//! These symbols come from the libc that std already links; no extra crates.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_void};

#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
struct MsgHdr {
    name: *mut c_void,
    namelen: u32,
    iov: *mut IoVec,
    iovlen: usize,
    control: *mut c_void,
    controllen: usize,
    flags: c_int,
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

extern "C" {
    fn sendmsg(fd: c_int, msg: *const MsgHdr, flags: c_int) -> isize;
    fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
    fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    #[link_name = "poll"]
    fn c_poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
}

const SOL_SOCKET: c_int = 1;
const SCM_RIGHTS: c_int = 1;
const MSG_NOSIGNAL: c_int = 0x4000;
const MSG_CMSG_CLOEXEC: c_int = 0x4000_0000;
const O_NONBLOCK: c_int = 0o4000;
const O_CLOEXEC: c_int = 0o2000000;
const MFD_CLOEXEC: c_uint = 1;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;

pub const POLLIN: i16 = 0x1;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

const HDR: usize = std::mem::size_of::<usize>() + 2 * std::mem::size_of::<c_int>();

fn align(n: usize) -> usize {
    let a = std::mem::size_of::<usize>();
    (n + a - 1) & !(a - 1)
}

fn cmsg_space(n: usize) -> usize {
    align(HDR) + align(n)
}

/// Sends `data` on a Unix socket, attaching `fds` as SCM_RIGHTS ancillary data.
/// Returns the number of bytes written (may be short; the fds go with the first byte).
pub fn send_with_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = IoVec { base: data.as_ptr() as *mut c_void, len: data.len() };
    let payload = fds.len() * std::mem::size_of::<c_int>();
    let mut control = vec![0u8; if fds.is_empty() { 0 } else { cmsg_space(payload) }];
    if !fds.is_empty() {
        let len = align(HDR) + payload;
        control[..std::mem::size_of::<usize>()].copy_from_slice(&len.to_ne_bytes());
        let o = std::mem::size_of::<usize>();
        control[o..o + 4].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
        control[o + 4..o + 8].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        for (i, fd) in fds.iter().enumerate() {
            let p = align(HDR) + i * 4;
            control[p..p + 4].copy_from_slice(&fd.to_ne_bytes());
        }
    }
    let msg = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        control: if control.is_empty() { std::ptr::null_mut() } else { control.as_mut_ptr() as *mut c_void },
        controllen: control.len(),
        flags: 0,
    };
    loop {
        let r = unsafe { sendmsg(sock, &msg, MSG_NOSIGNAL) };
        if r >= 0 {
            return Ok(r as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Writes all of `data`, passing `fds` with the first chunk.
pub fn send_all_with_fds(sock: RawFd, mut data: &[u8], mut fds: &[RawFd]) -> io::Result<()> {
    while !data.is_empty() {
        let n = send_with_fds(sock, data, fds)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed"));
        }
        data = &data[n..];
        fds = &[];
    }
    Ok(())
}

/// Receives bytes from a Unix socket, collecting any SCM_RIGHTS fds into `fds_out`.
pub fn recv_with_fds(sock: RawFd, buf: &mut [u8], fds_out: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut iov = IoVec { base: buf.as_mut_ptr() as *mut c_void, len: buf.len() };
    let mut control = vec![0u8; cmsg_space(28 * 4)];
    let mut msg = MsgHdr {
        name: std::ptr::null_mut(),
        namelen: 0,
        iov: &mut iov,
        iovlen: 1,
        control: control.as_mut_ptr() as *mut c_void,
        controllen: control.len(),
        flags: 0,
    };
    let n = loop {
        let r = unsafe { recvmsg(sock, &mut msg, MSG_CMSG_CLOEXEC) };
        if r >= 0 {
            break r as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let clen = msg.controllen.min(control.len());
    let us = std::mem::size_of::<usize>();
    let mut off = 0;
    while off + HDR <= clen {
        let len = usize::from_ne_bytes(control[off..off + us].try_into().unwrap());
        let level = c_int::from_ne_bytes(control[off + us..off + us + 4].try_into().unwrap());
        let ty = c_int::from_ne_bytes(control[off + us + 4..off + us + 8].try_into().unwrap());
        if len < HDR || off + len > clen {
            break;
        }
        if level == SOL_SOCKET && ty == SCM_RIGHTS {
            let mut p = off + align(HDR);
            while p + 4 <= off + len {
                let fd = c_int::from_ne_bytes(control[p..p + 4].try_into().unwrap());
                fds_out.push(unsafe { OwnedFd::from_raw_fd(fd) });
                p += 4;
            }
        }
        off += align(len);
    }
    Ok(n)
}

/// Creates a pipe; returns (read end, write end). Both ends are close-on-exec.
pub fn pipe(nonblocking: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as c_int; 2];
    let flags = O_CLOEXEC | if nonblocking { O_NONBLOCK } else { 0 };
    if unsafe { pipe2(fds.as_mut_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// poll(2); returns the number of ready fds (0 on timeout). EINTR counts as 0.
pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    let r = unsafe { c_poll(fds.as_mut_ptr(), fds.len() as u64, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(r as usize)
}

/// A shared-memory file mapped read/write into our address space (for wl_shm).
pub struct ShmMap {
    pub fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for ShmMap {}

impl ShmMap {
    pub fn new(len: usize) -> io::Result<ShmMap> {
        let name = CString::new("screenmirror").unwrap();
        let fd = unsafe { memfd_create(name.as_ptr(), MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let file = std::fs::File::from(fd.try_clone()?);
        file.set_len(len as u64)?;
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(ShmMap { fd, ptr: ptr as *mut u8, len })
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr as *mut c_void, self.len);
        }
    }
}
