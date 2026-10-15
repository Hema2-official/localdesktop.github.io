//! How the app's code forks: the link sends every call to `fork` in this library, Rust's
//! `std::process::Command` included, to [`__wrap_fork`] (`-Wl,--wrap=fork`, see `build.rs`).
//!
//! libc's `fork` runs the handlers libraries registered with `pthread_atfork`. Qualcomm's
//! OpenGL ES driver (`libGLESv2_adreno.so`) registers one for the child, with none before the
//! fork, that closes its GPU library (`gsl_library_close`), which takes a lock the driver holds
//! while it brings the GPU up. A child forked while Android's RenderThread, or the app's own
//! compositor, initialises the driver waits for that lock forever, before it gets to run a line
//! of its own: the app then waits for the program it started, and never comes up.
//!
//! The app's children only exec, after nothing but system calls (Rust's `Command` and the
//! `pre_exec` closures here), so they need none of the handlers: the clone system call makes
//! them without. One difference to libc's `fork`: until it execs, the child's `getpid` gives the
//! parent's pid, which bionic caches.

/// `fork`, without the handlers libraries registered for it.
///
/// # Safety
/// As for `fork`, and the child must make only system calls until it execs or exits.
#[no_mangle]
pub unsafe extern "C" fn __wrap_fork() -> libc::pid_t {
    // SAFETY: a plain fork (a copy of this process with only the calling thread, signalling
    // SIGCHLD when it ends); the caller keeps to system calls in the child.
    unsafe {
        libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_long, 0, 0, 0, 0) as libc::pid_t
    }
}
