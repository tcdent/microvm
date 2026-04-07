// Lightweight epoll wrapper for polling multiple event sources
// between vCPU exits. Not a full event loop — just a non-blocking
// check of registered fds after each vcpu.run() cycle.

use std::os::fd::RawFd;

const MAX_EVENTS: usize = 16;

pub struct EventPoll {
    epoll_fd: RawFd,
}

impl EventPoll {
    pub fn new() -> Self {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        assert!(fd >= 0, "Failed to create epoll fd");
        Self { epoll_fd: fd }
    }

    /// Register an fd with a token. The token is returned in poll results.
    pub fn add(&self, fd: RawFd, token: u64) {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: token,
        };
        let ret = unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event)
        };
        assert!(ret == 0, "epoll_ctl ADD failed");
    }

    /// Non-blocking poll. Returns tokens of fds that are ready.
    pub fn poll(&self) -> Vec<u64> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        let n = unsafe {
            libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), MAX_EVENTS as i32, 0)
        };
        if n <= 0 {
            return Vec::new();
        }
        events[..n as usize].iter().map(|e| e.u64).collect()
    }
}

impl Drop for EventPoll {
    fn drop(&mut self) {
        unsafe { libc::close(self.epoll_fd); }
    }
}
