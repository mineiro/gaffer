# Changelog

Notable changes to gaffer, newest first.

This file is the **canonical** record. The GitHub release notes are generated
from the section below matching the tag, and the `%changelog` in the Fedora
spec — which now lives in [mineiro/rpms](https://github.com/mineiro/rpms) — is a
derived summary. This is the one place a change needs to be written down.

Versions follow [semantic versioning](https://semver.org). While gaffer is
below 1.0 the minor slot carries features and the patch slot carries fixes.
The D-Bus API is a contract: every entry that touches it says whether the
change is additive, and so far all of them have been.

## [Unreleased]

## [0.2.1] — 2026-08-18

Housekeeping: gaffer's behaviour and its D-Bus API are unchanged. Cut so the
dependency refresh below reaches distro packages, which build from a release
tarball and the `Cargo.lock` committed beside it rather than from `main`.

### Changed

- Release process: pushing a `v*` tag publishes a GitHub release with notes from
  this file. Push order no longer matters.
- Release tags are now signed and immutable. Cut them with `git tag -s`; an
  unsigned tag is rejected at push time, and a pushed tag can no longer be moved.
- **Fedora packaging moved to [mineiro/rpms](https://github.com/mineiro/rpms)**
  and the COPR project is now `mineiro/rpms`. Enable that instead of
  `mineiro/gaffer`; packages there are built from release tarballs rather than
  from every commit. Nothing about gaffer itself changes.
- Dependencies refreshed: mdns-sd 0.21.0, zbus 5.19.0, clap 4.6.6 and
  futures-util 0.3.34. mdns-sd 0.21.0 caps outgoing packets at the Ethernet MTU
  per RFC 6762 section 17; gaffer only browses and re-probes, so it sends
  queries and never a response, and the cap does not change what it puts on the
  wire.
- Every pull request and a weekly run now check the dependency tree against the
  RustSec advisory database, and the tagged tree is audited again at release
  time — an advisory published after the last run would otherwise ship with
  every check green.

## [0.2.0] — 2026-08-08

### Added

- **Gangs.** Link lights so they move as one instrument, keeping the brightness
  difference they had — set a fill seven points below the key and it stays seven
  points below wherever you take the pair. Colour temperature and power mirror;
  only brightness carries the offset. The first lamp named is the gang's
  reference, so `gaffer link left right` reads "link right onto left".
- **Scenes.** Save and restore the whole desk with `gaffer scene save <name>`
  and `gaffer scene <name>`. A scene stores topology plus values — each gang's
  members, mode, offsets and level — rather than a flat list of brightnesses, so
  restoring one brings back the instrument and not just the numbers.
- **`Manager1.BuildId`** and `gaffer version`, reporting which build is actually
  running. The CLI now warns when the daemon was upgraded but never restarted,
  which a package upgrade cannot do for a user service.
- D-Bus: `Link`, `Unlink`, `SetLinkMode`, `SetLinkLevel`, `LinkLevel`,
  `SaveScene`, `ApplyScene`, `DeleteScene`, and the `Links`, `Scenes` and
  `BuildId` properties. **All additive** — nothing existing changed shape, so a
  client written against 0.1.0 keeps working.
- Gangs and scenes persist in `~/.config/gaffer/config.toml`, which is meant to
  stay readable and hand-editable.

### Fixed

- `Manager1.Link` sorted the lights it matched before building the gang, so the
  gang's reference became whichever member had the lowest hardware id rather
  than the one named first. `link a b` and `link b a` produced the same gang,
  and a later mirror snapped onto the wrong lamp.
- `Manager1.Links` never emitted `PropertiesChanged` despite being annotated
  `emits-change`, and a burst of unrelated identity properties was emitted in
  its place — so clients were inferring gang changes from a signal that claimed
  Name, Model, Firmware and Address had changed when none had.

### Security

- Requires **mdns-sd 0.20.3**, which fixes a panic in the mDNS packet write
  path. gaffer stores names received over mDNS and hands them back to the
  library to encode when re-probing, so a single hostile advertisement on the
  local network could take discovery down until the daemon was restarted.

## [0.1.0] — 2026-07-25

Initial version: mDNS discovery of Elgato Key Lights, the D-Bus session-bus
API, the `gaffer` CLI, the Waybar module, and Fedora packaging via COPR.

Never tagged — it shipped only as COPR snapshots.

[Unreleased]: https://github.com/mineiro/gaffer/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/mineiro/gaffer/releases/tag/v0.2.1
[0.2.0]: https://github.com/mineiro/gaffer/releases/tag/v0.2.0
