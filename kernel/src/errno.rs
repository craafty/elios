//! Error numbers, as defined by the Linux system call ABI (which eliOS
//! implements). System calls return them negated.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
#[allow(dead_code, reason = "the full set, for system calls to come")]
#[allow(clippy::upper_case_acronyms, reason = "the names Linux and C use")]
pub enum Errno {
    EPERM = 1,
    ENOENT = 2,
    ESRCH = 3,
    EINTR = 4,
    EIO = 5,
    ENXIO = 6,
    E2BIG = 7,
    ENOEXEC = 8,
    EBADF = 9,
    ECHILD = 10,
    EAGAIN = 11,
    ENOMEM = 12,
    EACCES = 13,
    EFAULT = 14,
    EBUSY = 16,
    EEXIST = 17,
    EXDEV = 18,
    ENODEV = 19,
    ENOTDIR = 20,
    EISDIR = 21,
    EINVAL = 22,
    ENFILE = 23,
    EMFILE = 24,
    ENOTTY = 25,
    EFBIG = 27,
    ENOSPC = 28,
    ESPIPE = 29,
    EROFS = 30,
    EMLINK = 31,
    EPIPE = 32,
    ERANGE = 34,
    ENAMETOOLONG = 36,
    ENOSYS = 38,
    ENOTEMPTY = 39,
    ELOOP = 40,
}

pub type Result<T> = core::result::Result<T, Errno>;

/// The symbolic name of an error number, for tracing.
pub fn name(errno: i64) -> &'static str {
    const NAMES: [&str; 41] = [
        "0",
        "EPERM",
        "ENOENT",
        "ESRCH",
        "EINTR",
        "EIO",
        "ENXIO",
        "E2BIG",
        "ENOEXEC",
        "EBADF",
        "ECHILD",
        "EAGAIN",
        "ENOMEM",
        "EACCES",
        "EFAULT",
        "ENOTBLK",
        "EBUSY",
        "EEXIST",
        "EXDEV",
        "ENODEV",
        "ENOTDIR",
        "EISDIR",
        "EINVAL",
        "ENFILE",
        "EMFILE",
        "ENOTTY",
        "ETXTBSY",
        "EFBIG",
        "ENOSPC",
        "ESPIPE",
        "EROFS",
        "EMLINK",
        "EPIPE",
        "EDOM",
        "ERANGE",
        "EDEADLK",
        "ENAMETOOLONG",
        "ENOLCK",
        "ENOSYS",
        "ENOTEMPTY",
        "ELOOP",
    ];
    NAMES.get(errno as usize).copied().unwrap_or("E?")
}
