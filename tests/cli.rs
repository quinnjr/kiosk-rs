//! End-to-end checks of the parts of `main` that run before any hardware.
//!
//! These exist because the ordering in `main` is load-bearing and invisible to
//! unit tests: CLI validation must happen *before* `LibSeatSession::new()`, so a
//! typo in a unit file is reported plainly instead of being masked by "is seatd
//! running?" on a machine that has no seat. An innocent refactor that moved the
//! validation below the session setup would break that with no other signal.

use std::process::{Command, Output};

fn kiosk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kiosk"))
        .args(args)
        // Otherwise an ambient RUST_LOG changes what lands on stderr.
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run the kiosk binary")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The marker that we reached session setup, i.e. validation ran too late.
fn mentions_the_session(err: &str) -> bool {
    err.contains("seatd") || err.contains("logind") || err.contains("session")
}

#[test]
fn no_command_fails_with_usage_before_touching_a_seat() {
    let out = kiosk(&[]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("no command given"), "{err}");
    assert!(
        !mentions_the_session(&err),
        "validation ran after session setup: {err}"
    );
}

#[test]
fn an_unknown_modifier_is_rejected_before_touching_a_seat() {
    let out = kiosk(&["--exit-key", "Hyper+q", "--", "/bin/true"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("unknown modifier"), "{err}");
    assert!(
        err.contains("Hyper"),
        "the error should quote the input: {err}"
    );
    assert!(
        !mentions_the_session(&err),
        "keybind parsing ran after session setup: {err}"
    );
}

#[test]
fn an_unknown_keysym_is_rejected_before_touching_a_seat() {
    let out = kiosk(&["--exit-key", "Ctrl+NotAKey", "--", "/bin/true"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("unknown keysym"), "{err}");
    assert!(!mentions_the_session(&err), "{err}");
}

#[test]
fn a_missing_child_binary_is_rejected_before_graphics_mode() {
    // Spec tier 1: "child binary missing" must fail before the screen is taken.
    let out = kiosk(&["--", "kiosk-rs-no-such-binary-xyz"]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("kiosk-rs-no-such-binary-xyz"), "{err}");
}

#[test]
fn an_unwritable_log_path_fails_while_stderr_is_still_visible() {
    let out = kiosk(&[
        "--log-file",
        "/nonexistent-dir-xyz/k.log",
        "--",
        "/bin/true",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("/nonexistent-dir-xyz/k.log"), "{err}");
    assert!(
        !mentions_the_session(&err),
        "the log file was opened after session setup: {err}"
    );
}

#[test]
fn list_outputs_refuses_a_command_rather_than_silently_discarding_it() {
    // A stray --list-outputs in a unit file must not report success while the
    // application never starts.
    let out = kiosk(&["--list-outputs", "--", "/bin/true"]);
    assert_ne!(out.status.code(), Some(0), "a command was silently ignored");
}

#[test]
fn help_and_version_succeed_and_document_the_separator() {
    let help = kiosk(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.contains("-- <COMMAND>"),
        "--help must document the `--` separator: {text}"
    );

    assert_eq!(kiosk(&["--version"]).status.code(), Some(0));
}
