//! A program the app starts gets only stdio of the app's descriptors. Android opens many of the
//! app's files, sockets and GPU buffers without close-on-exec, and a program would hold on to
//! them and pass them on to the programs it starts, so [`close_app_fds_on_exec`] marks them in
//! the child, between fork and exec.

use libc::c_int;

/// Up to this many slots in the descriptor table, each number is tried; above, the descriptors are
/// listed instead, since a table that large more likely comes from a few high numbers than from
/// that many descriptors.
const TRY_EACH_UP_TO: c_int = 4096;

/// For a child between fork and exec: mark every descriptor but its stdio close-on-exec. Only
/// system calls, since the app's other threads may hold locks the child would wait for.
pub fn close_app_fds_on_exec() {
    // Every open descriptor is below the table's size, so trying each number up to it marks them
    // all, as a number not in use just fails. Listing /proc/self/fd costs several times as much
    // per descriptor in a new process, where the kernel makes an entry for each.
    let size = table_size();
    match size {
        Some(size) if size <= TRY_EACH_UP_TO => mark_below(size),
        _ => {
            if !mark_listed() {
                mark_below(size.unwrap_or_else(descriptor_limit));
            }
        }
    }
}

/// Marks the descriptors from 3 to `end` - 1 close-on-exec.
fn mark_below(end: c_int) {
    for fd in 3..end {
        // SAFETY: sets the flags of this process's descriptor `fd`, or fails if there's none.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// The number of slots in this process's descriptor table, from /proc/self/status.
fn table_size() -> Option<c_int> {
    // "FDSize:" comes within the first few hundred bytes.
    let mut status = [0u8; 1024];
    // SAFETY: opens, reads at most `status.len()` bytes into `status`, and closes.
    let read = unsafe {
        let file = libc::open(
            c"/proc/self/status".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if file < 0 {
            return None;
        }
        let read = libc::read(file, status.as_mut_ptr().cast(), status.len());
        libc::close(file);
        read
    };
    table_size_in(status.get(..usize::try_from(read).ok()?)?)
}

/// `FDSize` from the text of a /proc/<pid>/status, if it's there in full.
fn table_size_in(status: &[u8]) -> Option<c_int> {
    const KEY: &[u8] = b"\nFDSize:";
    let start = status.windows(KEY.len()).position(|window| window == KEY)? + KEY.len();
    let value = &status[start..];
    let is_blank = |&&byte: &&u8| byte == b'\t' || byte == b' ';
    let value = &value[value.iter().take_while(is_blank).count()..];
    let end = value.iter().position(|byte| !byte.is_ascii_digit())?;
    if end == 0 || value[end] != b'\n' {
        return None;
    }
    value[..end].iter().try_fold(0, |size: c_int, &digit| {
        size.checked_mul(10)?.checked_add(c_int::from(digit - b'0'))
    })
}

/// Marks the descriptors /proc/self/fd lists, but stdio, close-on-exec; false if it can't be read
/// to the end.
fn mark_listed() -> bool {
    // SAFETY: system calls on this process's descriptors, and reads within `buffer`, at the
    // offsets the kernel gives.
    unsafe {
        let directory = libc::open(
            c"/proc/self/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if directory < 0 {
            return false;
        }
        // Records of linux_dirent64: d_ino (8 bytes), d_off (8), d_reclen (2), d_type (1), d_name.
        let mut buffer = [0u64; 512];
        let complete = loop {
            let read = libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                std::mem::size_of_val(&buffer),
            );
            if read <= 0 {
                break read == 0;
            }
            let bytes = buffer.as_ptr().cast::<u8>();
            let mut offset = 0;
            while offset < read as usize {
                let record = bytes.add(offset);
                let name = record.add(19);
                // A descriptor's number; "." and ".." aren't any.
                let mut fd: c_int = 0;
                let mut digits = 0;
                while let digit @ b'0'..=b'9' = *name.add(digits) {
                    fd = fd * 10 + c_int::from(digit - b'0');
                    digits += 1;
                }
                if digits > 0 && *name.add(digits) == 0 && fd > 2 && fd != directory {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                offset += usize::from(record.add(16).cast::<u16>().read_unaligned());
            }
        };
        libc::close(directory);
        complete
    }
}

/// How many descriptors this process may have, at most 65536.
fn descriptor_limit() -> c_int {
    // SAFETY: getrlimit writes into `limit`.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur.min(1 << 16) as c_int
        } else {
            1024
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Descriptors without close-on-exec, with gaps between them and one far above the rest,
    /// closed again when this is dropped.
    struct Open(Vec<c_int>);

    impl Open {
        fn new() -> Self {
            // SAFETY: opens and closes descriptors of its own.
            unsafe {
                let all: Vec<c_int> = (0..30)
                    .map(|_| libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY))
                    .collect();
                assert!(all.iter().all(|&fd| fd >= 0));
                let mut kept = Vec::new();
                for (n, fd) in all.into_iter().enumerate() {
                    if n % 3 == 0 {
                        libc::close(fd);
                    } else {
                        kept.push(fd);
                    }
                }
                // The lowest free number from 900 on.
                let high = libc::fcntl(kept[0], libc::F_DUPFD, 900.min(descriptor_limit() - 1));
                assert!(high >= 0);
                kept.push(high);
                for &fd in &kept {
                    assert_eq!(libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
                }
                Self(kept)
            }
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            for &fd in &self.0 {
                // SAFETY: closes a descriptor that `Open::new` opened.
                unsafe { libc::close(fd) };
            }
        }
    }

    /// Forks a child that runs `mark`, then counts its descriptors from 3 up that are still open
    /// without close-on-exec (at most 253): 254 if `mark` failed, 255 if it changed stdio's flags.
    fn unmarked_after(mark: fn() -> bool) -> c_int {
        let limit = descriptor_limit();
        // SAFETY: the child makes only system calls before it exits.
        unsafe {
            let stdio = [0, 1, 2].map(|fd| libc::fcntl(fd, libc::F_GETFD));
            let child = libc::fork();
            assert!(child >= 0);
            if child == 0 {
                if !mark() {
                    libc::_exit(254);
                }
                if (0..3).any(|fd| libc::fcntl(fd, libc::F_GETFD) != stdio[fd as usize]) {
                    libc::_exit(255);
                }
                let unmarked = (3..limit)
                    .filter(|&fd| {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        flags >= 0 && flags & libc::FD_CLOEXEC == 0
                    })
                    .count();
                libc::_exit(unmarked.min(253) as c_int);
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFEXITED(status));
            libc::WEXITSTATUS(status)
        }
    }

    #[test]
    fn leaves_the_program_only_stdio() {
        let open = Open::new();
        assert!(unmarked_after(|| true) >= open.0.len() as c_int);
        let marked = || {
            close_app_fds_on_exec();
            true
        };
        assert_eq!(unmarked_after(marked), 0);
    }

    #[test]
    fn trying_each_number_and_listing_mark_the_same() {
        let _open = Open::new();
        assert_eq!(unmarked_after(|| table_size().map(mark_below).is_some()), 0);
        assert_eq!(unmarked_after(mark_listed), 0);
    }

    #[test]
    fn every_descriptor_is_below_the_table_size() {
        let open = Open::new();
        assert!(table_size().unwrap() > *open.0.iter().max().unwrap());
    }

    #[test]
    fn reads_the_table_size_only_when_complete() {
        let status = b"Name:\tcat\nUmask:\t0022\nFDSize:\t512\nGroups:\t\n";
        assert_eq!(table_size_in(status), Some(512));
        assert_eq!(table_size_in(b"Name:\tcat\nFDSize: 64\n"), Some(64));
        // Cut off, empty, not a number, too large, or missing.
        assert_eq!(table_size_in(b"Name:\tcat\nFDSize:\t51"), None);
        assert_eq!(table_size_in(b"Name:\tcat\nFDSize:\t\n"), None);
        assert_eq!(table_size_in(b"Name:\tcat\nFDSize:\t5x2\n"), None);
        assert_eq!(table_size_in(b"Name:\tcat\nFDSize:\t99999999999\n"), None);
        assert_eq!(table_size_in(b"Name:\tcat\nUmask:\t0022\n"), None);
    }
}
