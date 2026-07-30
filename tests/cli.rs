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
///
/// **This proxy only bites where no seat provider exists** — it matches the exact
/// context string `LibSeatSession::new()` failure carries. On a machine where seatd
/// or logind *is* running the session would succeed silently, the validation error
/// would surface with its normal message, and these assertions would still pass.
/// So this detects a mis-ordering in CI and on a seatless dev box, but not on the
/// hardware the manual matrix targets. The exit-code and message assertions below
/// hold everywhere and are the primary check.
fn mentions_the_session(err: &str) -> bool {
    err.contains("is seatd or logind running")
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

    // Exit 2 and the conflict text specifically: asserting merely "not 0" would
    // also be satisfied by the session failure that follows if the `conflicts_with`
    // attribute were deleted, so the test would pass while the guard was gone.
    assert_eq!(out.status.code(), Some(2), "expected clap's conflict exit");
    let err = stderr(&out);
    assert!(err.contains("--list-outputs"), "{err}");
    assert!(
        err.contains("cannot be used with"),
        "expected a clap conflict error, got: {err}"
    );
}

/// The filter directives are useless if they name a target no event carries, and
/// that failure is invisible: levels appear to work via the bare global default.
#[test]
fn our_own_log_events_are_actually_matched_by_the_v_filter() {
    // A startup failure logs at error. With a filter that matches only our target,
    // the line must still appear — if the target were wrong, output would be empty
    // apart from the unconditional eprintln.
    let out = Command::new(env!("CARGO_BIN_EXE_kiosk"))
        .args(["--output", "BOGUS-9", "--", "/bin/true"])
        .env("RUST_LOG", "kiosk=trace")
        .output()
        .expect("failed to run the kiosk binary");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("kiosk:"),
        "expected a diagnostic on stderr, got: {err}"
    );
}

/// A fatal error must never be silent, whatever the filter says.
#[test]
fn a_startup_failure_is_reported_even_when_the_filter_suppresses_everything() {
    let out = Command::new(env!("CARGO_BIN_EXE_kiosk"))
        .args(["--output", "BOGUS-9", "--", "/bin/true"])
        // Matches nothing this binary emits.
        .env("RUST_LOG", "no_such_target=trace")
        .output()
        .expect("failed to run the kiosk binary");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.trim().is_empty(),
        "a startup failure exited 1 with no diagnostic at all"
    );
}

/// `--list-outputs` takes no command, so the pre-flight binary check must not be
/// applied to it. Regressed once when preflight moved into the validation chain.
#[test]
fn list_outputs_does_not_require_a_command() {
    let out = kiosk(&["--list-outputs"]);
    let err = stderr(&out);
    assert!(
        !err.contains("empty command"),
        "--list-outputs was rejected for having no command: {err}"
    );
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
