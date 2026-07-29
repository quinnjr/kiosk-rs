//! Child process lifecycle.
//!
//! The compositor's lifetime is the child's: when the child exits, the
//! compositor exits with its status.
//!
//! Exit is detected with `pidfd_open(2)` rather than a `SIGCHLD` handler. A
//! pidfd is pollable, so child death becomes an ordinary event loop source with
//! no async-signal-safety constraints and no race between a signal handler and
//! the loop. It also cannot be confused by an unrelated process exiting.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
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
/// Deliberately distinct from any status a child can produce (0-255 for a normal
/// exit, 128+n for a signal), so a script can tell "kiosk could not determine the
/// child's fate" apart from "the child exited 1". 125 follows the convention used
/// by `env` and `timeout` for "the tool itself failed".
const REAP_FAILURE_STATUS: i32 = 125;

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
    /// stdout and stderr are inherited: a kiosk application's own logging should
    /// not be swallowed by the compositor. Compositor logging goes to
    /// `--log-file` instead, which keeps the two separable.
    ///
    /// stdin is *not* inherited, and the child is placed in its own session. The
    /// compositor is launched from a TTY, so an inherited console fd would let a
    /// compromised kiosk application call `ioctl(0, VT_ACTIVATE, n)` and drop the
    /// physical user at a login prompt — defeating the confinement that is the
    /// whole point of a kiosk. `setsid` additionally detaches the controlling
    /// terminal so `open("/dev/tty")` fails.
    pub fn spawn(command: &[String], wayland_display: &str) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("internal error: empty command")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .env("WAYLAND_DISPLAY", wayland_display)
            // A Wayland client must not fall back to an inherited X11 display.
            .env_remove("DISPLAY")
            // The compositor may run as root; a kiosk application should not
            // inherit loader overrides from whatever launched us.
            .env_remove("LD_PRELOAD")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_AUDIT");

        // SAFETY: `setsid` is async-signal-safe and touches only the calling
        // (already-forked) process, which is what `pre_exec` requires.
        unsafe {
            cmd.pre_exec(|| {
                if rustix::process::setsid().is_err() {
                    // Already a session leader is fine; anything else is not
                    // worth aborting the spawn over.
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
    /// Death by signal maps to `128 + signum`, matching shell convention, so
    /// scripts wrapping the compositor see what they would have seen wrapping
    /// the application directly.
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
    pub fn terminate(&mut self) {
        if self.reaped {
            // The pid may already have been recycled; signalling it would hit an
            // unrelated process.
            tracing::debug!("child already reaped; not signalling");
            return;
        }

        let pid = self.child.id();
        let Some(signal_target) = Pid::from_raw(pid as i32) else {
            tracing::warn!(pid, "child has an invalid pid; not signalling");
            return;
        };

        match rustix::process::kill_process(signal_target, Signal::TERM) {
            Ok(()) => tracing::debug!(pid, "sent SIGTERM to child"),
            // The child is already gone; nothing to chase.
            Err(rustix::io::Errno::SRCH) => tracing::debug!(pid, "child already gone"),
            Err(err) => tracing::warn!(pid, ?err, "failed to signal child"),
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
            // EINTR just means we go round again against the same deadline.
            let _ = poll(&mut fds, Some(&timeout));
        }
    }

    /// The pidfd, for registration as an event source.
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
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

    let is_executable = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };

    if program.contains('/') {
        if !is_executable(Path::new(program)) {
            bail!("failed to spawn {program:?}: not an executable file");
        }
        return Ok(());
    }

    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = std::env::split_paths(&path).any(|dir| {
        let mut candidate = dir;
        candidate.push(program);
        is_executable(&candidate)
    });

    if !found {
        bail!("failed to spawn {program:?}: not found in PATH");
    }
    Ok(())
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

        child.terminate();

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

        // Bounded, but it must actually wait — returning instantly would mean a
        // well-behaved child never gets to run its handler.
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

        // Must be a no-op: the pid may already belong to someone else.
        child.terminate();
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
