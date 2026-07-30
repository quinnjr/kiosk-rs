# kiosk-rs — Design

**Date:** 2026-07-29
**Status:** Approved (design); not yet implemented
**Crate:** `kiosk-rs` · **Binary:** `kiosk`

## 1. Purpose

`kiosk-rs` is a minimal Wayland compositor equivalent to [cage](https://github.com/cage-kiosk/cage):
it launches a single application fullscreen on a single output, forwards all input to it, and exits
when that application exits. Its target is kiosk, embedded, and appliance deployments where exactly
one program owns the screen.

It is not a window manager, has no configuration file, and has no compositor UI.

## 2. Scope

### In scope (v0.1)

- Bare-metal operation on a Linux TTY via DRM/KMS, udev, libseat, and libinput.
- Exactly one output, chosen by connector name or defaulted to the first connected connector.
- One fullscreen `xdg_toplevel` at a time, with a stack so additional toplevels (dialogs) work.
- `xdg_popup` support (menus, tooltips).
- Keyboard, pointer, and touch forwarding.
- `wl_shm` and dmabuf buffer paths.
- Child process lifecycle: spawn from argv, exit when it exits, propagate its exit status.
- VT switch away and back.
- Verbose logging levels and optional file destination.
- Optional single compositor keybind to exit.

### Out of scope (v0.1)

| Excluded | Rationale |
| --- | --- |
| Nested (winit) backend | Explicitly deferred. Development happens on a TTY. |
| Headless backend | No kiosk value; revisit if automated render tests are wanted. |
| Multi-window switching (Alt-Tab, `cage -s`) | Stacking covers dialogs; focus cycling is a WM feature. |
| XWayland | Large surface area (window hierarchy, override-redirect, WM protocol). |
| Damage-tracked rendering | Full-frame repaint per vblank is correct, just less efficient. |
| Multiple outputs (extend/mirror) | Requires a layout abstraction and per-output vblank timing. |
| `--mode` / refresh-rate override, rotation | Preferred mode only. |
| Log rotation | Documented limitation, not a silent one. |
| Configuration file | CLI flags only. |

## 3. Foundation

Built on [Smithay](https://github.com/Smithay/smithay), a pure-Rust compositor toolkit. Smithay
supplies Wayland protocol handling (`wayland-server`), the `calloop` event loop, DRM/GBM/libinput/udev
backends, an EGL/GLES2 renderer, and `xdg-shell` helpers. The cage-equivalent feature set lands in
roughly 1500 lines of Rust with no hand-written FFI.

Alternatives considered and rejected:

- **wlroots via FFI** — the C library cage itself uses. Most battle-tested, closest 1:1 port, but
  `wlroots-rs` is unmaintained, wlroots' API churns across releases, and the result is Rust that
  reads like C.
- **`wayland-rs` with no toolkit** — implementing `xdg-shell`, seat, DRM modesetting, and input
  dispatch directly. Maximum understanding, but rebuilds months of Smithay to reach the same place.

### Dependencies

| Crate | Purpose |
| --- | --- |
| `smithay` | Protocol, backends, renderer. Features: `backend_drm`, `backend_libinput`, `backend_session_libseat`, `backend_udev`, `backend_egl`, `renderer_gl`, `wayland_frontend` |
| `calloop` | Event loop (re-exported by Smithay; pinned to the same version) |
| `rustix` | `pidfd_open`, `kill`, `setsid`, `poll` |
| `clap` | Argument parsing (derive) |
| `tracing`, `tracing-subscriber` | Logging; `env-filter` feature |
| `anyhow` | Error context chains |
| `libc` | **dev-dependency only** — `poll(2)` and raw signals in tests |

System libraries: `libinput`, `libseat`, `libgbm`, `libEGL`.

## 4. Architecture

### 4.1 Shape

A single `struct Kiosk` holds all protocol state and the backend. Smithay's `delegate_*!` macros wire
Wayland globals to trait implementations on that struct — the pattern used by Smithay's own `anvil`
reference compositor.

Two deliberate simplifications:

- **No backend abstraction.** There is one backend. A `dyn Backend` trait or `enum Backend` seam
  would add indirection with nothing on the other side of it.
- **No `smithay::desktop::Space`.** With exactly one fullscreen window there is nothing to lay out.
  The render path collects render elements from a single `Window` positioned at the origin.

Rejected: cage's actual structure of one ~1200-line file. It works, but it forfeits the ability to
hold a module in context while editing it.

### 4.2 Event loop

Single-threaded `calloop::EventLoop<Kiosk>` with these sources:

| Source | Fires on | Action |
| --- | --- | --- |
| `ListeningSocketSource` | client connect | insert client into `Display` |
| `Generic<Display>` | client requests readable | `Display::dispatch_clients` |
| `DrmDevice` event source | vblank / page-flip complete | submit next frame, send frame callbacks |
| `LibinputInputBackend` | key / pointer / touch event | forward to `Seat` → focused surface |
| `Generic<pidfd>` | child process exits | `waitpid`, break loop, propagate status |
| `LibSeatSession` notifier | session pause / activate | suspend or resume backend |

The child watcher uses `pidfd_open(2)` rather than a `SIGCHLD` handler. A pidfd is pollable, so child
death becomes an ordinary calloop event with no async-signal-safety constraints and no race between
the handler and the loop.

### 4.3 Startup order

Order matters and is easy to get wrong:

1. Parse CLI; initialise `tracing` subscriber (before any Smithay call, so Smithay's own spans are
   captured).
2. Open the log file if `--log-file` was given. Failure here is fatal and reported to stderr.
3. Create `LibSeatSession`.
4. Create `Display`, bind the Wayland socket, register globals.
5. Enumerate DRM devices and connectors (§5).
6. Select the output (§5). `--list-outputs` prints and exits here, having never touched EGL.
7. Build `GbmDevice`, `EGLDisplay`/`EGLContext`, `GlesRenderer`, and `DrmCompositor` for the chosen
   card only.
8. Advertise the `wl_output` global from the connector's preferred mode.
9. Install panic containment (§8.2).
10. Spawn the child with `WAYLAND_DISPLAY` set; open its pidfd and register the source.
11. Run the loop.

The socket must exist before the child runs, and the output must be advertised before the child asks
for a fullscreen size.

### 4.4 Module layout

```
src/
  main.rs              CLI, tracing init, wiring, loop run, exit code
  cli.rs               argv parsing (-- separator for child command)
  state.rs             struct Kiosk + ClientState, window stack, render elements
  focus.rs             FocusTarget: the seat's keyboard/pointer/touch focus type
  pacing.rs            FramePacer: when to render and when to wait
  handlers/
    compositor.rs      CompositorHandler, BufferHandler, ShmHandler
    xdg_shell.rs       XdgShellHandler — fullscreen policy
    seat.rs            SeatHandler, focus, cursor image
    dmabuf.rs          DmabufHandler
  backend/
    discovery.rs       DRM device + connector enumeration and selection policy
    drm.rs             session, GBM, renderer, DrmCompositor, vblank → render
    input.rs           libinput event → Seat dispatch
  child.rs             spawn + pidfd exit watcher
tests/
  cli.rs               pre-hardware CLI behaviour, run against the real binary
docs/
  manual-test-matrix.md
```

Each `handlers/*` file is one protocol's worth of trait implementations: small, independently
readable, and identifiable from its filename. `backend/drm.rs` is the largest file and the only place
GPU setup lives.

## 5. Output selection

### 5.1 CLI

```
kiosk [OPTIONS] -- <COMMAND> [ARGS...]

Options:
      --output <CONNECTOR>   Use this connector (e.g. DP-1). Case-insensitive.
      --list-outputs         Print connectors and exit.
      --exit-key <BINDING>   Compositor keybind that exits kiosk-rs. Default: none.
      --log-file <PATH>      Write logs here instead of stderr.
  -v...                      Increase log verbosity (repeatable).
  -h, --help
  -V, --version
```

Behaviour:

- `--output DP-1` — use exactly this connector. If it is unknown or disconnected, exit 1 and print
  the connectors that *are* available. No fallback: a kiosk that boots to the wrong monitor is worse
  than one that fails loudly.
- `--list-outputs` — print connector name, connection state, and preferred mode, then exit 0. Runs
  before any EGL initialisation, so it works over SSH. This is the discoverability half of
  `--output`; without it, `--output` is guess-and-check on a machine with no other compositor.
- Neither — use the first connected connector. "First" is defined deterministically: candidates are
  sorted by `(device path, connector handle id)` before selection, so the default does not depend on
  udev enumeration order between boots.

### 5.2 Connector names

DRM reports an interface type and a per-type index, not a string. Names are constructed the way every
other compositor and `drmModeGetConnector` consumer does it:

```rust
format!("{}-{}", interface.as_str(), interface_id)   // "DP-1", "HDMI-A-1", "eDP-1"
```

Matching against `--output` is case-insensitive.

### 5.3 Discovery inverts device selection

With `--output`, the connector is what is being searched for and the card is derived from it. A
machine with an integrated GPU plus a discrete card has connectors split across `/dev/dri/card0` and
`/dev/dri/card1`, so scanning only the first card would report `DP-1` as missing when it exists.

Discovery therefore enumerates *all* DRM devices before applying any selection policy:

```rust
struct OutputCandidate {
    device: PathBuf,                 // /dev/dri/card1
    device_id: u64,                  // dev_t, captured while the node still exists
    connector: connector::Handle,
    name: String,                    // "DP-1"
    connected: bool,
    preferred_mode: Option<Mode>,
}

fn enumerate<S: Session>(
    session: &mut S,
    udev: &UdevBackend,
) -> Result<Vec<OutputCandidate>>;

fn select(
    candidates: Vec<OutputCandidate>,
    wanted: Option<&str>,
) -> Result<OutputCandidate>;
```

`enumerate` walks the `UdevBackend`'s device list, opens each device fd through the session, and reads
its resources. It returns candidates sorted by `(device, connector handle id)`. Its output is exactly
what `--list-outputs` prints, so both paths share one function.

`select` is pure and holds the entire policy — name matches, name unknown, name known but
disconnected, nothing connected. It is the one piece of the backend that unit-tests without a GPU,
which is why it is a separate module.

## 6. Logging

`-v` is counted (`clap::ArgAction::Count`) and mapped to a `tracing_subscriber::EnvFilter` directive.
The filter target is `kiosk` — the *crate* name, not the package name. The package
is `kiosk-rs`, but its only non-test target is the `kiosk` binary, so Cargo compiles
with `--crate-name kiosk` and every event's `module_path!()` root is `kiosk`.
A directive naming `kiosk_rs` matches nothing and silently degrades each row below
to its bare global level.

| Flag | Filter |
| --- | --- |
| *(none)* | `kiosk=info,warn` |
| `-v` | `kiosk=debug,info` |
| `-vv` | `kiosk=trace,debug` |
| `-vvv` | `trace` — includes Smithay's protocol-level spans and DRM atomic-commit detail |

If `RUST_LOG` is set it wins outright and the `-v` count is ignored — the standard `tracing`
convention, and the escape hatch for tracing one module without drowning in the rest.

Smithay instruments itself with `tracing`, so `-vvv` yields per-request Wayland protocol logging for
free, provided the subscriber is registered before any Smithay call.

### 6.1 Destination

Once kiosk-rs puts the TTY into graphics mode, `stderr` is written to a console nobody can see.
Verbose logging on the real target is write-only without a file destination.

- `--log-file <PATH>` absent → stderr.
- Present → opened in **append** mode, before graphics mode. A crash-looping kiosk keeps the history
  of every attempt, which is the case logs are most needed for.
- Parent directories are not created. A missing directory or unwritable path is a startup error,
  reported to stderr while stderr is still visible.
- ANSI colours are disabled when the sink is a file.
- No rotation in v0.1.

`-v` and `--log-file` are orthogonal: the count sets the filter, the flag sets the destination.

### 6.2 Child streams

The child inherits stdout and stderr **unless they are a terminal**, in which case
they are replaced with `/dev/null`. A kiosk application's own logging should not be
swallowed, so a stream the operator redirected to a file or pipe passes through
untouched. But a console descriptor is a VT-switching capability — a compromised
client can `ioctl(fd, VT_ACTIVATE, n)` on any inherited console fd and drop the
physical user at a login prompt — and the console is invisible anyway once the TTY
is in graphics mode. stdin is always `/dev/null`.

The child is also placed in its own session (`setsid`), which removes the
controlling terminal so `/dev/tty` cannot be reopened. That is defence in depth:
`setsid` does **not** invalidate descriptors already inherited across `exec`, so
closing the console streams above is the load-bearing measure.

## 7. Window and input policy

All window policy lives in `handlers/xdg_shell.rs` and is stated exhaustively:

- **New toplevel** — set the fullscreen state, set size to the output's mode, `send_configure`. Push
  onto a `Vec<Window>` stack; the top of the stack holds focus. A second toplevel stacks above and
  takes focus. No focus cycling.
- **Toplevel destroyed** — pop, focus the new top. An empty stack does **not** exit. kiosk-rs's
  lifetime is the child process's, full stop: one shutdown trigger, no interaction between two.
- **Client requests** `unset_fullscreen`, `maximize`, `unmaximize` — acknowledged with a configure
  that restates fullscreen. `move`, `resize`, and `minimize` are ignored: the protocol requires no
  reply to any of them, and the geometry is fixed regardless. Either way the client cannot escape the
  kiosk geometry.
- **Toplevel count** — capped at 32. A client that exceeds it is sent a protocol
  error and disconnected, so it cannot exhaust memory or stall the per-frame window
  walk. This is distinct from refusing a second toplevel, which would break dialogs.
- **Popups** — honoured via `PopupManager`, with the positioner constrained to the output rect.

Rejecting extra toplevels was considered and rejected: a GTK dialog *is* an `xdg_toplevel`, so
rejection breaks ordinary applications. Stacking is cage's default behaviour.

`backend/input.rs` is a thin forwarder:

- **Keyboard** — `KeyboardHandle::input()`; every key goes to the focused surface, except a match on
  `--exit-key` when configured.
- **Pointer** — motion clamped to the output rect. A cursor is rendered only if a pointer device
  exists at startup; its image comes from the client's `wl_pointer.set_cursor`.
- **Touch** — forwarded via `TouchHandle`. In scope because kiosk hardware is usually a touchscreen
  and it is a few lines alongside pointer handling.

`DataDeviceState` is registered even though there is a single client, because toolkits expect the
global to exist.

### 7.1 Session pause and resume

The only input path that is not a forwarder.

- `SessionEvent::PauseSession` — mark the backend inactive, stop rendering, suspend libinput.
- `SessionEvent::ActivateSession` — resume libinput, reset `DrmCompositor` state, mark everything
  damaged, schedule a frame.

Without this, switching away from the VT and back leaves the screen permanently black.

### 7.2 Exit keybind

`--exit-key <BINDING>` installs the only compositor-level keybind. Default is none, so the default
behaviour matches cage: every key reaches the client. With no nested backend available for
development, it is the difference between "wrong keymap, reboot the machine" and "press the key."

**Binding syntax:** `Mod+Mod+Keysym`, where each modifier is one of `Ctrl`, `Alt`, `Shift`, `Super`
(case-insensitive) and the final component is an xkb keysym name as accepted by
`xkbcommon::Keysym::from_name` — for example `Ctrl+Alt+Backspace` or `Super+q`. Parsing happens at
startup, and an unparseable binding is a startup error (tier 1), not a silently dead keybind. The
match is on modifier state plus keysym at key-press; the matched press is consumed and never forwarded
to the client.

## 8. Error handling

Three tiers:

A fatal error is always written to stderr with `eprintln!`, independently of the
`tracing` subscriber: a filter that matches nothing must not be able to make a
startup failure silent.

1. **Startup** — no DRM device, connector unknown or disconnected, EGL failure, child binary missing,
   socket bind failure, unwritable log file. All fail before graphics mode, print an `anyhow` context
   chain to stderr, and exit 1. Nothing here is recoverable, and a half-initialised compositor is
   worse than none.
2. **Runtime recoverable** — DRM commit returning `EBUSY`/`EACCES` after losing master, client
   protocol errors (Smithay disconnects that client), buffer import failure. Logged at `warn`; the
   frame is dropped and the loop continues.
3. **Runtime fatal** — GPU device removed, a `DrmEvent::Error` (page-flip completions
   were lost, so the vblank stream can no longer be trusted), a failed session
   resume, and event loop poll errors. Logged at `error`, followed by clean teardown
   and a nonzero exit.

   GPU removal is matched by the card's `dev_t`, captured at enumeration. By the time
   `UdevEvent::Removed` arrives the device is gone from sysfs, so resolving a `dev_t`
   back to a path fails — a path comparison would never fire.

A dropped frame is tier 2, but "drop it and continue" requires a wake-up: with no
flip queued there is no vblank, so the caller must release the clients and arm a
one-shot retry timer (upstream's `DrmCompositor::queue_frame` documents this as the
caller's responsibility). A frame that keeps dropping escalates to tier 3 after 60
consecutive failures — about a second at the 16ms retry interval. Retrying forever
would trade a visible freeze for an invisible one: the screen stuck either way,
while the retry loop fills a log that has no rotation.

### 8.1 Exit codes

kiosk-rs is transparent in scripts:

| Condition | Exit code |
| --- | --- |
| Child exited normally | the child's code, verbatim |
| Child died by signal | `128 + signum` (shell convention) |
| Exited via `--exit-key` | 0 (the child is sent `SIGTERM` and waited for, bounded at 2s, then left to init) |
| kiosk-rs startup failure | 1 |
| kiosk-rs runtime fatal | 1 |
| Child status could not be determined | 125, so it cannot be confused with a child that exited 1 |

A recorded child status outranks a teardown error: a fatal error during shutdown
must not rewrite a real `exit 42` into the ambiguous `1`.

### 8.2 Panic containment

A panic during render would otherwise leave the TTY in graphics mode with no input — an
unrecoverable machine.

The event loop run is therefore wrapped in `catch_unwind` rather than guarded by a
panic hook. A hook would have to be `Send + Sync` and `LibSeatSession` is neither,
but the better reason is that catching converts the panic into an ordinary error
return, so the normal teardown runs: dropping the `DrmCompositor` restores the
original CRTC configuration and dropping the session hands the VT back. A hook that
switched VTs itself would fight that same teardown. The VT is therefore restored
*after* the unwind, by `Drop`, not before it.

The first frame is painted inside the same `catch_unwind`, since it is the process's
first GPU submission and the most likely place to panic.

### 8.3 Teardown

Dropping `DrmCompositor` restores the original CRTC configuration; libseat closes the devices; and
dropping the session restores the VT.

## 9. Testing

A DRM compositor is largely untestable without hardware. The design concentrates testable logic
accordingly.

**Unit tests, no GPU:**

- `cli.rs` — the `--` separator, `-v` counting, `RUST_LOG` precedence over `-v`.
- `backend/discovery.rs::select` — all four branches: name matches, name unknown, name known but
  disconnected, nothing connected.
- Connector-name formatting.
- `--exit-key` binding parsing: valid bindings, unknown modifier, unknown keysym, missing keysym.
- Exit-code mapping, including the `128 + signum` path.

**Smoke test:** `--list-outputs` on real hardware. It never touches EGL, so it runs over SSH and
validates enumeration independently of rendering.

**Manual test matrix**, written to `docs/manual-test-matrix.md` and part of the deliverable:

| Case | Covers |
| --- | --- |
| `foot` | `wl_shm` buffer path, keyboard |
| `weston-simple-egl` | dmabuf path, frame pacing |
| A client that opens a dialog | toplevel stacking, focus transfer |
| A client with a menu | `xdg_popup`, positioner constraint |
| VT switch away and back | session pause/resume |
| Child exits 0 | normal shutdown, exit-code propagation |
| Child SIGKILLed | pidfd path, `128 + signum` |
| `--output BOGUS-9` | selection failure message |
| `--output` naming a disconnected connector | disconnected branch |
| `--list-outputs` over SSH | pre-EGL discovery |
| `--log-file` to an unwritable path | startup failure while stderr is visible |

Without this matrix, "tested" would mean nothing for this project.

**CI:** `cargo clippy -- -D warnings`, `cargo fmt --check`, and the unit tests. No GPU in CI.
