# kiosk-rs

A minimal Wayland compositor that runs one application fullscreen on one output
and exits when it exits — a Rust equivalent of [cage](https://github.com/cage-kiosk/cage),
built on [Smithay](https://github.com/Smithay/smithay).

It is not a window manager. There is no configuration file, no compositor UI, and
no way for the client to leave the kiosk geometry.

## Usage

```
kiosk [OPTIONS] -- <COMMAND> [ARGS...]

      --output <CONNECTOR>   Use this connector, e.g. DP-1 (case-insensitive)
      --list-outputs         Print the available connectors and exit
      --exit-key <BINDING>   Keybind that exits the compositor, e.g. Ctrl+Alt+BackSpace
      --log-file <PATH>      Write logs here instead of stderr
  -v...                      Increase log verbosity (repeatable)
```

```sh
# Find out what outputs exist. Never initialises EGL and never takes DRM master,
# so it is safe to run over SSH — though it still needs a session provider (see
# Requirements), which a plain logind SSH session does not give you.
kiosk --list-outputs

# Run a browser on a specific screen, with an escape hatch.
kiosk --output DP-1 --exit-key Ctrl+Alt+BackSpace -- firefox --kiosk https://example.com
```

`kiosk` exits with the application's own exit status, or `128 + signum` if it died
by a signal, so it is transparent inside a script or a systemd unit.

If `--output` names a connector that is unknown or disconnected, startup fails and
lists what is available. There is no fallback: a kiosk that boots to the wrong
monitor is worse than one that fails loudly.

## Requirements

Runs on bare metal on a Linux TTY. There is no nested backend, so it cannot be
launched from inside another compositor.

- A session provider: `seatd` running with your user in the `seat` group, or a
  valid logind session. Without one, startup fails with
  `Function not implemented (os error 38)`.
- System libraries: `libinput`, `libseat`, `libgbm`, `libEGL`, `libudev`,
  `libxkbcommon`, `libdrm`.

```sh
cargo build --release   # binary at target/release/kiosk
```

## Logging

Verbosity maps onto a `tracing` filter targeting `kiosk`; `-vvv` includes Smithay's
own protocol-level spans and DRM commit detail. `RUST_LOG` overrides the `-v` count
entirely — but a fatal error is always printed to stderr regardless, so no filter
can make a startup failure silent.

Once the TTY is in graphics mode, stderr goes to a console nobody can see, so
`--log-file` is what makes verbose logging useful on the real target. It appends,
so a crash-looping kiosk keeps the history of every attempt.

The application keeps its own stdout and stderr **only when you have redirected
them** to a file or pipe. A stream that is still a console is replaced with
`/dev/null`, because an inherited console descriptor lets a compromised client
switch VTs and escape the kiosk — and the console is unreadable under graphics mode
anyway. So redirect if you want the application's logging: `kiosk -- app > app.log 2>&1`.

## Scope

In: DRM/KMS on one output, one fullscreen toplevel at a time (with a stack, so
dialogs work), popups, keyboard/pointer/touch, `wl_shm` and dmabuf, VT switching.

Popups take a proper keyboard and pointer grab, so menus dismiss on a click or key
outside them.

Out, deliberately: nested and headless backends, focus cycling (Alt-Tab),
XWayland, multiple outputs, damage-tracked rendering, HiDPI scaling, mode and
rotation overrides, log rotation.

See [`docs/manual-test-matrix.md`](docs/manual-test-matrix.md) for the hardware
test procedure and the full list of known limitations, and
[`docs/superpowers/specs/`](docs/superpowers/specs/) for the design rationale.

## Testing

```sh
cargo test                             # unit + integration tests
KIOSK_REQUIRE_DRM=1 cargo test         # additionally require a reachable GPU
cargo llvm-cov --summary-only          # coverage by module
```

The testable logic is deliberately concentrated. Coverage depends on whether the
machine has a reachable GPU, so both numbers are given — the "no GPU" column is
what CI measures:

| Module | No GPU (CI) | With a GPU | What it holds |
| --- | --- | --- | --- |
| `pacing.rs` | 99% | 99% | the frame-pacing state machine |
| `cli.rs` | 99% | 99% | argument and keybind parsing |
| `child.rs` | ~93% | ~93% | spawn, reap, terminate, exit-status mapping |
| `backend/discovery.rs` | ~73% | ~92% | output enumeration and selection policy |
| **overall** | **~49%** | **~52%** | |

Both columns are `cargo llvm-cov --summary-only`. The "No GPU" column is what CI
reports: the DRM tests still *run* there, they just return early once
`drm_reachable()` is false, so their guards and prologues count as covered. (An
earlier version of this table measured that column with `--skip hardware_tests`,
which excludes them entirely and understates it by several points.)

A caveat worth knowing: because those tests early-return rather than being
excluded, the test *count* is identical whether or not a GPU is present, so a CI
log alone cannot tell you they no-oped. `KIOSK_REQUIRE_DRM=1` is what makes the
difference visible.

`child.rs` tests spawn real processes and poll the real pidfd, so exit-code
propagation and `128 + signum` are verified end to end rather than mirrored.
`tests/cli.rs` runs the real binary to check the pre-hardware failure paths,
including that argument validation happens *before* any seat is opened.
`backend::discovery` enumerates this machine's actual connectors when `/dev/dri`
is reachable, opening devices directly so it needs no seat manager — those tests
skip themselves otherwise, which is why `KIOSK_REQUIRE_DRM=1` exists: it turns an
unreachable GPU into a failure so a hardware run cannot silently become a no-op.

The remainder is not reachable from a test process: taking DRM master needs a GPU
that no other compositor is holding, and `Kiosk` cannot be constructed without it —
which gates `handlers/`, `focus.rs`, and most of the input forwarders. Those are
covered by [`docs/manual-test-matrix.md`](docs/manual-test-matrix.md), and **the
manual matrix is the real gate**: several defects in the GPU-only paths are
invisible to every automated check here.
