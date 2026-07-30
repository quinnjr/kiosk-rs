# kiosk-rs manual test matrix

Most of a DRM compositor cannot be tested without hardware. The logic that
*can* be tested in isolation is covered by `cargo test`; everything below needs a
real machine and a free VT, and is part of the deliverable rather than an
afterthought.

Run the automated suite with `KIOSK_REQUIRE_DRM=1 cargo test` first: on hardware
that variable turns a silently-skipped DRM test into a failure, so you know the
GPU-dependent tests actually ran.

## Prerequisites

- A Linux machine with a GPU and at least one connected output.
- A free virtual terminal. Switch with `Ctrl+Alt+F<n>` and log in on the text
  console; do **not** run these from inside another compositor's terminal.
- A session provider (`seatd` or logind) — see
  [Requirements](../README.md#requirements) for the exact symptom when it is missing.
- Test clients: `foot`, `weston-simple-egl` (from `weston`), and a GTK
  application with a dialog and a menu, e.g. `gedit` or `gnome-text-editor`.

Run with `--exit-key Ctrl+Alt+BackSpace` throughout. Without it, a client that
grabs the keyboard and misbehaves leaves no way out but SSH or a power cycle.

## Cases

| # | Command | Expected | Covers |
|---|---------|----------|--------|
| 1 | `kiosk --exit-key Ctrl+Alt+BackSpace -- foot` | Terminal fills the screen, no decorations. Typing works. | `wl_shm` path, keyboard, fullscreen configure |
| 2 | `kiosk -- weston-simple-egl` | Spinning triangle, smooth, no tearing. | dmabuf import, frame pacing, vblank loop |
| 3 | `kiosk -- gnome-text-editor`, then open a dialog (e.g. Open File) | Dialog appears fullscreen on top and takes keyboard focus. Closing it returns focus to the document. | Toplevel stacking, focus transfer, `toplevel_destroyed` |
| 4 | Same as 3, then open a menu near the bottom-right corner | Menu is fully on screen, not clipped off the edge. | `xdg_popup`, positioner constraint |
| 4b | With a menu open, click somewhere outside it | Menu closes, and the window underneath is **not** raised or refocused by that click. | Popup grab, `focus_surface` grab guard |
| 4c | With a menu open, press `Escape` | Menu closes; keyboard focus returns to the toplevel. | Popup keyboard grab, focus restore |
| 4d | Open a submenu from a menu, then click outside both | The whole popup chain closes, not just the submenu. | Nested grab, `previous_serial` |
| 5 | With case 1 running, switch VT and back. **Note:** under logind/seatd the VT keyboard is in `K_OFF`, so `Ctrl+Alt+F<n>` may do nothing — use `sudo chvt N` from another machine over SSH instead, then `chvt` back. | Screen returns intact and input still works. Not black, not frozen. Modifiers are not stuck: typing in `foot` produces plain characters, and `--exit-key` still fires. | Session pause/resume, `reset_state`, held-key release |
| 6 | `kiosk -- true; echo $?` | Exits immediately, prints `0`. | Child exit propagation, pidfd |
| 7 | `kiosk -- sh -c 'exit 42'; echo $?` | Prints `42`. | Verbatim status propagation |
| 8 | `kiosk -- sleep 60`, then `kill -9` the `sleep` from another VT; `echo $?` | Prints `137` (`128 + 9`). | Signal death, shell convention |
| 9 | `kiosk -- foot`, then press `Ctrl+Alt+BackSpace`; `echo $?` | Exits `0` within ~2s; `foot` received `SIGTERM` and is gone. A child that ignores `SIGTERM` is left to init after the 2s grace, with a warning. | Exit keybind, bounded child termination |
| 10 | `kiosk --output DP-1 -- foot` (use a real connected name) | Runs on that output specifically. | Named output selection |
| 11 | `kiosk --output BOGUS-9 -- foot; echo $?` | Exits `1` with `no output named "BOGUS-9"; available outputs: ...`. Never enters graphics mode. | Unknown-name branch |
| 12 | `kiosk --output <a disconnected name> -- foot; echo $?` | Exits `1` mentioning `disconnected`. Does **not** fall back to a connected output. | Disconnected branch, no silent fallback |
| 13 | `kiosk --list-outputs` over SSH, no VT needed | Prints every connector with state and preferred mode; exits `0`. | Pre-EGL discovery |
| 14 | `kiosk --log-file /nonexistent-dir/x.log -- foot; echo $?` | Exits `1` with a visible message while stderr still works. | Log destination failure before graphics mode |
| 15 | `kiosk -vvv --log-file /tmp/k.log -- foot`, then inspect the file | The file holds kiosk's logs. The terminal stays silent — on a console the child's stdout/stderr are replaced with `/dev/null` (see case 24), so the client prints nothing there either. | Verbosity mapping, stream separation |
| 16 | `RUST_LOG=kiosk=warn kiosk -vvv -- foot` | Only warnings from us, despite `-vvv`. Note the target is `kiosk`, the crate name — `kiosk_rs` matches nothing. | `RUST_LOG` precedence |
| 17 | `kiosk -- foot` on a machine with two connected outputs | Only the first connected output lights up; the other stays blank. | Single-output policy |
| 18 | `kiosk -- /nonexistent-binary; echo $?` | Exits `1` naming the binary, **before** the screen is touched — the console never blanks. | Pre-flight binary check (tier 1 ordering) |
| 19 | `kiosk -- foot`, then **physically unplug an eGPU** or `echo 1 > /sys/bus/pci/devices/<addr>/remove`. `udevadm trigger --action=remove` is **not** equivalent — it fires a synthetic event while the device is still in sysfs, so it would pass even with a broken matcher. | Exits nonzero with `GPU removed` logged at `error`; does not sit frozen. | Tier 3 GPU-removal handling (dev_t matching) |
| 20 | Open a menu in a GTK app and hold it open for a minute without moving the mouse | The app stays responsive; the screen does not freeze. | Frame-callback release on empty frames |
| 21 | In a GTK app, hide a dialog and re-show it | The dialog reappears. | Configure-on-remap |
| 22 | `kiosk --log-file /tmp/k.log -- foot`, then `stat -c %a /tmp/k.log` | `600`. | Log file not world-readable |
| 23 | `ln -s /tmp/target /tmp/k.log`, then `kiosk --log-file /tmp/k.log -- /bin/true; echo $?` | Exits `1`; `/tmp/target` is untouched. | `O_NOFOLLOW` on the log sink |
| 24 | From a TTY, `kiosk -- sh -c 'ls -l /proc/self/fd; sleep 5'` | The child's fds 0/1/2 all point at `/dev/null`, not the console. | Console-fd escape closed |
| 25 | `kiosk -- foot` redirecting output: `kiosk -- foot > /tmp/app.log 2>&1` | `foot`'s own output still reaches `/tmp/app.log` — a redirected stream is passed through, only a console one is replaced. | Child logging preserved |
| 26 | `RUST_LOG=no_such_target=trace kiosk --output BOGUS-9 -- foot; echo $?` | Exits `1` **with a visible message** — a filter must not be able to silence a fatal error. | Unconditional fatal report |
| 27 | `kiosk -- foot`, then `kill <pid>` (plain `SIGTERM`) from another VT; `echo $?` | Exits `143` (`128+15`); the screen returns to text mode and `foot` is gone. | Signal handling, teardown on a supervisor stop |
| 28 | With a GTK app, open a menu, then a submenu, then a sub-submenu | All three appear. Nothing is disconnected — the depth cap is 8. | Popup depth cap not too tight |
| 29 | `touch /tmp/pre.log && chmod 666 /tmp/pre.log && kiosk --log-file /tmp/pre.log -- foot`, then `stat -c %a /tmp/pre.log` | `600` — an inherited file is tightened, not trusted. | Log sink `fchmod` |
| 30 | `touch /tmp/a && ln /tmp/a /tmp/b` (hard link), then `kiosk --log-file /tmp/b -- /bin/true; echo $?` | Exits `1`, complaining about hard links. | Log sink link check |
| 31 | `sudo mkdir -m 0777 /run/kiosk-test` (**no sticky bit**), then as another user `touch /run/kiosk-test/theirs.log`, then as root `kiosk --log-file /run/kiosk-test/theirs.log -- /bin/true; echo $?` | Exits `1`, naming the owning uid. Tightening the mode is not enough — the owner could `chmod` it back or hard-link it at any time. **`/tmp` will not work for this case**: it is sticky, and `fs.protected_regular=1` (the systemd default) denies the `O_CREAT` open of another user's file there with `EACCES` — even for root, which has no bypass — so you would see "Permission denied" from the open and never reach the ownership check. | Log sink ownership check |
| 32 | `kiosk -- foot`, then `kill <pid>`; time how long until the prompt returns, then `pgrep foot` | Effectively immediate — well under a second. If it takes the **full 2 seconds** the child never received `SIGTERM`, and `pgrep foot` will still match: `terminate` gives up after the grace and leaves it to init, so the application survives as an orphan rather than dying with its parent. That is what an inherited blocked signal mask looks like. | Signal mask reset in the child |
| 33 | A hand-written client calling `xdg_surface.get_popup` with a **null** parent (no toolkit does this; needs a custom client) | The client is disconnected with a protocol error. It must not accumulate silently — an unparented popup is never positioned or drawn. | Parent-less popup rejection |
| 34 | A hand-written client rooting a popup tree on an `xdg_surface` that is **never given a role** (not a toplevel), then opening popups in a loop | The 65th popup is refused with a protocol error. The cap counts popups directly, so it must not matter that the tree's root is not a toplevel. | Popup count cap is not derived from the window stack |
| 35 | `kiosk -- sh -c 'trap "" TERM; sleep 60'` (a child that ignores `SIGTERM`), then `kill <kiosk-pid>`, then **immediately `kill` it again** while it is still in the 2s grace | The console still returns to text mode with a working keyboard. The second signal is queued in the signalfd and delivered only when the mask lifts on teardown, so if the signal source is dropped before libseat closes the seat, the process dies before `libseat_close_seat` restores `KDSETMODE`/`KDSKBMODE` — leaving an unusable console. Most visible with libseat's **builtin** backend (a root kiosk with no seatd/logind), where nothing else cleans up. | Signal source drop order vs libseat teardown |
| 36 | In a GTK app with two toplevels stacked, hover the **lower** one to raise a tooltip, then click the tooltip | The tooltip's own toplevel is raised. A tooltip takes no grab, so this exercises the popup/subsurface climb rather than the grab path. | `focus_surface` popup resolution |

## Known limitations to confirm, not fix

These are deliberate v0.1 scope decisions. Confirm the behaviour matches the
description rather than filing it as a bug.

- **No cursor for named shapes.** A client that requests a named cursor shape
  rather than supplying a surface gets no visible cursor. Toolkits normally load
  the theme themselves and pass a surface, so in practice a cursor appears.
- **Popup grabs do not intercept touch.** `xdg-shell` grabs cover keyboard and
  pointer only, so on a touch-only device a tap outside a menu does not dismiss it
  through the grab; the client has to notice the tap itself.
- **At most 32 toplevels.** A client exceeding that is disconnected with a protocol
  error rather than being allowed to exhaust memory.
- **Startup input is dropped.** Probing for pointer/touch devices drains libinput's
  queue, so a keypress made before the compositor was ready is discarded.
- **No focus cycling.** There is no Alt-Tab. A stacked dialog is dismissed by the
  client, not by the compositor.
- **Fixed input capabilities.** Pointer and touch are advertised based on the
  devices present at startup. Plugging in a mouse afterwards does not add a
  pointer.
- **Scale is always 1.** No HiDPI scaling; clients see a scale-1 output.
- **No log rotation.** `--log-file` appends without bound. A display that fails
  continuously escalates to a nonzero exit after ~1s rather than retrying forever,
  so a stuck compositor cannot fill the disk.
- **Descriptors above stderr are not force-closed in the child.** `LibSeatSession`
  discards the `O_CLOEXEC` kiosk-rs requests, so close-on-exec for the DRM and evdev
  fds depends on the system libseat rather than on this code. See the note on
  `ChildProcess::spawn`.
- **The child runs as the compositor's user.** On a root kiosk that voids the VT
  confinement, since a root child can open `/dev/tty0` directly. There is no
  `--child-user` flag yet; arrange it in the unit file.
- **Subsurface depth is not capped, only popup depth is.** The same recursion
  argument applies — Smithay's `is_ancestor` recurses on every `get_subsurface`, and
  a commit walks the tree — but subsurface creation has no handler hook to reject
  from, so there is nowhere for this compositor to enforce a limit. A client can
  therefore still build a deep subsurface chain. Unlike the popup case the compositor's
  own walks are iterative, so the stack cost is Smithay's, not ours.
- **`setsid` is best-effort.** If it fails the child shares our session; the console
  descriptors are already closed, so this is defence in depth rather than the
  primary confinement.
