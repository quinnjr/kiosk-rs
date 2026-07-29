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
- `seatd` running (`systemctl start seatd`, and your user in the `seat` group) or
  a valid logind session. Without either, libseat has no backend and startup
  fails with `Function not implemented (os error 38)`.
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
| 15 | `kiosk -vvv --log-file /tmp/k.log -- foot`, then inspect the file | Contains Smithay protocol-level spans and DRM commit detail. Terminal output is the client's only. | Verbosity mapping, stream separation |
| 16 | `RUST_LOG=kiosk_rs=warn kiosk -vvv -- foot` | Only warnings from us, despite `-vvv`. | `RUST_LOG` precedence |
| 17 | `kiosk -- foot` on a machine with two connected outputs | Only the first connected output lights up; the other stays blank. | Single-output policy |
| 18 | `kiosk -- /nonexistent-binary; echo $?` | Exits `1` naming the binary, **before** the screen is touched — the console never blanks. | Pre-flight binary check (tier 1 ordering) |
| 19 | `kiosk -- foot`, then from another VT `sudo udevadm trigger --action=remove /sys/class/drm/card1` (or physically unplug an eGPU) | Exits nonzero with `GPU removed` logged at `error`; does not sit frozen. | Tier 3 GPU-removal handling |
| 20 | Open a menu in a GTK app and hold it open for a minute without moving the mouse | The app stays responsive; the screen does not freeze. | Frame-callback release on empty frames |
| 21 | In a GTK app, hide a dialog and re-show it | The dialog reappears. | Configure-on-remap |
| 22 | `kiosk --log-file /tmp/k.log -- foot`, then `stat -c %a /tmp/k.log` | `600`. | Log file not world-readable |
| 23 | `ln -s /tmp/target /tmp/k.log`, then `kiosk --log-file /tmp/k.log -- /bin/true; echo $?` | Exits `1`; `/tmp/target` is untouched. | `O_NOFOLLOW` on the log sink |

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
- **No log rotation.** `--log-file` appends without bound.
