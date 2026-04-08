use std::ffi::CString;
use fuse_backend_rs::api::filesystem::Context;

/// Default context simulating root (uid=0, gid=0, pid=1).
pub fn root_ctx() -> Context {
    Context { uid: 0, gid: 0, pid: 1 }
}

/// Context for a specific user.
#[allow(dead_code)]
pub fn user_ctx(uid: u32, gid: u32) -> Context {
    Context { uid, gid, pid: 1 }
}

/// Construct a CString for passing to lookup/create/mkdir/unlink.
pub fn cname(s: &str) -> CString {
    CString::new(s).expect("CString::new failed")
}
