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
by a signal, so it is transparent inside a script or a systemd unit. Two codes are
its own: **1** for a kiosk-rs failure (bad arguments, no such output, a fatal
runtime error) and **125** when the child's status could not be determined. A
supervisor stop (`SIGTERM`) also exits `128 + signum`, after giving the child the
same 2-second grace as `--exit-key`. See spec §8.1 for the full table.

If `--output` names a connector that is unknown or disconnected, startup fails and
lists what is available — there is no fallback (see spec §5.1 for why).

## Requirements

Runs on bare metal on a Linux TTY. There is no nested backend, so it cannot be
launched from inside another compositor.

- A session provider: `seatd` running with your user in the `seat` group, or a
  valid logind session. Without one, startup fails with
  `Function not implemented (os error 38)`.
- System libraries: `libinput`, `libseat`, `libgbm`, `libEGL`, `libudev`,
  `libxkbcommon`, `libdrm`.

**Run the application as an unprivileged user.** The child inherits the
compositor's uid, and a kiosk is usually started as root at boot. kiosk-rs replaces
a console stdout/stderr with `/dev/null` and puts the child in its own session so it
cannot switch VTs, but a child running *as root* does not need an inherited
descriptor — it can open `/dev/tty0` or `/dev/input/event*` directly and escape the
kiosk regardless. There is currently no `--child-user` flag, so this has to be
arranged by the unit file or wrapper that launches kiosk-rs.

```sh
cargo build --release   # binary at target/release/kiosk
```

### Packages

CI builds a `.deb` and an `.rpm` from every release build and uploads them as
workflow artifacts. Locally:

```sh
cargo install cargo-deb cargo-generate-rpm
cargo build --release
cargo deb --no-build      # target/debian/*.deb
cargo generate-rpm        # target/generate-rpm/*.rpm
```

Arch users have [`kiosk-rs-git`](https://aur.archlinux.org/packages/kiosk-rs-git)
in the AUR.

Two notes if you package this yourself. **`libEGL` is dlopened, not linked**, so
no dependency scanner finds it — it is added by hand in both package definitions,
and a package built without it installs cleanly and then fails at startup.
And `cargo deb` needs `dpkg-dev` present: without it, `$auto` resolves to nothing
and cargo-deb emits a *warning* rather than an error, producing a `.deb` whose
`Depends` field has silently lost every library. CI asserts that field is
populated for exactly that reason.

## Logging

Verbosity maps onto a `tracing` filter targeting `kiosk`; `-vvv` includes Smithay's
own protocol-level spans and DRM commit detail. **`-vvv` records every keystroke** —
Smithay traces the resolved keysym of each key event — so treat a `-vvv` log as
sensitive, keep it off shared directories, and do not leave it enabled on a kiosk
that takes a PIN or password. The sink is created mode `0600`, and kiosk-rs refuses
a log path that is not a regular file, has more than one hard link, or is owned by
another user — tightening the mode on a file someone else owns is theatre, since
they can loosen it again or hard-link it whenever they like. A non-empty `RUST_LOG` overrides the `-v` count
entirely (an empty one is ignored) — but a fatal error is always printed to stderr regardless, so no filter
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

Those files are also published as a site at
[quinnjr.github.io/kiosk-rs](https://quinnjr.github.io/kiosk-rs/), which renders this
README and the two documents above directly — there is no second copy to keep in
sync. To work on it: `cd docs && pnpm install && pnpm dev`.

## Releasing

The repository follows git flow: `feature/*` and `bugfix/*` branch off `develop`,
`release/*` stabilises a version, and `hotfix/*` branches off `main`. `main` holds
the released tree and is where versions are tagged.

```sh
git flow release start 0.2.0
# bump `version` in Cargo.toml, commit, let CI go green on the branch
git flow release finish 0.2.0     # merges to main + develop, tags v0.2.0
git push origin main develop --tags
```

Pushing the tag is the trigger. `.github/workflows/release.yml` then builds the
`.deb` and `.rpm` from that exact tag and attaches them to a GitHub release whose
notes are the annotated tag's own message — so the release text cannot drift from
the tag. Two guards run first and fail the release rather than shipping:

- **the tag must be reachable from `main`**, since a tag anywhere else means
  `git flow release finish` never ran and the published tree is not the one `main`
  calls released;
- **`Cargo.toml`'s version must equal the tag**, or the artifacts get their
  filenames from one and their contents from the other.

crates.io is published only if a `CARGO_REGISTRY_TOKEN` secret exists; otherwise
that step is skipped and `cargo publish` is a one-liner locally. `workflow_dispatch`
re-runs the whole thing against an existing tag without moving it.

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
| `child.rs` | 92% | 92% | spawn, reap, terminate, exit-status mapping |
| `backend/discovery.rs` | 67% | 89% | output enumeration and selection policy |
| `backend/drm.rs` | 17% | 17% | the frame-drop budget (the rest needs a GPU) |
| **overall** | **51%** | **54%** | |

Both columns are line coverage from `cargo llvm-cov --summary-only`, measured on the
same machine: the "With a GPU" column directly, and the "No GPU" column by hiding
`/dev/dri` (`unshare -rm sh -c 'mount -t tmpfs none /dev/dri && cargo llvm-cov'`) so
the hardware tests take their skip path exactly as they do in CI. The DRM tests still
*run* in that column — they return early rather than being excluded, so their guards
and prologues count as covered. (An earlier version measured it with
`--skip hardware_tests`, which excludes them entirely and understates it.)

A caveat worth knowing: because those tests early-return rather than being
excluded, the test *count* is identical whether or not a GPU is present, so a CI
log alone cannot tell you they no-oped. `KIOSK_REQUIRE_DRM=1` is what makes the
difference visible — it turns each skip into a failure. Note that the skips are not
all keyed on the same condition: most test `drm_reachable()`, but
`a_render_node_yields_no_connectors` keys on whether a *render node* exists, which is
a strictly narrower check.

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
