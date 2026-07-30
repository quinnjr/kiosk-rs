//! Command line interface.
//!
//! Also owns parsing of the `--exit-key` binding syntax, since that is argument
//! validation: an unparseable binding is a startup error, not a silently dead
//! keybind.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use smithay::input::keyboard::{Keysym, ModifiersState, xkb};

/// Minimal Wayland kiosk compositor: runs one application fullscreen on one
/// output and exits when it exits.
#[derive(Parser, Debug)]
#[command(
    name = "kiosk",
    version,
    about,
    long_about = None,
    // Clap would otherwise render `[COMMAND]...` and never mention `--`, which is
    // what separates the child's flags from ours. The docs and the error path both
    // show this form, so `--help` must agree.
    override_usage = "kiosk [OPTIONS] -- <COMMAND> [ARGS...]"
)]
pub struct Cli {
    /// Use this connector, e.g. DP-1 (case-insensitive).
    ///
    /// If the connector is unknown or disconnected, startup fails rather than
    /// falling back to another output.
    #[arg(long, value_name = "CONNECTOR")]
    pub output: Option<String>,

    /// Print the available connectors and exit.
    #[arg(long, conflicts_with = "command")]
    pub list_outputs: bool,

    /// Keybind that exits the compositor, e.g. Ctrl+Alt+Backspace.
    ///
    /// Default is none, so every key reaches the client.
    #[arg(long, value_name = "BINDING")]
    pub exit_key: Option<String>,

    /// Write logs to this file instead of stderr.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Increase log verbosity (repeatable). Overridden by RUST_LOG.
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// The application to run, and its arguments.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    pub command: Vec<String>,
}

impl Cli {
    /// Reject argument combinations clap cannot express.
    pub fn validate(&self) -> Result<()> {
        if !self.list_outputs && self.command.is_empty() {
            bail!("no command given; usage: kiosk [OPTIONS] -- <COMMAND> [ARGS...]");
        }
        Ok(())
    }
}

/// The `tracing` target our own events carry.
///
/// This is the *crate* name, not the package name. The package is `kiosk-rs` but
/// its only non-test target is the `kiosk` binary, so Cargo compiles it with
/// `--crate-name kiosk` and `module_path!()` — hence every event target — is
/// `kiosk`. Filtering on `kiosk_rs` matches nothing, which silently reduces every
/// directive below to its bare global level.
#[cfg(test)]
pub const LOG_TARGET: &str = "kiosk";

/// The `EnvFilter` directive implied by a `-v` count.
///
/// `RUST_LOG` takes precedence over this and is handled by the caller.
pub fn log_filter(verbose: u8) -> &'static str {
    match verbose {
        0 => "kiosk=info,warn",
        1 => "kiosk=debug,info",
        2 => "kiosk=trace,debug",
        _ => "trace",
    }
}

/// A parsed `--exit-key` binding: a set of required modifiers plus a keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub keysym: Keysym,
}

impl Binding {
    /// Does this binding match the given modifier state and keysym?
    ///
    /// Modifiers must match exactly, so `Ctrl+q` does not fire on
    /// `Ctrl+Alt+q`. Lock modifiers are ignored.
    pub fn matches(&self, mods: &ModifiersState, keysym: Keysym) -> bool {
        self.keysym == keysym
            && self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && self.shift == mods.shift
            && self.logo == mods.logo
    }
}

/// Parse `Mod+Mod+Keysym`, e.g. `Ctrl+Alt+Backspace` or `Super+q`.
///
/// Modifier names are case-insensitive; the final component is an xkb keysym
/// name. Keysym lookup is case-insensitive too, so `Q` and `q` both resolve —
/// use an explicit `Shift+` to require the shift modifier.
pub fn parse_binding(spec: &str) -> Result<Binding> {
    let mut parts = spec.split('+').map(str::trim).peekable();
    let mut binding = Binding {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        keysym: Keysym::NoSymbol,
    };

    let mut key: Option<&str> = None;
    while let Some(part) = parts.next() {
        if part.is_empty() {
            bail!("empty component in binding {spec:?}");
        }

        // The last component is the keysym; everything before it is a modifier.
        if parts.peek().is_none() {
            key = Some(part);
            break;
        }

        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => binding.ctrl = true,
            "alt" => binding.alt = true,
            "shift" => binding.shift = true,
            "super" | "logo" | "mod4" => binding.logo = true,
            other => bail!(
                "unknown modifier {other:?} in binding {spec:?} \
                 (expected Ctrl, Alt, Shift, or Super)"
            ),
        }
    }

    let key = key.with_context(|| format!("binding {spec:?} has no keysym"))?;
    let keysym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym == Keysym::NoSymbol {
        bail!("unknown keysym {key:?} in binding {spec:?}");
    }

    binding.keysym = keysym;
    Ok(binding)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool, logo: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        }
    }

    #[test]
    fn log_filter_saturates() {
        assert_eq!(log_filter(0), "kiosk=info,warn");
        assert_eq!(log_filter(1), "kiosk=debug,info");
        assert_eq!(log_filter(2), "kiosk=trace,debug");
        assert_eq!(log_filter(3), "trace");
        assert_eq!(log_filter(9), "trace");
    }

    /// The directives are worthless if they name a target no event carries.
    /// `module_path!()` here is the crate root, which is what a directive must
    /// match.
    #[test]
    fn the_filter_target_matches_this_crates_real_module_path() {
        let crate_root = module_path!().split("::").next().unwrap();
        assert_eq!(
            crate_root, LOG_TARGET,
            "log directives target {LOG_TARGET:?} but events are emitted as {crate_root:?}"
        );
        for verbose in 0..=2 {
            assert!(
                log_filter(verbose).starts_with(crate_root),
                "-v{verbose} directive {:?} does not match target {crate_root:?}",
                log_filter(verbose)
            );
        }
    }

    #[test]
    fn command_after_double_dash_keeps_its_own_flags() {
        let cli = Cli::parse_from(["kiosk", "--output", "DP-1", "--", "foot", "--version"]);
        assert_eq!(cli.output.as_deref(), Some("DP-1"));
        assert_eq!(cli.command, ["foot", "--version"]);
        cli.validate().unwrap();
    }

    #[test]
    fn command_without_double_dash_also_works() {
        let cli = Cli::parse_from(["kiosk", "foot"]);
        assert_eq!(cli.command, ["foot"]);
        cli.validate().unwrap();
    }

    #[test]
    fn verbose_counts() {
        assert_eq!(Cli::parse_from(["kiosk", "foot"]).verbose, 0);
        assert_eq!(Cli::parse_from(["kiosk", "-vv", "foot"]).verbose, 2);
        assert_eq!(
            Cli::parse_from(["kiosk", "-v", "-v", "-v", "foot"]).verbose,
            3
        );
    }

    #[test]
    fn missing_command_is_rejected_unless_listing_outputs() {
        assert!(Cli::parse_from(["kiosk"]).validate().is_err());
        assert!(
            Cli::parse_from(["kiosk", "--list-outputs"])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn parses_modifier_combinations() {
        let b = parse_binding("Ctrl+Alt+BackSpace").unwrap();
        assert!(b.ctrl && b.alt && !b.shift && !b.logo);
        assert_eq!(b.keysym, Keysym::BackSpace);

        let b = parse_binding("super+q").unwrap();
        assert!(b.logo && !b.ctrl && !b.alt && !b.shift);
        assert_eq!(b.keysym, Keysym::q);

        // Bare keysym, no modifiers.
        let b = parse_binding("Escape").unwrap();
        assert!(!b.ctrl && !b.alt && !b.shift && !b.logo);
        assert_eq!(b.keysym, Keysym::Escape);
    }

    #[test]
    fn modifier_names_are_case_insensitive() {
        assert_eq!(
            parse_binding("CTRL+ALT+Delete").unwrap(),
            parse_binding("ctrl+alt+Delete").unwrap()
        );
        assert_eq!(
            parse_binding("Super+q").unwrap(),
            parse_binding("Logo+q").unwrap()
        );
    }

    #[test]
    fn rejects_bad_bindings_with_a_discriminating_message() {
        // Assert the message, not just is_err: collapsing these into one generic
        // error would otherwise go unnoticed, and the message is what the user
        // sees when a unit file has a typo.
        for (spec, expected) in [
            ("Hyper+q", "unknown modifier"),
            ("Ctrl+NotAKey", "unknown keysym"),
            ("", "empty component"),
            ("Ctrl+", "empty component"),
            ("Ctrl++q", "empty component"),
        ] {
            let err = format!("{:#}", parse_binding(spec).unwrap_err());
            assert!(
                err.contains(expected),
                "{spec:?} gave the wrong error: {err}"
            );
        }
    }

    #[test]
    fn matches_requires_exact_modifiers() {
        let b = parse_binding("Ctrl+Alt+BackSpace").unwrap();
        assert!(b.matches(&mods(true, true, false, false), Keysym::BackSpace));
        // Wrong keysym.
        assert!(!b.matches(&mods(true, true, false, false), Keysym::q));
        // Missing a required modifier.
        assert!(!b.matches(&mods(true, false, false, false), Keysym::BackSpace));
        // Extra modifier held.
        assert!(!b.matches(&mods(true, true, true, false), Keysym::BackSpace));
    }

    #[test]
    fn lock_modifiers_do_not_block_a_match() {
        let b = parse_binding("Ctrl+q").unwrap();
        let state = ModifiersState {
            ctrl: true,
            caps_lock: true,
            num_lock: true,
            ..Default::default()
        };
        assert!(b.matches(&state, Keysym::q));
    }
}
