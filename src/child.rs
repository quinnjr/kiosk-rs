//! Child process lifecycle.
//!
//! The compositor's lifetime is the child's: when the child exits, the
//! compositor exits with its status.
//!
//! Exit is detected with `pidfd_open(2)` rather than a `SIGCHLD` handler. A
//! pidfd is pollable, so child death becomes an ordinary event loop source with
//! no async-signal-safety constraints and no race between a signal handler and
//! the loop. It also cannot be confused by an unrelated process exiting.

use std::io::IsTerminal;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::Timespec;
use rustix::process::{Pid, PidfdFlags, Signal};

/// How long `terminate` waits for a signalled child before giving up on it.
///
/// Bounded rather than unbounded: a child that ignores `SIGTERM` must not hang
/// shutdown, but the spec's "the child is sent SIGTERM and reaped" is only
/// honoured if we actually wait for the common case.
const TERMINATE_GRACE: Duration = Duration::from_millis(2000);

/// Exit status used when the child cannot be reaped at all.
///
/// 125 is the convention `env` and `timeout` use for "the tool itself failed". It
/// cannot be distinct from *every* status a child can produce — a child may exit 125
/// too — but it is distinct from the likely ones (0, 1, and 128+n), which is enough
/// for a script to tell this apart from an ordinary failure.
const REAP_FAILURE_STATUS: i32 = 125;

/// What [`ChildProcess::terminate`] did.
///
/// Returned rather than inferred, so a test can assert that the reaped-child guard
/// actually suppressed the signal — the previous test called `terminate()` and
/// asserted nothing, and passed with the guard deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminate {
    /// The child had already been waited for; no signal was sent, because the pid
    /// may now belong to an unrelated process.
    AlreadyReaped,
    /// `SIGTERM` was sent (or the child was already gone).
    Signalled,
    /// Nothing was sent: either the signal failed (e.g. `EPERM`) or the recorded pid
    /// was not valid. Distinct from [`Self::Signalled`] because the child may still
    /// be running with nothing on the way to stop it. The two causes were separate
    /// variants, but no caller or test ever distinguished them — the differing
    /// `tracing` messages carry that detail.
    Failed,
}

/// A spawned child and the pidfd that reports its exit.
#[derive(Debug)]
pub struct ChildProcess {
    child: Child,
    /// Becomes readable exactly once, when the child exits.
    pidfd: OwnedFd,
    /// True once the child has been waited for. Guards against signalling a pid
    /// the kernel may already have recycled.
    reaped: bool,
}

impl ChildProcess {
    /// Spawn `command` with `WAYLAND_DISPLAY` pointing at our socket.
    ///
    /// # Standard streams
    ///
    /// A kiosk application's own logging should not be swallowed, so stdout and
    /// stderr are inherited — *unless* they are a terminal. The compositor is
    /// launched from a TTY, and a console fd is a VT-switching capability: a
    /// compromised client can call `ioctl(fd, VT_ACTIVATE, n)` on any of them and
    /// drop the physical user at a login prompt, defeating the confinement that is
    /// the point of a kiosk. Nulling stdin alone does not help, because `setsid`
    /// removes the *controlling terminal* but leaves already-inherited descriptors
    /// perfectly usable.
    ///
    /// So: a console stream is replaced with `/dev/null` (it is invisible anyway
    /// once the TTY is in graphics mode — see the spec's logging section), while a
    /// stream the operator redirected to a file or pipe is passed through
    /// untouched. That keeps real logging setups working and closes the escape.
    /// stdin is always `/dev/null`; a kiosk application has no console to read.
    ///
    /// # Inherited descriptors above stderr
    ///
    /// Not closed here, and not guaranteed closed. `LibSeatSession::open` discards
    /// the `O_CLOEXEC` this crate passes it, so whether the DRM master fd, the evdev
    /// fds, and the seatd socket survive `exec` is decided by the system libseat —
    /// which does set close-on-exec in its own backends, but is not something this
    /// code verifies. A `close_range(3, ..)` in `pre_exec` would make the guarantee
    /// ours, and `libc` is now a direct dependency, so availability is no longer the
    /// obstacle. The obstacle is that `std`'s fork+exec path reports `pre_exec` and
    /// `exec` failures to the parent over a `CLOEXEC` `SOCK_SEQPACKET` socketpair
    /// whose descriptor it does not publish (on Linux the same fd also carries the
    /// pidfd). Closing the range blind would close it, and the parent reads EOF as
    /// "exec succeeded" — so a failed spawn would be reported as a *successful* one,
    /// and the child would then die on the closed socket: `std` calls
    /// `always_abort()` before running our closure, so the write failure becomes
    /// `SIGABRT` and surfaces as exit `134` instead of the real `exec` errno. Doing
    /// this safely needs the range to skip that fd, which means not using `Command`.
    /// Worth revisiting, but not a one-line change.
    pub fn spawn(command: &[String], wayland_display: &str) -> Result<Self> {
        Self::spawn_with(
            command,
            wayland_display,
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        )
    }

    /// [`Self::spawn`] with the console decision injected.
    ///
    /// The `is_terminal` checks are the one part of `spawn` a test cannot control —
    /// under `cargo test` both streams are pipes, so the console branch would never
    /// be exercised and a stubbed-out decision would pass.
    fn spawn_with(
        command: &[String],
        wayland_display: &str,
        stdout_is_console: bool,
        stderr_is_console: bool,
    ) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("internal error: empty command")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stream_plan(stdout_is_console)))
            .stderr(Stdio::from(stream_plan(stderr_is_console)))
            .env("WAYLAND_DISPLAY", wayland_display)
            // A Wayland client must not fall back to an inherited X11 display.
            .env_remove("DISPLAY")
            // The compositor may run as root; a kiosk application should not
            // inherit loader overrides from whatever launched us.
            .env_remove("LD_PRELOAD")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_AUDIT");

        // SAFETY: `setsid` and `pthread_sigmask` are both async-signal-safe and
        // touch only the calling (already-forked) process, which is what `pre_exec`
        // requires. Nothing is logged from here — allocation and locking are not
        // permitted between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // `EPERM` means the child is already a session leader, which is
                // the outcome we wanted. Any other failure leaves it sharing our
                // session; a console stdout/stderr has already been replaced with
                // `/dev/null` above, so this is a defence-in-depth measure rather
                // than the load-bearing one.
                let _ = rustix::process::setsid();

                // Undo the inherited signal mask. calloop's signalfd source blocks
                // SIGTERM/SIGINT in our thread, and std deliberately does *not*
                // reset the mask before `exec` ("Inherit the signal mask from the
                // parent rather than resetting it") — so without this the child
                // would run with those signals blocked, `terminate()` below could
                // never be delivered, every shutdown would burn the full
                // `TERMINATE_GRACE`, and the application would survive as an orphan
                // on a VT nobody composites. Blocked signals also propagate to
                // everything the client itself spawns.
                let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
                if libc::sigemptyset(empty.as_mut_ptr()) == 0 {
                    // Failure here is not recoverable from a `pre_exec` and not
                    // worth aborting the spawn over: the child still runs, it just
                    // cannot be stopped politely. `terminate` bounds that at
                    // `TERMINATE_GRACE` and warns rather than hanging.
                    let _ = libc::pthread_sigmask(
                        libc::SIG_SETMASK,
                        empty.as_ptr(),
                        std::ptr::null_mut(),
                    );
                }
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {program:?}"))?;

        let pid = Pid::from_raw(child.id() as i32)
            .context("internal error: child reported an invalid pid")?;
        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::NONBLOCK)
            .context("failed to open pidfd for child")?;

        tracing::info!(pid = child.id(), program = %program, "child spawned");

        Ok(Self {
            child,
            pidfd,
            reaped: false,
        })
    }

    /// Reap the child and map its status to a process exit code.
    ///
    /// Called once the pidfd signals readability, so the child has already
    /// exited and this does not block.
    ///
    /// The status mapping is [`exit_code_of`]; an unreapable child yields
    /// [`REAP_FAILURE_STATUS`].
    pub fn reap(&mut self) -> i32 {
        self.reaped = true;
        match self.child.wait() {
            Ok(status) => exit_code_of(&status),
            Err(err) => {
                tracing::error!(?err, "failed to reap child");
                REAP_FAILURE_STATUS
            }
        }
    }

    /// The child's process id.
    #[cfg(test)]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Ask the child to exit, used when the compositor shuts down first (the
    /// `--exit-key` path).
    ///
    /// Waits up to [`TERMINATE_GRACE`] for the child to go, then gives up and
    /// leaves it to init. A child that ignores `SIGTERM` must not hang shutdown —
    /// the event loop has already stopped, so nothing is compositing the screen
    /// while we wait — but returning instantly would mean the child never gets to
    /// run its handler, and would make the "still running" warning below fire on
    /// every healthy shutdown.
    pub fn terminate(&mut self) -> Terminate {
        if self.reaped {
            // The pid may already have been recycled; signalling it would hit an
            // unrelated process.
            tracing::debug!("child already reaped; not signalling");
            return Terminate::AlreadyReaped;
        }

        let pid = self.child.id();
        let Some(signal_target) = Pid::from_raw(pid as i32) else {
            tracing::warn!(pid, "child has an invalid pid; not signalling");
            return Terminate::Failed;
        };

        match rustix::process::kill_process(signal_target, Signal::TERM) {
            Ok(()) => tracing::debug!(pid, "sent SIGTERM to child"),
            // The child is already gone; nothing to chase.
            Err(rustix::io::Errno::SRCH) => tracing::debug!(pid, "child already gone"),
            Err(err) => {
                tracing::warn!(pid, ?err, "failed to signal child");
                return Terminate::Failed;
            }
        }

        match self.wait_for_exit(TERMINATE_GRACE) {
            Ok(Some(status)) => tracing::debug!(pid, ?status, "child exited after SIGTERM"),
            Ok(None) => {
                tracing::warn!(
                    pid,
                    grace_ms = TERMINATE_GRACE.as_millis(),
                    "child ignored SIGTERM; leaving it to init"
                );
            }
            Err(err) => tracing::warn!(pid, ?err, "failed to check child status"),
        }

        Terminate::Signalled
    }

    /// Poll the pidfd until the child exits or `grace` elapses.
    ///
    /// Uses the pidfd rather than a sleep loop so the common case returns as soon
    /// as the child is gone.
    fn wait_for_exit(&mut self, grace: Duration) -> Result<Option<ExitStatus>> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.child.try_wait().context("failed to reap child")? {
                self.reaped = true;
                return Ok(Some(status));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            };
            // EINTR just means we go round again against the same deadline; any
            // other error would otherwise spin the loop for the whole grace period
            // and hide its own cause behind the "child ignored SIGTERM" warning.
            match poll(&mut fds, Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(err) => return Err(err).context("failed to poll the child pidfd"),
            }
        }
    }

    /// The pidfd, for registration as an event source.
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }
}

/// What to do with one of the child's standard streams.
///
/// A named value rather than a bare `Stdio`, because `Stdio` is opaque — no `Debug`,
/// no `PartialEq` — so a test could only assert `matches!(x, Stdio { .. })`, which is
/// true of every `Stdio` and passes with the decision inverted. That is exactly the
/// vacuous guard this type exists to make impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamPlan {
    /// Replace with `/dev/null`: a console fd is a VT-switching capability.
    Null,
    /// Pass through: the operator redirected this somewhere, so it is real logging.
    Inherit,
}

impl From<StreamPlan> for Stdio {
    fn from(plan: StreamPlan) -> Self {
        match plan {
            StreamPlan::Null => Stdio::null(),
            StreamPlan::Inherit => Stdio::inherit(),
        }
    }
}

/// `/dev/null` for a console stream, inherit for anything else.
fn stream_plan(is_console: bool) -> StreamPlan {
    if is_console {
        StreamPlan::Null
    } else {
        StreamPlan::Inherit
    }
}

/// Check that a command names something executable, before the compositor takes
/// over the screen.
///
/// Spec tier 1 requires "child binary missing" to fail before graphics mode, but
/// the real spawn necessarily happens after the display is up (the child needs a
/// socket to connect to). This closes the ordinary typo-in-a-unit-file case while
/// stderr is still visible on the console.
pub fn preflight(command: &[String]) -> Result<()> {
    let Some(program) = command.first() else {
        bail!("internal error: empty command");
    };

    // `access(X_OK)` rather than the mode bits: a file can be `0o700` and owned by
    // someone else, which passes a mode-bit test and then fails `EACCES` at spawn
    // — after the screen has been taken, which is what this check exists to avoid.
    let is_executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
            && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
    };

    if program.contains('/') {
        if !is_executable(Path::new(program)) {
            bail!("failed to spawn {program:?}: not an executable file");
        }
        return Ok(());
    }

    let found = search_path().iter().any(|dir| {
        let mut candidate = dir.clone();
        candidate.push(program);
        is_executable(&candidate)
    });

    if !found {
        bail!("failed to spawn {program:?}: not found in PATH");
    }
    Ok(())
}

/// The directories `execvp` would search.
///
/// An unset or empty `PATH` is not "no directories": `execvp` falls back to a
/// confstr default, conventionally `/bin:/usr/bin`. Treating it as empty would make
/// preflight reject a command the spawn would happily run.
fn search_path() -> Vec<PathBuf> {
    search_path_from(std::env::var_os("PATH"))
}

/// [`search_path`] over an explicit value, so the fallback rules are testable.
fn search_path_from(value: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    match value {
        Some(ref value) if !value.is_empty() => std::env::split_paths(value)
            // `split_paths` yields "" for an empty component, which `execvp`
            // interprets as the current directory.
            .map(|dir| {
                if dir.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    dir
                }
            })
            .collect(),
        _ => vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")],
    }
}

/// Map a child's exit status onto the code this process should exit with.
///
/// A normal exit propagates verbatim. Death by signal maps to `128 + signum`,
/// matching shell convention, so a script wrapping the compositor sees what it
/// would have seen wrapping the application directly.
fn exit_code_of(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        tracing::info!(code, "child exited");
        return code;
    }

    // On Unix a status with no code always carries a signal; the fallback keeps
    // this total rather than panicking on a case the platform says cannot happen.
    let signal = status.signal().unwrap_or(0);
    tracing::info!(signal, "child killed by signal");
    128 + signal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    /// Block until the pidfd reports the child has exited.
    ///
    /// Production code polls this fd through calloop; a test has no event loop,
    /// so it polls directly. This also proves the pidfd actually becomes readable
    /// on exit, which is the mechanism the whole shutdown path depends on.
    fn wait_for_pidfd(child: &ChildProcess) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pollfd = libc::pollfd {
            fd: child.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        while Instant::now() < deadline {
            // SAFETY: a valid pollfd array of length 1 with a live fd.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 100) };
            if ready > 0 && pollfd.revents & libc::POLLIN != 0 {
                return;
            }
        }
        panic!("pidfd never became readable; the child exit path is broken");
    }

    /// The child must not inherit our blocked signal mask.
    ///
    /// calloop's signalfd source blocks SIGTERM/SIGINT in the compositor thread, and
    /// `std` deliberately does *not* reset the mask before `exec`. Without the
    /// `pthread_sigmask` in `pre_exec` the client would run with SIGTERM blocked, so
    /// `terminate` could never stop it: every shutdown would burn the full grace and
    /// leave the application alive on a VT nobody composites.
    ///
    /// This blocks SIGTERM in the *test* thread first, reproducing the compositor's
    /// state at spawn time, so it fails if the reset is removed.
    #[test]
    fn a_child_does_not_inherit_our_blocked_signals() {
        // SAFETY: `sigemptyset`/`sigaddset`/`pthread_sigmask` on a local sigset.
        // Per-thread, so this does not disturb the rest of the suite.
        unsafe {
            let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            assert_eq!(libc::sigemptyset(set.as_mut_ptr()), 0);
            assert_eq!(libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM), 0);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut()),
                0
            );
        }

        // Absolute path via `sh`, matching the rest of this file rather than
        // relying on a PATH lookup.
        let mut child = spawn(&["/bin/sh", "-c", "exec sleep 30"]).expect("failed to spawn");
        assert_eq!(child.terminate(), Terminate::Signalled);

        // `terminate` waits up to TERMINATE_GRACE and reaps on success. If the mask
        // leaked, SIGTERM was never delivered and the child is still running.
        assert!(
            child.reaped,
            "child survived SIGTERM: it inherited our blocked signal mask"
        );
    }

    fn spawn(args: &[&str]) -> Result<ChildProcess> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        ChildProcess::spawn(&owned, "wayland-test-0")
    }

    #[test]
    fn spawning_a_missing_binary_fails_with_the_program_name() {
        let err = spawn(&["kiosk-rs-no-such-binary-xyz"]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("kiosk-rs-no-such-binary-xyz"),
            "error should name the program: {message}"
        );
    }

    #[test]
    fn spawning_an_empty_command_is_an_internal_error() {
        let empty: Vec<String> = Vec::new();
        assert!(ChildProcess::spawn(&empty, "wayland-test-0").is_err());
    }

    #[test]
    fn a_successful_child_propagates_zero() {
        let mut child = spawn(&["/bin/sh", "-c", "exit 0"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0);
    }

    #[test]
    fn a_failing_child_propagates_its_code_verbatim() {
        let mut child = spawn(&["/bin/sh", "-c", "exit 42"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 42);
    }

    #[test]
    fn the_highest_normal_exit_code_survives() {
        let mut child = spawn(&["/bin/sh", "-c", "exit 255"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 255);
    }

    #[test]
    fn a_child_killed_by_sigkill_reports_137() {
        let mut child = spawn(&["/bin/sh", "-c", "kill -9 $$"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 128 + 9);
    }

    #[test]
    fn a_child_killed_by_sigterm_reports_143() {
        let mut child = spawn(&["/bin/sh", "-c", "kill -15 $$"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 128 + 15);
    }

    #[test]
    fn a_child_that_segfaults_reports_139() {
        let mut child = spawn(&["/bin/sh", "-c", "kill -11 $$"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 128 + 11);
    }

    #[test]
    fn the_child_receives_our_wayland_display() {
        // Proves the child is told which socket to connect to, which is the whole
        // point of spawning it ourselves.
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test \"$WAYLAND_DISPLAY\" = wayland-test-7".to_string(),
        ];
        let mut child = ChildProcess::spawn(&owned, "wayland-test-7").unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0, "WAYLAND_DISPLAY was not set correctly");
    }

    #[test]
    fn an_inherited_x11_display_is_removed() {
        // A Wayland client must not silently fall back to X11.
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test -z \"${DISPLAY+set}\"".to_string(),
        ];
        let mut child = ChildProcess::spawn(&owned, "wayland-test-0").unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0, "DISPLAY leaked into the child");
    }

    /// A sentinel path unique to this test process and name.
    fn sentinel(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kiosk-test-{}-{name}", std::process::id()))
    }

    /// Wait for the child to create `path`, proving it has reached a known point.
    ///
    /// Without this handshake, `terminate()` can signal `sh` before it has run its
    /// `trap` builtin, so the child dies and the test asserts the wrong thing —
    /// an intermittent failure on a loaded runner.
    fn await_sentinel(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("child never reached its sentinel at {}", path.display());
    }

    #[test]
    fn terminate_kills_a_child_that_honours_sigterm() {
        let mut child = spawn(&["/bin/sh", "-c", "sleep 300"]).unwrap();

        assert_eq!(child.terminate(), Terminate::Signalled);

        // The real assertion: the child is gone, and gone *because* of SIGTERM.
        let status = child
            .child
            .try_wait()
            .expect("try_wait failed")
            .expect("child survived SIGTERM");
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[test]
    fn terminate_does_not_block_on_a_child_that_ignores_sigterm() {
        let flag = sentinel("ignores-sigterm");
        let _ = std::fs::remove_file(&flag);
        let script = format!("trap '' TERM; : > {}; sleep 30", flag.display());
        let mut child = spawn(&["/bin/sh", "-c", &script]).unwrap();

        // The trap is installed only once the sentinel exists.
        await_sentinel(&flag);

        let started = Instant::now();
        child.terminate();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= TERMINATE_GRACE,
            "terminate gave up after {elapsed:?}, before the grace period"
        );
        assert!(
            elapsed < TERMINATE_GRACE + Duration::from_secs(3),
            "terminate blocked for {elapsed:?}, well past the grace period"
        );
        // Still alive, which is the documented outcome: left to init.
        assert!(matches!(child.child.try_wait(), Ok(None)));

        // Do not leave it running for the rest of the session.
        let pid = Pid::from_raw(child.id() as i32).unwrap();
        let _ = rustix::process::kill_process(pid, Signal::KILL);
        let _ = child.child.wait();
        let _ = std::fs::remove_file(&flag);
    }

    /// The guard that keeps `terminate` from signalling a recycled pid.
    #[test]
    fn terminate_after_reap_sends_no_signal() {
        let mut child = spawn(&["/bin/sh", "-c", "exit 0"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0);
        assert!(child.reaped, "reap must record that it waited");

        // Asserting the returned outcome is what makes this fail if the guard is
        // deleted — the previous version just called `terminate()` and asserted
        // nothing, and passed either way.
        assert_eq!(child.terminate(), Terminate::AlreadyReaped);
    }

    #[test]
    fn reaping_twice_is_idempotent_and_does_not_hang() {
        let mut child = spawn(&["/bin/sh", "-c", "exit 3"]).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 3);
        // `Child::wait` caches the status, so a second reap returns the same code
        // rather than erroring or blocking on a pid that no longer exists. The
        // shutdown path only reaps once, but this guarantees a double reap could
        // never silently turn a real exit code into a fabricated failure.
        assert_eq!(child.reap(), 3);
    }

    /// The decision itself, as a value. Inverting `stream_plan` fails this.
    #[test]
    fn a_console_stream_is_planned_null_and_a_redirected_one_inherited() {
        assert_eq!(stream_plan(true), StreamPlan::Null);
        assert_eq!(stream_plan(false), StreamPlan::Inherit);
    }

    /// The behaviour, with the console decision forced.
    ///
    /// Under `cargo test` both streams are pipes, so `spawn` would take the
    /// inherit branch and this could not distinguish the fix from its absence —
    /// which is why `spawn_with` exists.
    #[test]
    fn a_forced_console_child_gets_dev_null_not_the_console() {
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"exec 3>&1; test "$(readlink /proc/self/fd/3)" = /dev/null"#.to_string(),
        ];
        let mut child = ChildProcess::spawn_with(&owned, "wayland-test-0", true, true).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(
            child.reap(),
            0,
            "a console stdout was not replaced with /dev/null"
        );
    }

    /// The other half: a redirected stream must be passed through untouched, or the
    /// application's own logging is silently swallowed.
    #[test]
    fn a_redirected_child_stream_is_passed_through() {
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"exec 3>&1; test "$(readlink /proc/self/fd/3)" != /dev/null"#.to_string(),
        ];
        let mut child = ChildProcess::spawn_with(&owned, "wayland-test-0", false, false).unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0, "a redirected stdout was replaced");
    }

    #[test]
    fn the_child_gets_its_own_session() {
        // setsid means the child is its own session leader, so it has no
        // controlling terminal to reopen via /dev/tty.
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test \"$(ps -o sid= -p $$ | tr -d ' ')\" = \"$$\"".to_string(),
        ];
        let mut child = ChildProcess::spawn(&owned, "wayland-test-0").unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0, "child is not a session leader");
    }

    #[test]
    fn loader_overrides_are_not_inherited() {
        let owned = [
            "/bin/sh".to_string(),
            "-c".to_string(),
            "test -z \"${LD_PRELOAD+set}${LD_LIBRARY_PATH+set}${LD_AUDIT+set}\"".to_string(),
        ];
        let mut child = ChildProcess::spawn(&owned, "wayland-test-0").unwrap();
        wait_for_pidfd(&child);
        assert_eq!(child.reap(), 0, "a loader override leaked into the child");
    }

    /// Drives the seam directly — the previous version called `search_path()`, which
    /// reads the ambient PATH, so it passed with the fallback replaced by `vec![]`.
    /// The rationale for the fallback itself is on [`search_path`].
    #[test]
    fn an_unset_path_falls_back_to_the_execvp_default() {
        assert_eq!(
            search_path_from(None),
            vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn an_empty_path_falls_back_to_the_execvp_default_too() {
        assert_eq!(
            search_path_from(Some(std::ffi::OsString::new())),
            vec![PathBuf::from("/bin"), PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn search_path_reads_the_environment() {
        // Pins the wiring without mutating the environment.
        let expected: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set under cargo test"))
                .map(|d| {
                    if d.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        d
                    }
                })
                .collect();
        assert_eq!(search_path(), expected);
    }

    #[test]
    fn an_empty_path_component_means_the_current_directory() {
        assert_eq!(
            search_path_from(Some(std::ffi::OsString::from("/a::/b"))),
            vec![PathBuf::from("/a"), PathBuf::from("."), PathBuf::from("/b")]
        );
    }

    #[test]
    fn preflight_accepts_an_executable_absolute_path() {
        preflight(&["/bin/sh".to_string()]).unwrap();
    }

    #[test]
    fn preflight_accepts_a_bare_name_on_path() {
        preflight(&["sh".to_string()]).unwrap();
    }

    #[test]
    fn preflight_rejects_a_missing_absolute_path() {
        let err = preflight(&["/nonexistent/kiosk-xyz".to_string()]).unwrap_err();
        assert!(format!("{err:#}").contains("kiosk-xyz"));
    }

    #[test]
    fn preflight_rejects_a_name_not_on_path() {
        let err = preflight(&["kiosk-rs-no-such-binary-xyz".to_string()]).unwrap_err();
        assert!(format!("{err:#}").contains("not found in PATH"));
    }

    #[test]
    fn preflight_rejects_a_non_executable_file() {
        // A directory is a file-system entry that is not an executable file.
        let err = preflight(&["/etc/hostname".to_string()]).unwrap_err();
        assert!(format!("{err:#}").contains("not an executable file"));
    }

    #[test]
    fn preflight_rejects_an_empty_command() {
        assert!(preflight(&[]).is_err());
    }

    #[test]
    fn exit_code_mapping_covers_both_status_kinds() {
        // Built from raw wait statuses so every branch of exit_code_of is hit
        // without needing a process per case.
        // Low byte 0 with high byte n => normal exit with code n.
        assert_eq!(exit_code_of(&ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(42 << 8)), 42);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(255 << 8)), 255);
        // Low 7 bits = terminating signal, no exit code.
        assert_eq!(exit_code_of(&ExitStatus::from_raw(9)), 137);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(15)), 143);
        assert_eq!(exit_code_of(&ExitStatus::from_raw(11)), 139);
    }

    #[test]
    fn the_reap_failure_status_cannot_be_confused_with_a_child_status() {
        // A child can produce 0-255 normally and 128+n on a signal; 125 must not
        // collide with a plausible real status the way 1 did.
        assert_eq!(REAP_FAILURE_STATUS, 125);
        assert_ne!(REAP_FAILURE_STATUS, 1);
    }
}
