//! kiosk-rs: a minimal Wayland compositor that runs one application fullscreen
//! on one output and exits when it exits.

mod backend;
mod child;
mod cli;
mod focus;
mod handlers;
mod pacing;
mod state;

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use smithay::backend::drm::DrmEvent;
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::ImportDma;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::desktop::PopupManager;
use smithay::input::SeatState;
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::input::Libinput;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use crate::backend::discovery;
use crate::backend::drm::DrmBackend;
use crate::child::ChildProcess;
use crate::cli::Cli;
use crate::state::{ClientState, Kiosk};

/// Exit code for any failure of the compositor itself, as opposed to a status
/// propagated from the child.
const EXIT_FAILURE: u8 = 1;

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = init_logging(&cli) {
        eprintln!("kiosk: {err:#}");
        return ExitCode::from(EXIT_FAILURE);
    }

    // Validate everything that needs no hardware first, so a typo is reported
    // without depending on a seat being available.
    // Everything here needs no hardware: argument shape, the keybind, and whether
    // the child binary exists. Spec tier 1 wants all of these reported before the
    // screen is taken, and doing them before `run` also means they are reported
    // without depending on a seat being available at all.
    let exit_key = match cli
        .validate()
        // Not for `--list-outputs`, which legitimately has no command.
        .and_then(|()| {
            if cli.list_outputs {
                Ok(())
            } else {
                child::preflight(&cli.command)
            }
        })
        .and_then(|()| cli.exit_key.as_deref().map(cli::parse_binding).transpose())
    {
        Ok(exit_key) => exit_key,
        Err(err) => {
            eprintln!("kiosk: {err:#}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    match run(&cli, exit_key) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_FAILURE)),
        Err(err) => {
            // Startup failures happen before graphics mode, so stderr is still
            // visible. The printed chain is what a user running this by hand
            // actually sees; the log line is for the --log-file case.
            // `eprintln!` unconditionally: it is the only report that cannot be
            // suppressed by a filter. An earlier version emitted this only when
            // `--log-file` was set, on the assumption that the subscriber would
            // cover the stderr case — but a `RUST_LOG` that matches nothing (or
            // any filter below `error`) then made a startup failure exit 1 in
            // total silence. The `tracing` copy is for the log file, so it is the
            // one that is conditional.
            if cli.log_file.is_some() {
                tracing::error!("{err:#}");
            }
            eprintln!("kiosk: {err:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Install the tracing subscriber.
///
/// Done first so Smithay's own spans are captured, and the log file is opened
/// before anything can put the TTY into graphics mode: an unwritable path must
/// fail while stderr is still readable.
fn init_logging(cli: &Cli) -> Result<()> {
    use tracing_subscriber::EnvFilter;

    // RUST_LOG wins outright over the -v count: it is the escape hatch for
    // tracing one module without drowning in everything else.
    let filter = EnvFilter::new(filter_directive(
        std::env::var("RUST_LOG").ok().as_deref(),
        cli.verbose,
    ));

    match &cli.log_file {
        Some(path) => {
            // Append rather than truncate: a crash-looping kiosk keeps the
            // history of every attempt, which is the case logs are most needed
            // for. Parent directories are deliberately not created.
            let file = open_log_sink(path)?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(Mutex::new(file))
                // Escape codes in a file are noise.
                .with_ansi(false)
                .init();
        }
        None => {
            let ansi = std::io::stderr().is_terminal();
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(ansi)
                .init();
        }
    }

    Ok(())
}

/// The filter directive to use: `RUST_LOG` when set and non-empty, else the `-v`
/// count.
///
/// `RUST_LOG` winning outright is the documented escape hatch for tracing one
/// module without drowning in the rest.
fn filter_directive(rust_log: Option<&str>, verbose: u8) -> String {
    match rust_log {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => cli::log_filter(verbose).to_string(),
    }
}

/// Open the log sink.
///
/// Mode `0600`: at `-vvv` the log carries Smithay's protocol traces, which include
/// keycodes and window titles — on a kiosk taking a PIN that must not be
/// world-readable. `O_NOFOLLOW`: the compositor may run as root and the documented
/// example path is under `/tmp`, so a pre-planted symlink would otherwise get root
/// appending attacker-influenced text to a file of their choosing.
///
/// Append, not truncate, so a crash-looping kiosk keeps the history of every
/// attempt. Parent directories are deliberately not created.
fn open_log_sink(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc_o_nofollow())
        .open(path)
        .with_context(|| format!("failed to open log file {}", path.display()))
}

/// `O_NOFOLLOW` without pulling in `libc` as a production dependency.
const fn libc_o_nofollow() -> i32 {
    // Linux value, stable across architectures that matter here.
    0o400_000
}

fn run(cli: &Cli, exit_key: Option<cli::Binding>) -> Result<i32> {
    let (mut session, session_notifier) =
        LibSeatSession::new().context("failed to open a session (is seatd or logind running?)")?;
    let seat_name = session.seat();

    let udev = UdevBackend::new(&seat_name)
        .with_context(|| format!("failed to scan devices on seat {seat_name:?}"))?;

    // Enumerate before touching EGL so --list-outputs works over SSH.
    let candidates = discovery::enumerate(&mut session, &udev)?;

    if cli.list_outputs {
        if candidates.is_empty() {
            println!("no DRM connectors found on seat {seat_name:?}");
        } else {
            for candidate in &candidates {
                println!("{candidate}");
            }
        }
        return Ok(0);
    }

    let chosen = discovery::select(candidates, cli.output.as_deref())?;

    let mut event_loop: EventLoop<Kiosk> =
        EventLoop::try_new().context("failed to create event loop")?;
    let loop_handle = event_loop.handle();

    let display: Display<Kiosk> = Display::new().context("failed to create Wayland display")?;
    let display_handle = display.handle();

    // Bind the socket before spawning anything: the child needs it to exist.
    let socket = ListeningSocketSource::new_auto().context("failed to bind a Wayland socket")?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    loop_handle
        .insert_source(socket, |stream, _, state: &mut Kiosk| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, ClientState::new_arc())
            {
                tracing::warn!(?err, "failed to accept client");
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to register socket source: {err}"))?;

    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, CalloopMode::Level),
            |_, display, state: &mut Kiosk| {
                // SAFETY: the display is only ever dispatched from this callback,
                // so the inner value is never aliased.
                unsafe { display.get_mut() }.dispatch_clients(state)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to register display source: {err}"))?;

    // Take DRM master and set up rendering.
    let init = DrmBackend::init(&mut session, &chosen)?;
    let backend = init.backend;
    let output = init.output;

    loop_handle
        .insert_source(
            init.notifier,
            |event, _metadata, state: &mut Kiosk| match event {
                DrmEvent::VBlank(_crtc) => state.on_vblank(),
                DrmEvent::Error(err) => {
                    // This is emitted when reading the DRM event queue fails,
                    // which means page-flip completions in that read were lost.
                    // The vblank stream is no longer trustworthy, so a pending
                    // flip would never complete and the compositor would stall
                    // silently. Exit instead, so a supervisor can restart us.
                    tracing::error!(?err, "fatal DRM error; the vblank stream is unreliable");
                    state.shutdown(EXIT_FAILURE as i32);
                }
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to register DRM source: {err}"))?;

    // Watch for the GPU disappearing. Spec tier 3: log, tear down, exit nonzero.
    // Without this, a removed card produces one warning per commit forever while
    // the screen is frozen and the process never exits.
    let chosen_device = chosen.device.clone();
    let chosen_device_id = chosen.device_id;
    loop_handle
        .insert_source(udev, move |event, _, state: &mut Kiosk| {
            // Compare `dev_t`, not a path. By the time this fires the device is
            // gone from sysfs, so `DrmNode::from_dev_id` (which stats
            // `/sys/dev/char/M:m/device/drm`) and `dev_path` both fail — matching
            // on a resolved path would silently never fire on a real removal.
            if let UdevEvent::Removed { device_id } = event
                && device_id == chosen_device_id
            {
                tracing::error!(device = %chosen_device.display(), "GPU removed");
                state.shutdown(EXIT_FAILURE as i32);
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to register udev source: {err}"))?;

    // Protocol globals. The output global is advertised before the child starts,
    // so the client's first toplevel already knows its fullscreen size.
    let compositor_state = CompositorState::new::<Kiosk>(&display_handle);
    let xdg_shell_state = XdgShellState::new::<Kiosk>(&display_handle);
    let _xdg_decoration_state = XdgDecorationState::new::<Kiosk>(&display_handle);
    let shm_state = ShmState::new::<Kiosk>(&display_handle, Vec::new());
    // Held as locals until `run` returns, matching `_xdg_decoration_state` below.
    // Dropping these does not withdraw the globals (none of them implement
    // `Drop`); they simply need to outlive the loop.
    let _output_manager_state = OutputManagerState::new_with_xdg_output::<Kiosk>(&display_handle);
    let data_device_state = DataDeviceState::new::<Kiosk>(&display_handle);
    let _output_global = output.create_global::<Kiosk>(&display_handle);

    let mut dmabuf_state = DmabufState::new();
    let dmabuf_formats = backend.renderer.dmabuf_formats();
    let _dmabuf_global = dmabuf_state.create_global::<Kiosk>(&display_handle, dmabuf_formats);

    // Seat and input devices.
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&display_handle, seat_name.clone());

    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| anyhow::anyhow!("failed to assign libinput to seat {seat_name:?}"))?;

    seat.add_keyboard(Default::default(), 200, 25)
        .context("failed to create keyboard (is the XKB keymap available?)")?;

    // Advertise pointer and touch only if such a device exists. In particular the
    // cursor is only ever drawn when a pointer is present.
    let capabilities = probe_capabilities(&mut libinput)?;
    let pointer = capabilities.pointer.then(|| seat.add_pointer());
    if capabilities.touch {
        seat.add_touch();
    }
    tracing::info!(
        pointer = capabilities.pointer,
        touch = capabilities.touch,
        "input capabilities"
    );

    let input_backend = LibinputInputBackend::new(libinput.clone());
    loop_handle
        .insert_source(
            input_backend,
            |event: InputEvent<LibinputInputBackend>, _, state: &mut Kiosk| {
                state.process_input_event(event);
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to register input source: {err}"))?;

    // VT switching. Without pause/resume, switching away and back leaves the
    // screen permanently black.
    let mut libinput_for_session = libinput;
    loop_handle
        .insert_source(
            session_notifier,
            move |event, _, state: &mut Kiosk| match event {
                SessionEvent::PauseSession => {
                    tracing::info!("session paused");
                    // The release of the Ctrl+Alt chord that switched away goes to
                    // the incoming VT, so tell the client those keys are up before
                    // we stop dispatching. Otherwise it sees them held forever and
                    // `--exit-key`, which needs exact modifiers, stops matching.
                    state.release_all_keys();
                    libinput_for_session.suspend();
                    state.backend.pause();
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("session activated");
                    if libinput_for_session.resume().is_err() {
                        tracing::error!("failed to resume libinput");
                    }
                    match state.backend.resume() {
                        Ok(()) => state.render(),
                        Err(err) => {
                            // The screen cannot be driven any more. Exiting runs
                            // the normal teardown, which restores the CRTC and the
                            // VT — strictly better than a live process holding a
                            // display it cannot paint.
                            tracing::error!("{err:#}; shutting down");
                            state.shutdown(EXIT_FAILURE as i32);
                        }
                    }
                }
            },
        )
        .map_err(|err| anyhow::anyhow!("failed to register session source: {err}"))?;

    // Start the pointer in the middle of the screen, which is where a user will
    // look for it.
    let pointer_location = {
        let size = output
            .current_mode()
            .map(|mode| mode.size)
            .unwrap_or_default();
        (size.w as f64 / 2.0, size.h as f64 / 2.0).into()
    };

    let mut state = Kiosk {
        display_handle: display_handle.clone(),
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        data_device_state,
        dmabuf_state,
        seat,
        pointer,
        cursor_status: CursorImageStatus::default_named(),
        pointer_location,
        exit_key,
        pressed_keys: HashSet::new(),
        last_input_time: 0,
        popups: PopupManager::default(),
        windows: Vec::new(),
        output,
        backend,
        start_time: Instant::now(),
        loop_signal: event_loop.get_signal(),
        loop_handle: loop_handle.clone(),
        retry_armed: false,
        consecutive_drops: 0,
        exit_code: None,
        child_reaped: false,
    };

    // Everything is ready: start the application.
    let child = Arc::new(Mutex::new(ChildProcess::spawn(&cli.command, &socket_name)?));

    // Everything from here to the loop can fail. Any early return would skip the
    // teardown below and leave the application running on a screen nobody is
    // compositing, so failures are funnelled through one place that kills it.
    let wire_up = || -> Result<()> {
        let pidfd = child
            .lock()
            .unwrap()
            .pidfd()
            .try_clone_to_owned()
            .context("failed to duplicate the child pidfd")?;

        let child_for_source = Arc::clone(&child);
        loop_handle
            .insert_source(
                Generic::new(pidfd, Interest::READ, CalloopMode::Level),
                move |_, _, state: &mut Kiosk| {
                    // A pidfd becomes readable exactly once, when the child exits.
                    let code = child_for_source.lock().unwrap().reap();
                    state.child_reaped = true;
                    state.shutdown(code);
                    Ok(PostAction::Remove)
                },
            )
            .map_err(|err| anyhow::anyhow!("failed to register child watcher: {err}"))?;
        Ok(())
    };

    if let Err(err) = wire_up() {
        child.lock().unwrap().terminate();
        return Err(err);
    }

    tracing::info!(socket = %socket_name, output = %chosen.name, "compositor running");

    let loop_result = run_event_loop(&mut event_loop, &mut state);

    // Shut the child down before propagating any loop error, so a panic or a
    // fatal DRM error does not leave an orphaned application running on a screen
    // nobody is compositing.
    if !state.child_reaped {
        child.lock().unwrap().terminate();
    }

    // A recorded child status outranks a loop error: the documented contract is
    // that kiosk is transparent in a script, so a teardown error must not rewrite
    // a real `exit 42` into the ambiguous `1`.
    if let Err(err) = loop_result {
        tracing::error!("{err:#}");
        if state.exit_code.is_none() {
            return Err(err);
        }
    }

    let code = state.exit_code.unwrap_or(0);
    tracing::info!(code, "shutting down");
    Ok(code)
}

/// Which input capabilities to advertise on the seat.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Capabilities {
    pointer: bool,
    touch: bool,
}

impl Capabilities {
    /// Fold one device's capabilities into the running total.
    ///
    /// Separated from libinput iteration so the accumulation rule is testable:
    /// capabilities are a union across all devices, never a replacement, or a
    /// keyboard appearing after a mouse would clear the pointer.
    fn add_device(&mut self, has_pointer: bool, has_touch: bool) {
        self.pointer |= has_pointer;
        self.touch |= has_touch;
    }
}

/// Probe the initial device set for pointer and touch capabilities.
///
/// Dispatching once is what makes libinput emit `DeviceAdded` for devices that
/// already exist. Devices hotplugged after this point do not change the
/// advertised capabilities: a kiosk's hardware is fixed, and changing seat
/// capabilities mid-session is a protocol event most single-window clients
/// handle poorly.
///
/// Note that this drains *all* queued libinput events, not just device ones, so
/// any real input queued before startup (the release of the Enter that launched
/// us, typically) is discarded rather than delivered to the client.
fn probe_capabilities(libinput: &mut Libinput) -> Result<Capabilities> {
    use smithay::reexports::input::DeviceCapability;
    use smithay::reexports::input::event::{Event as LibinputEvent, EventTrait};

    let mut caps = Capabilities::default();

    // A failure here is not survivable as a silent empty result: on a touchscreen
    // appliance it would mean touch is never advertised and the kiosk boots
    // completely uninteractive, indistinguishable from keyboard-only hardware.
    libinput
        .dispatch()
        .context("failed to probe input devices")?;

    for event in &mut *libinput {
        if let LibinputEvent::Device(device_event) = event {
            let device = device_event.device();
            caps.add_device(
                device.has_capability(DeviceCapability::Pointer),
                device.has_capability(DeviceCapability::Touch),
            );
        }
    }

    Ok(caps)
}

/// Run the event loop so that a panic inside it still restores the console.
///
/// A panic mid-render would otherwise unwind straight out of `main`, leaving the
/// TTY in graphics mode with no input: a machine that cannot be recovered
/// without a power cycle.
///
/// This catches the unwind instead of installing a panic hook. A hook would need
/// to be `Send + Sync` and `LibSeatSession` is neither, but more importantly
/// catching here converts the panic into an ordinary error return, so the normal
/// teardown path runs: dropping the `DrmCompositor` restores the original CRTC
/// configuration and dropping the session hands the VT back to the kernel. A
/// hook that switched VTs itself would fight that same teardown.
fn run_event_loop(event_loop: &mut EventLoop<Kiosk>, state: &mut Kiosk) -> Result<()> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The first frame is inside the guard deliberately: it is the process's
        // first GLES/DRM submission and the most likely place to panic, and a panic
        // outside `catch_unwind` would skip the child teardown in `run`, orphaning
        // the application on a screen nobody composites. Painting it here also
        // means the screen is black rather than whatever the previous owner of the
        // framebuffer left behind.
        state.render();

        event_loop.run(None, state, |state| {
            // Popups whose clients are gone would otherwise render forever.
            state.popups.cleanup();
            if let Err(err) = state.display_handle.flush_clients() {
                tracing::warn!(?err, "failed to flush clients");
            }
        })
    }));

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err).context("event loop failed"),
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(panic = %message, "compositor panicked");
            anyhow::bail!("compositor panicked: {message}")
        }
    }
}

/// Best-effort extraction of a panic payload's message.
///
/// `panic!` produces a `&str` for a literal and a `String` for a formatted
/// message; anything else came from `panic_any` and has no printable form.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_default_to_nothing() {
        let caps = Capabilities::default();
        assert!(!caps.pointer);
        assert!(!caps.touch);
    }

    #[test]
    fn one_device_grants_exactly_its_own_capabilities() {
        for (pointer, touch) in [(true, false), (false, true), (true, true), (false, false)] {
            let mut caps = Capabilities::default();
            caps.add_device(pointer, touch);
            assert_eq!(caps, Capabilities { pointer, touch }, "{pointer} {touch}");
        }
    }

    /// The bug the union guards against: a later capability-less device must not
    /// clear what an earlier one established.
    #[test]
    fn a_keyboard_after_a_mouse_does_not_clear_the_pointer() {
        let mut caps = Capabilities::default();
        caps.add_device(true, false);
        caps.add_device(false, false);
        assert!(caps.pointer, "the mouse was forgotten");
    }

    #[test]
    fn capabilities_accumulate_across_devices() {
        let mut caps = Capabilities::default();
        caps.add_device(false, false); // keyboard
        caps.add_device(true, false); // mouse
        caps.add_device(false, true); // touchscreen
        assert_eq!(
            caps,
            Capabilities {
                pointer: true,
                touch: true
            }
        );
    }

    #[test]
    fn rust_log_overrides_the_verbose_count() {
        assert_eq!(
            filter_directive(Some("kiosk_rs=trace"), 0),
            "kiosk_rs=trace"
        );
        // Even at -vvv, an explicit RUST_LOG wins outright.
        assert_eq!(filter_directive(Some("kiosk_rs=warn"), 3), "kiosk_rs=warn");
    }

    #[test]
    fn an_absent_or_blank_rust_log_falls_back_to_the_verbose_count() {
        assert_eq!(filter_directive(None, 0), "kiosk=info,warn");
        assert_eq!(filter_directive(None, 1), "kiosk=debug,info");
        assert_eq!(filter_directive(Some(""), 2), "kiosk=trace,debug");
        assert_eq!(filter_directive(Some("   "), 3), "trace");
    }

    #[test]
    fn the_log_file_is_appended_not_truncated() {
        // The README promises a crash-looping kiosk keeps every attempt's history.
        let path = std::env::temp_dir().join(format!("kiosk-log-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"first run\n").unwrap();

        {
            use std::io::Write;
            let mut sink = open_log_sink(&path).unwrap();
            writeln!(sink, "second run").unwrap();
        }

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.starts_with("first run"),
            "history was truncated: {body:?}"
        );
        assert!(body.contains("second run"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_new_log_file_is_not_world_readable() {
        // At -vvv the log carries protocol traces, including keycodes.
        use std::os::unix::fs::MetadataExt;
        let path = std::env::temp_dir().join(format!("kiosk-mode-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let file = open_log_sink(&path).unwrap();
        let mode = file.metadata().unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "log file is readable by others");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unwritable_log_path_is_reported_with_the_path() {
        let err = open_log_sink(Path::new("/nonexistent-dir-xyz/k.log")).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent-dir-xyz/k.log"));
    }

    #[test]
    fn panic_message_reads_a_literal_panic() {
        let payload = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "boom");
    }

    #[test]
    fn panic_message_reads_a_formatted_panic() {
        let value = 7;
        let payload = std::panic::catch_unwind(|| panic!("boom {value}")).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "boom 7");
    }

    #[test]
    fn panic_message_falls_back_for_an_unprintable_payload() {
        // `panic_any` with a non-string type: nothing sensible to print.
        let payload = std::panic::catch_unwind(|| std::panic::panic_any(42u32)).unwrap_err();
        assert_eq!(panic_message(payload.as_ref()), "unknown panic payload");
    }

    #[test]
    fn an_unwrap_on_none_is_reported_as_a_string_panic() {
        // The realistic shape of a compositor panic: a message std formats for us.
        let payload = std::panic::catch_unwind(|| {
            let empty: Vec<u8> = Vec::new();
            // A real out-of-bounds panic, whose message std formats for us.
            empty[0]
        })
        .unwrap_err();
        let message = panic_message(payload.as_ref());
        assert!(
            message.contains("index out of bounds"),
            "expected the std panic text, got {message:?}"
        );
    }
}
