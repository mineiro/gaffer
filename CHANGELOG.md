# Changelog

Notable changes to gaffer, newest first.

This file is the **canonical** record. The `%changelog` in `gaffer.spec` is a
derived summary for Fedora's benefit, and the GitHub release notes are
generated from the section below matching the tag — so this is the one place a
change needs to be written down.

Versions follow [semantic versioning](https://semver.org). While gaffer is
below 1.0 the minor slot carries features and the patch slot carries fixes.
The D-Bus API is a contract: every entry that touches it says whether the
change is additive, and so far all of them have been.

## [Unreleased]

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

[Unreleased]: https://github.com/mineiro/gaffer/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mineiro/gaffer/releases/tag/v0.2.0
