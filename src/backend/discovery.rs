//! DRM device and connector enumeration, and the output selection policy.
//!
//! Discovery is inverted relative to the obvious approach: the connector is
//! what we search for and the card is derived from it. A machine with an
//! integrated GPU plus a discrete card has connectors split across
//! `/dev/dri/card0` and `/dev/dri/card1`, so scanning only the first card would
//! report `DP-1` as missing when it exists.
//!
//! [`select`] is deliberately pure so the whole policy unit-tests without a GPU.

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rustix::fs::OFlags;
use smithay::backend::drm::{DrmDeviceFd, NodeType};
use smithay::backend::session::Session;
use smithay::backend::udev::UdevBackend;
use smithay::reexports::drm::control::{Device as ControlDevice, Mode, connector};
use smithay::utils::DeviceFd;

/// One connector on one DRM device, and everything needed to decide whether to
/// use it or to print it for `--list-outputs`.
#[derive(Debug, Clone)]
pub struct OutputCandidate {
    /// The DRM device this connector lives on, e.g. `/dev/dri/card1`.
    pub device: PathBuf,
    pub connector: connector::Handle,
    /// Connector name in the conventional form, e.g. `DP-1`, `HDMI-A-1`, `eDP-1`.
    pub name: String,
    pub connected: bool,
    /// The connector's preferred mode, or its first mode if none is flagged
    /// preferred. `None` when the connector reports no modes at all, which is
    /// normal for a disconnected connector.
    pub preferred_mode: Option<Mode>,
}

impl fmt::Display for OutputCandidate {
    /// The `--list-outputs` line format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.connected {
            "connected"
        } else {
            "disconnected"
        };
        write!(
            f,
            "{:<12} {:<13} {}",
            self.name,
            state,
            self.device.display()
        )?;
        if let Some(mode) = self.preferred_mode {
            write!(f, "{}", mode_suffix(mode.size(), mode.vrefresh()))?;
        }
        Ok(())
    }
}

/// The `WxH@Hz` suffix `--list-outputs` appends for a connector with a mode.
///
/// Separated from the `Display` impl so the format — which is a user-facing
/// contract, since it is how someone discovers what to pass to `--output` — can be
/// asserted without a real `drm::Mode`.
fn mode_suffix(size: (u16, u16), vrefresh: u32) -> String {
    format!("  {}x{}@{}", size.0, size.1, vrefresh)
}

/// The conventional name for a connector, matching what every other compositor
/// and `drmModeGetConnector` consumer produces.
pub fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// Pick a connector's preferred mode, falling back to its first mode.
fn preferred_mode(info: &connector::Info) -> Option<Mode> {
    pick_preferred(info.modes(), |mode| {
        mode.mode_type()
            .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
    })
}

/// The first item satisfying `preferred`, else the first item at all.
///
/// Generic over the item so the policy is testable without constructing a
/// `connector::Info`, which needs a real DRM device. Picking the wrong mode is the
/// silent "kiosk boots at the wrong resolution" bug.
fn pick_preferred<T: Copy>(items: &[T], preferred: impl Fn(&T) -> bool) -> Option<T> {
    items
        .iter()
        .find(|item| preferred(item))
        .or_else(|| items.first())
        .copied()
}

/// Enumerate every connector on every DRM device on the session's seat.
///
/// Candidates are returned sorted by `(device, connector handle id)` so that the
/// default "first connected connector" does not depend on udev enumeration
/// order between boots.
///
/// This opens each device fd through the session but deliberately does *not*
/// construct a [`DrmDevice`](smithay::backend::drm::DrmDevice): reading
/// resources only needs `ControlDevice`, so enumeration never becomes DRM master
/// and never disturbs an already-running compositor. That is also what lets
/// `--list-outputs` work over SSH.
pub fn enumerate<S: Session>(session: &mut S, udev: &UdevBackend) -> Result<Vec<OutputCandidate>> {
    let mut devices: Vec<PathBuf> = udev
        .device_list()
        .map(|(_id, path)| path.to_path_buf())
        .collect();
    devices.sort();
    enumerate_devices(session, devices)
}

/// Enumerate an explicit list of DRM devices.
///
/// Split from [`enumerate`] so the "one broken card must not hide a good one"
/// behaviour can be tested by passing a deliberately mixed list.
pub(crate) fn enumerate_devices<S: Session>(
    session: &mut S,
    devices: Vec<PathBuf>,
) -> Result<Vec<OutputCandidate>> {
    let mut candidates = Vec::new();

    for path in devices {
        match enumerate_device(session, &path) {
            Ok(mut found) => candidates.append(&mut found),
            // One unusable card should not prevent us finding the connector on
            // another. Render-only nodes with no connectors land here too.
            Err(err) => {
                tracing::debug!(device = %path.display(), ?err, "skipping DRM device");
            }
        }
    }

    candidates.sort_by(candidate_order);

    Ok(candidates)
}

/// Total order over candidates: by device path, then connector id.
///
/// Extracted so the default "first connected connector" can be tested without a
/// GPU. Determinism matters — without it the default output would follow udev
/// enumeration order and could differ between boots.
pub(crate) fn candidate_order(a: &OutputCandidate, b: &OutputCandidate) -> Ordering {
    a.device
        .cmp(&b.device)
        .then_with(|| u32::from(a.connector).cmp(&u32::from(b.connector)))
}

fn enumerate_device<S: Session>(session: &mut S, path: &Path) -> Result<Vec<OutputCandidate>> {
    // Only primary nodes carry connectors; skip render nodes early so the debug
    // log is not full of expected failures.
    if let Ok(node) = smithay::backend::drm::DrmNode::from_path(path)
        && node.ty() != NodeType::Primary
    {
        return Ok(Vec::new());
    }

    let raw_fd = session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|err| anyhow!("{err:?}"))
        .with_context(|| format!("failed to open DRM device {}", path.display()))?;

    // Enumeration is a read-only probe, so hand the device back to the session
    // when we are done with it. Dropping the fd instead would leave libseat with a
    // `TakeDevice` and no matching `ReleaseDevice` for every card on the seat, so
    // it would keep issuing pause/resume for cards the compositor never uses.
    let result = read_connectors(path, raw_fd.try_clone()?);
    if let Err(err) = session.close(raw_fd) {
        tracing::debug!(device = %path.display(), ?err, "failed to release DRM device");
    }
    result
}

/// Read a device's connectors from an already-open fd.
fn read_connectors(path: &Path, fd: std::os::fd::OwnedFd) -> Result<Vec<OutputCandidate>> {
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let resources = fd
        .resource_handles()
        .with_context(|| format!("failed to read DRM resources of {}", path.display()))?;

    let mut candidates = Vec::new();
    for handle in resources.connectors() {
        // `false`: do not force a probe. Probing a connector can take hundreds
        // of milliseconds per output and we only need the cached state.
        let info = match fd.get_connector(*handle, false) {
            Ok(info) => info,
            Err(err) => {
                tracing::debug!(?handle, ?err, "failed to read connector");
                continue;
            }
        };

        candidates.push(OutputCandidate {
            device: path.to_path_buf(),
            connector: *handle,
            name: connector_name(&info),
            connected: info.state() == connector::State::Connected,
            preferred_mode: preferred_mode(&info),
        });
    }

    Ok(candidates)
}

/// Apply the output selection policy.
///
/// With `wanted` set, only that connector will do; there is no fallback,
/// because a kiosk that boots to the wrong monitor is worse than one that fails
/// loudly. With `wanted` unset, the first connected connector wins.
pub fn select(candidates: Vec<OutputCandidate>, wanted: Option<&str>) -> Result<OutputCandidate> {
    let available = || {
        if candidates.is_empty() {
            "none".to_string()
        } else {
            candidates
                .iter()
                .map(|c| {
                    if c.connected {
                        c.name.clone()
                    } else {
                        format!("{} (disconnected)", c.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    };

    match wanted {
        Some(name) => {
            let found = candidates
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name));

            match found {
                Some(c) if c.connected => Ok(c.clone()),
                Some(c) => Err(anyhow!(
                    "output {:?} exists on {} but is disconnected; available outputs: {}",
                    c.name,
                    c.device.display(),
                    available()
                )),
                None => Err(anyhow!(
                    "no output named {name:?}; available outputs: {}",
                    available()
                )),
            }
        }
        None => candidates
            .iter()
            .find(|c| c.connected)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no connected output found; available outputs: {}",
                    available()
                )
            }),
    }
}

#[cfg(test)]
mod hardware_tests {
    //! Enumeration against the real DRM devices on this machine.
    //!
    //! These are the executable half of the `--list-outputs` smoke test. They
    //! skip themselves when DRM is unreachable — no GPU, or no permission —
    //! rather than failing, so CI without a GPU stays green.
    //!
    //! A [`DirectSession`] stands in for libseat: enumeration only needs an open
    //! fd, and going direct means these tests run without seatd or logind, which
    //! is exactly the situation on a developer machine already running a
    //! compositor.

    use std::path::Path;

    use rustix::fs::{Mode as FsMode, OFlags};
    use smithay::backend::session::{AsErrno, Session};

    use super::*;

    #[derive(Debug)]
    struct Errno(rustix::io::Errno);

    impl AsErrno for Errno {
        fn as_errno(&self) -> Option<i32> {
            Some(self.0.raw_os_error())
        }
    }

    /// Opens devices directly instead of through a seat manager.
    struct DirectSession;

    impl Session for DirectSession {
        type Error = Errno;

        fn open(
            &mut self,
            path: &Path,
            flags: OFlags,
        ) -> Result<std::os::fd::OwnedFd, Self::Error> {
            rustix::fs::open(path, flags, FsMode::empty()).map_err(Errno)
        }

        fn close(&mut self, _fd: std::os::fd::OwnedFd) -> Result<(), Self::Error> {
            Ok(())
        }

        fn change_vt(&mut self, _vt: i32) -> Result<(), Self::Error> {
            Ok(())
        }

        fn is_active(&self) -> bool {
            true
        }

        fn seat(&self) -> String {
            "seat0".to_string()
        }
    }

    /// Whether to run the hardware tests, and how loudly to skip.
    ///
    /// A skipped test still reports `ok`, and `cargo test` captures stdout — so on
    /// a runner without a GPU these print nothing and look like passes. Setting
    /// `KIOSK_REQUIRE_DRM=1` (which the manual test procedure does) turns an
    /// unreachable GPU into a failure, so a hardware run cannot silently degrade
    /// into a no-op.
    fn require_drm(what: &str) -> bool {
        if drm_reachable() {
            return true;
        }
        if std::env::var_os("KIOSK_REQUIRE_DRM").is_some() {
            panic!("KIOSK_REQUIRE_DRM is set but {what} is unavailable");
        }
        eprintln!("skipping: {what} unavailable");
        false
    }

    /// A udev handle, or `None` if it cannot be created.
    fn udev_or_skip() -> Option<UdevBackend> {
        if !require_drm("a DRM device") {
            return None;
        }
        match UdevBackend::new("seat0") {
            Ok(udev) => Some(udev),
            Err(err) => {
                if std::env::var_os("KIOSK_REQUIRE_DRM").is_some() {
                    panic!("KIOSK_REQUIRE_DRM is set but udev failed: {err}");
                }
                eprintln!("skipping: udev unavailable: {err}");
                None
            }
        }
    }

    /// True when at least one DRM primary node is open-able read-write.
    fn drm_reachable() -> bool {
        let Ok(entries) = std::fs::read_dir("/dev/dri") else {
            return false;
        };
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("card"))
                && rustix::fs::open(
                    entry.path(),
                    OFlags::RDWR | OFlags::CLOEXEC,
                    FsMode::empty(),
                )
                .is_ok()
        })
    }

    #[test]
    fn enumerate_finds_this_machines_connectors() {
        let Some(udev) = udev_or_skip() else { return };
        let candidates = enumerate(&mut DirectSession, &udev).expect("enumeration failed");

        // A machine with a DRM primary node should expose at least one connector.
        assert!(
            !candidates.is_empty(),
            "no connectors found on an accessible DRM device"
        );

        for candidate in &candidates {
            // Names must be usable as --output arguments.
            assert!(
                candidate.name.contains('-'),
                "connector name {:?} is not in INTERFACE-ID form",
                candidate.name
            );
            assert!(candidate.device.starts_with("/dev/dri/"));
            // A connected connector must offer a mode, or it could not be driven.
            if candidate.connected {
                assert!(
                    candidate.preferred_mode.is_some(),
                    "connected output {} reports no mode",
                    candidate.name
                );
            }
            eprintln!("found: {candidate}");
        }
    }

    #[test]
    fn enumeration_is_sorted_and_deterministic() {
        let Some(udev) = udev_or_skip() else { return };

        let first = enumerate(&mut DirectSession, &udev).expect("first enumeration failed");
        let second = enumerate(&mut DirectSession, &udev).expect("second enumeration failed");

        let names = |cs: &[OutputCandidate]| cs.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&first),
            names(&second),
            "enumeration order is not stable between calls"
        );

        // Ordering itself is asserted GPU-free in `tests::candidate_order_*`; here
        // we only check that repeated enumeration is stable.
    }

    #[test]
    fn a_render_node_yields_no_connectors() {
        // Render nodes carry no connectors and must be skipped silently rather
        // than logged as failures.
        let render_node = Path::new("/dev/dri/renderD128");
        if !render_node.exists() {
            eprintln!("skipping: no render node on this machine");
            return;
        }
        let found = enumerate_device(&mut DirectSession, render_node)
            .expect("a render node should be skipped, not an error");
        assert!(found.is_empty(), "a render node reported connectors");
    }

    #[test]
    fn an_unopenable_device_is_an_error_not_a_panic() {
        // enumerate() turns this into a skipped device; enumerate_device reports it.
        let err = enumerate_device(&mut DirectSession, Path::new("/dev/dri/card-nonexistent"))
            .expect_err("a missing device must fail");
        assert!(
            format!("{err:#}").contains("card-nonexistent"),
            "error should name the device: {err:#}"
        );
    }

    #[test]
    fn a_non_drm_device_is_an_error_not_a_panic() {
        // /dev/null opens fine but has no DRM resources, exercising the
        // resource_handles failure path rather than the open failure path.
        let err = enumerate_device(&mut DirectSession, Path::new("/dev/null"))
            .expect_err("a non-DRM device must fail");
        assert!(
            format!("{err:#}").contains("/dev/null"),
            "error should name the device: {err:#}"
        );
    }

    /// The behaviour the inverted design exists for: one unusable card must not
    /// hide the connectors on a good one.
    #[test]
    fn a_broken_device_does_not_hide_a_good_one() {
        let Some(udev) = udev_or_skip() else { return };
        let real: Vec<PathBuf> = udev
            .device_list()
            .map(|(_, path)| path.to_path_buf())
            .collect();

        let mut mixed = vec![
            PathBuf::from("/dev/null"),
            PathBuf::from("/dev/dri/card-nonexistent"),
        ];
        mixed.extend(real);

        let found = enumerate_devices(&mut DirectSession, mixed).expect("enumeration failed");
        assert!(
            !found.is_empty(),
            "the good card's connectors were lost to a bad device"
        );
    }

    #[test]
    fn only_bad_devices_yields_an_empty_list_rather_than_an_error() {
        let found = enumerate_devices(
            &mut DirectSession,
            vec![
                PathBuf::from("/dev/null"),
                PathBuf::from("/dev/dri/card-nonexistent"),
            ],
        )
        .expect("bad devices must be skipped, not fatal");
        assert!(found.is_empty());
    }

    /// The real selection path: whatever `--list-outputs` printed must be a valid
    /// `--output` argument, including case-insensitively.
    #[test]
    fn every_listed_connector_name_round_trips_through_select() {
        let Some(udev) = udev_or_skip() else { return };
        let candidates = enumerate(&mut DirectSession, &udev).expect("enumeration failed");

        for candidate in &candidates {
            let upper = candidate.name.to_uppercase();
            let result = select(candidates.clone(), Some(&upper));
            if candidate.connected {
                let picked = result.expect("connected output should be selectable");
                assert_eq!(picked.name, candidate.name);
            } else {
                let err = result.expect_err("disconnected output must not be selected");
                assert!(
                    err.to_string().contains("disconnected"),
                    "unexpected error: {err}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a candidate without touching DRM. The connector handle is unused by
    /// [`select`], so any non-zero handle is fine.
    fn candidate(device: &str, name: &str, connected: bool) -> OutputCandidate {
        OutputCandidate {
            device: PathBuf::from(device),
            connector: connector::Handle::from(std::num::NonZeroU32::new(1).unwrap()),
            name: name.to_string(),
            connected,
            preferred_mode: None,
        }
    }

    fn candidate_with(device: &str, name: &str, connector: u32) -> OutputCandidate {
        OutputCandidate {
            device: PathBuf::from(device),
            connector: connector::Handle::from(std::num::NonZeroU32::new(connector).unwrap()),
            name: name.to_string(),
            connected: true,
            preferred_mode: None,
        }
    }

    #[test]
    fn candidate_order_sorts_by_device_then_connector_id() {
        let mut candidates = [
            candidate_with("/dev/dri/card1", "DP-9", 2),
            candidate_with("/dev/dri/card0", "DP-2", 7),
            candidate_with("/dev/dri/card1", "DP-1", 1),
            candidate_with("/dev/dri/card0", "DP-1", 3),
        ];
        candidates.sort_by(candidate_order);

        let order: Vec<_> = candidates
            .iter()
            .map(|c| (c.device.to_str().unwrap(), u32::from(c.connector)))
            .collect();
        assert_eq!(
            order,
            [
                ("/dev/dri/card0", 3),
                ("/dev/dri/card0", 7),
                ("/dev/dri/card1", 1),
                ("/dev/dri/card1", 2),
            ]
        );
    }

    /// The cross-card case: a low handle on card1 must still sort after a high
    /// handle on card0, or the default output could flip between boots.
    #[test]
    fn candidate_order_puts_the_device_first() {
        let a = candidate_with("/dev/dri/card0", "DP-1", 99);
        let b = candidate_with("/dev/dri/card1", "DP-1", 1);
        assert_eq!(candidate_order(&a, &b), Ordering::Less);
    }

    #[test]
    fn the_preferred_flag_beats_an_earlier_mode() {
        let modes = [(1920, 1080, false), (2560, 1440, true)];
        assert_eq!(
            pick_preferred(&modes, |m| m.2),
            Some((2560, 1440, true)),
            "the PREFERRED-flagged mode must win"
        );
    }

    #[test]
    fn without_a_preferred_flag_the_first_mode_wins() {
        let modes = [(1024, 768, false), (800, 600, false)];
        assert_eq!(pick_preferred(&modes, |m| m.2), Some((1024, 768, false)));
    }

    #[test]
    fn no_modes_yields_none() {
        let empty: [(u16, u16, bool); 0] = [];
        assert_eq!(pick_preferred(&empty, |m| m.2), None);
    }

    #[test]
    fn the_mode_suffix_is_width_by_height_at_refresh() {
        // Catches a width/height transposition, which nothing else would.
        assert_eq!(mode_suffix((2560, 1440), 60), "  2560x1440@60");
        assert_eq!(mode_suffix((800, 600), 75), "  800x600@75");
    }

    #[test]
    fn listing_shows_name_state_and_device() {
        let line = candidate("/dev/dri/card1", "DP-1", true).to_string();
        assert!(line.contains("DP-1"), "{line}");
        assert!(line.contains("connected"), "{line}");
        assert!(line.contains("/dev/dri/card1"), "{line}");
    }

    #[test]
    fn listing_marks_a_disconnected_connector() {
        let line = candidate("/dev/dri/card0", "HDMI-A-1", false).to_string();
        assert!(line.contains("disconnected"), "{line}");
    }

    #[test]
    fn listing_omits_the_mode_when_there_is_none() {
        // A disconnected connector usually reports no modes; the line must still
        // be well formed rather than printing a bogus resolution.
        let line = candidate("/dev/dri/card0", "DP-2", false).to_string();
        assert!(!line.contains('@'), "unexpected mode in {line:?}");
    }

    #[test]
    fn named_output_is_selected() {
        let c = vec![
            candidate("/dev/dri/card0", "eDP-1", true),
            candidate("/dev/dri/card1", "DP-1", true),
        ];
        let picked = select(c, Some("DP-1")).unwrap();
        assert_eq!(picked.name, "DP-1");
        assert_eq!(picked.device, PathBuf::from("/dev/dri/card1"));
    }

    #[test]
    fn named_output_matching_is_case_insensitive() {
        let c = vec![candidate("/dev/dri/card0", "DP-1", true)];
        assert_eq!(select(c, Some("dp-1")).unwrap().name, "DP-1");
    }

    #[test]
    fn unknown_name_fails_and_lists_what_exists() {
        let c = vec![
            candidate("/dev/dri/card0", "eDP-1", true),
            candidate("/dev/dri/card0", "HDMI-A-1", false),
        ];
        let err = select(c, Some("BOGUS-9")).unwrap_err().to_string();
        assert!(err.contains("BOGUS-9"), "{err}");
        assert!(err.contains("eDP-1"), "{err}");
        assert!(err.contains("HDMI-A-1 (disconnected)"), "{err}");
    }

    #[test]
    fn known_but_disconnected_name_fails_without_falling_back() {
        let c = vec![
            candidate("/dev/dri/card0", "eDP-1", true),
            candidate("/dev/dri/card1", "DP-1", false),
        ];
        let err = select(c, Some("DP-1")).unwrap_err().to_string();
        assert!(err.contains("disconnected"), "{err}");
        // The crucial part: it did not silently pick the connected eDP-1.
        assert!(err.contains("DP-1"), "{err}");
    }

    #[test]
    fn default_picks_the_first_connected_candidate_in_order() {
        let c = vec![
            candidate("/dev/dri/card0", "VGA-1", false),
            candidate("/dev/dri/card0", "eDP-1", true),
            candidate("/dev/dri/card1", "DP-1", true),
        ];
        assert_eq!(select(c, None).unwrap().name, "eDP-1");
    }

    #[test]
    fn no_connected_output_fails() {
        let c = vec![candidate("/dev/dri/card0", "DP-1", false)];
        let err = select(c, None).unwrap_err().to_string();
        assert!(err.contains("no connected output"), "{err}");
    }

    #[test]
    fn no_candidates_at_all_fails() {
        let err = select(Vec::new(), None).unwrap_err().to_string();
        assert!(err.contains("none"), "{err}");

        let err = select(Vec::new(), Some("DP-1")).unwrap_err().to_string();
        assert!(err.contains("none"), "{err}");
    }
}
