# Repository Guidelines

gaffer is a Rust session daemon that discovers and controls Elgato Key Lights on
the LAN, and exposes them on the D-Bus session bus. It replaces two earlier
attempts — **`luz`** (GTK4/C, `~/Projects/luz`) and **`Photon`** (Qt6/QML,
`~/Code/Photon`). Both are **reference material only**: read them for protocol
knowledge, never add to them.

The point of the rewrite is not the language. luz and Photon are GUIs that *own*
discovery, so state dies with the window and every new client re-derives the
protocol. gaffer owns the state; UIs become thin D-Bus clients.

## Project Structure & Module Organization

| Path | Crate | Responsibility |
|------|-------|----------------|
| `crates/gaffer-core/` | `gaffer-core` | Pure logic, **zero dependencies**: mired conversion, `LightState`/`StatePatch`, unit parsing, selectors, group aggregation |
| `crates/gafferd/` | `gafferd` | The daemon: mDNS discovery, Elgato HTTP backend, reconciler, D-Bus service |
| `crates/gaffer/` | `gaffer` | The CLI: a D-Bus client and the adapter to the shell world |
| `data/` | — | systemd user unit and D-Bus activation file (`.in` templates) |

`gaffer-core` performs **no I/O** and must stay that way. It is where all the
interesting behaviour is tested without hardware or a network. Anything needing
a socket, a clock, or a config file belongs in `gafferd`.

## Architecture

Three seams, inherited from Photon and still the right decomposition:

- **`discovery`** — *finds* lights. Wraps `mdns-sd`; emits `Discovered`.
- **`elgato`** — *speaks* the protocol. Stateless functions over `reqwest`.
- **`world`** — *remembers*. `World` owns every `LightRecord`.

`supervisor` is the reconcile loop and **the only place that touches hardware**.

### Desired vs reported

Every light carries two states:

- `desired` — what gaffer is trying to make true. Clients read this, so a slider
  drag or a keybind feels instant.
- `reported` — what the hardware last said. **`None` is the definition of
  offline**, rather than a separate flag that can drift out of sync.

Commands set `desired` and mark the light dirty. The supervisor pushes, then
adopts the echo. On refresh, `desired` adopts `reported` *only when no push is
owed* — mid-flight, `desired` is the more recent intent.

### One writer, one emitter

D-Bus interface methods mutate `World` synchronously (optimistically) and then
send `Request::Applied` to the supervisor, which owes both the network push and
the `PropertiesChanged` signals. **Do not emit signals from interface methods.**

This shape exists because of a real bug: commands arriving through
`Manager1.SetState` mutated state but emitted nothing, because zbus only
auto-emits for property *setters* — which the CLI never used. Watching clients
silently saw nothing. Keeping one emitter means there is a single place where a
change can fail to be announced.

Related invariant: **property setters take `&self`, never `&mut self`.** A
`&mut self` setter holds the interface write lock across the reply and would
deadlock against the reconciler emitting on that same object. Never hold a
`World` guard across an object-server call either.

## Build, Test, and Development Commands

Requires Rust 1.88+ (edition 2024; let-chains set the floor). No system libraries beyond a session bus.

- `cargo build --workspace` — build everything.
- `cargo test --workspace` — run all tests; needs no hardware or network.
- `make install-user` — install to `~/.local`, wire up systemd + D-Bus activation.
- `make DESTDIR=/tmp/root PREFIX=/usr install` — staged install, as a package does.
- `GAFFER_LOG=debug ./target/debug/gafferd` — run the daemon in the foreground.
- `busctl --user tree io.mineiro.gaffer` — inspect the published object tree.
- `dbus-monitor --session "type='signal',interface='org.freedesktop.DBus.Properties'"`
  — watch what clients actually receive. Indispensable when signals go missing.

Builds must stay **warning-clean**. Delete dead code rather than annotating it.

## D-Bus API

```text
io.mineiro.gaffer                      bus name
 /io/mineiro/gaffer                    Manager1 + org.freedesktop.DBus.ObjectManager
   ├── /lights/00005E005301            Light1
   └── /lights/all                     Light1   (the group, as a light)
```

`Light1` — `Id Name Model Firmware Address Online OnlineCount LastError` (ro),
`On Brightness Kelvin` (rw), `MinKelvin MaxKelvin` (const), `Apply(as)`,
`Identify()`, `Refresh()`.

`Manager1` — `SetState(s, as) → as`, `Identify(s) → as`, `Rescan()`, plus
`Version LightCount OnlineCount`.

Two deliberate choices: **the group implements the same interface as a light**,
so clients need no special case; and `ObjectManager` provides hotplug-aware
enumeration as standard machinery, so clients get `InterfacesAdded/Removed` free.

Object paths are the hardware id with separators stripped and upper-cased, so
the same light always lands on the same path however its id is punctuated.

## Device Protocol Reference — Elgato Key Light

Discovered via mDNS `_elg._tcp.local`. HTTP on **port 9123**, 5 s timeout.

- `GET|PUT /elgato/lights` → `{"numberOfLights":1,"lights":[{"on":1,"brightness":75,"temperature":213}]}`
- `GET /elgato/accessory-info` → `displayName`, `productName`, `firmwareVersion`
- `POST /elgato/identify` — empty body; physically blinks the unit

`brightness` is **0–100**. `temperature` is **mireds 143–344, not Kelvin**, and
the scale is inverted (143 ≈ 7000K cool, 344 ≈ 2900K warm):

```
mired  = clamp((1000000 + kelvin/2) / kelvin, 143, 344)   // kelvin clamped 2900..7000
kelvin = clamp((1000000 + mired/2)  / mired,  2900, 7000)
```

The integer rounding is load-bearing — it is what the previous two
implementations sent, and it is pinned by tests.

**The mired grid is coarser than Kelvin.** One step spans ~50K at the cool end,
so 4200K and 4202K are *the same hardware setting*. The reconciler uses
`color::same_mired` to keep showing what was asked for rather than the
reciprocal-rounding artifact in the echo. `mired → kelvin → mired` is exactly
stable (pinned by an exhaustive test), which is what stops state oscillating.

**Key devices on the TXT `id=` field**, which is the MAC — not the mDNS instance
name. luz and Photon both keyed on the instance name, so renaming a light in
Elgato's app made it appear as a second device. `md=` also gives the model
without an `accessory-info` round trip.

Cadence: 12 s mDNS re-query (handled by `mdns-sd`), 15 s state refresh, ~90 ms
debounce coalescing rapid changes into one PUT.

## Grouping

Elgato firmware has **no grouping**; it is entirely client-side, and is two
concerns: **fan-out** (send to every member — the reconciler) and **aggregation**
(collapse to one displayed state — `core::group`, pure).

Aggregation: power is on only if *every online* member is on; brightness and
temperature are averaged over online members.

The `all` selector uses **group semantics** — the patch resolves once against the
aggregate and the resulting absolute state fans out. That is what makes
`gaffer toggle` do the obvious thing with one light on and one off, and it keeps
the selector consistent with `/lights/all`. A named selector resolves per light,
so `gaffer set left +10%` is relative to *that* light.

## Coding Style & Naming Conventions

Standard `rustfmt`; 4-space indent, 100-column lines. Prefer `?` with
`anyhow::Context` in the binaries and typed errors in `gaffer-core`.

Comments explain **why**, not what — the invariant, the hazard, or the reason a
value is what it is. A comment restating the code is noise.

Tests are named as sentences describing the property under test
(`a_member_going_offline_changes_the_group`), not `test_foo`.

## Testing Guidelines

Tests **must not require hardware or a network**. Everything interesting is
reachable without either, because the logic lives in `gaffer-core` and the
protocol layer is pure functions over JSON.

Patterns worth continuing:

- Pin the protocol with literal wire samples, including the malformed cases —
  a missing `on` field must not read as "turn off".
- Assert convergence, not just correctness: `a_state_pushed_then_echoed_round_trips_without_drift`
  exists because a value that shifts every cycle would oscillate forever.
- Test the *absence* of work: a command that changes nothing must emit no signal
  and generate no HTTP traffic.

Hardware smoke test, when hardware is present:

```sh
GAFFER_LOG=info ./target/debug/gafferd &
gaffer list && gaffer set left 42% 4200k && gaffer toggle && gaffer watch
```

Offline detection is unit-tested but is only exercised for real by unplugging a
light: `gaffer list` should show `offline` within ~20 s while the other stays up.

## Adding a Device Backend

The Elgato backend is currently a module rather than a trait, because there is
one backend and inventing an abstraction for one implementation is guesswork.
When a second arrives (Hue, Nanoleaf), introduce the seam then: discovery
already emits a generic `Discovered`, and `World`/`Light1` know nothing about
Elgato beyond the module they call.

Do not commit protocol code you cannot exercise against real hardware.

## Packaging

Packaging lives in-tree, deliberately: it cannot drift out of lockstep with the
code, and a Nix flake has to sit at the repo root anyway. If gaffer ever reaches
official Fedora or Debian, those projects take packaging into their own dist-git
and the split happens on its own.

- `gaffer.spec` — Fedora/COPR.
- `.copr/Makefile` — COPR's "Make SRPM" entry point. This is the only build
  stage with network access, so it is where `cargo vendor` runs; the RPM build
  itself is offline, as Fedora requires.

**Pushing to `main` builds a package.** A GitHub webhook posts to COPR, which
rebuilds all six chroots — roughly seven minutes, including for docs-only
commits. Note that `copr-cli add-package-scm --webhook-rebuild on` only makes
COPR *accept* such a request; the webhook on the GitHub side is what sends it,
and the two are configured separately. If pushes stop producing builds, check
the delivery log under the repository's webhook settings before suspecting the
spec.

The webhook deliberately carries its token in the URL path, which is how COPR
authenticates it; GitHub's separate "Secret" field must be left empty, since
COPR does not verify the `X-Hub-Signature-256` header that field produces.

Four things that will silently break a package if changed carelessly:

- **`make install` must stay `DESTDIR`-safe.** No `systemctl`, no writes outside
  `$(DESTDIR)`, and no dependency on `build` — packagers compile separately with
  their own flags.
- **`@BINDIR@` substitutes `$(BINDIR)`, never `$(DESTDIR)$(BINDIR)`.** Baking the
  staging root into a unit file ships a service pointing at a build-machine path.
- **`ARTIFACTDIR` exists because `%cargo_build` emits to `target/rpm`,** not
  `target/release`. The spec overrides it.
- **The `License:` field covers the whole statically-linked tree,** not just
  gaffer. It is not auto-generated — after any dependency change, check it
  against `%{cargo_license_summary}` in the build log.

Units install to `%{_userunitdir}` (`/usr/lib/systemd/user`), not the system
unit directory. gaffer is per-session and D-Bus activated; it must never be
enabled as a system service.

## The D-Bus API is a Contract

`crates/gafferd/api/Light1.xml` and `Manager1.xml` are the published shape of
the interface, pinned by tests in `dbus.rs`. Clients bind to it — the CLI, a
status-bar module, anything written since — and nothing else in the build
notices when a property is renamed or its signature changes.

Adding a property or method is free. Renaming, removing or retyping one fails
the build. When that is intended:

```sh
cargo test -p gafferd -- --ignored regenerate_api_snapshots
```

Read the resulting diff, and say in the commit message what clients must change.
Doc comments are stripped before comparison, so rewording one is not a failure.

Keep every exposed type fixed-width. A `usize` would introspect as `u` on
32-bit and `t` on 64-bit, so the same commit would publish two different
contracts across the aarch64 and x86_64 chroots.

## Nix

`flake.nix` exposes the package, a dev shell, a NixOS module and a VM check.
Nix does not have to be installed on the host — the Makefile wraps a rootless
container with a persistent `/nix` volume:

```sh
make nix-build   # just the package
make nix-check   # package + the NixOS VM test
```

The VM check is the only test here that boots a machine, and it earns that:
D-Bus activation cannot be exercised in a build sandbox. It is also the fiddly
one, because gaffer is a *user* service — NixOS tests drive the guest as root,
so the script lingers a user manager with `loginctl enable-linger` and runs
everything as that user with `XDG_RUNTIME_DIR` set. Without it the test would
pass while checking nothing.

Assert behaviour, not paths, in that test: nixpkgs relocates user units from
`lib/systemd/user` to `share/systemd/user` during fixup, so a path assertion
tests nixpkgs' conventions rather than this module.

## Gangs (links)

`gaffer_core::link` is the whole model, and it is pure. A gang holds each
member's brightness **offset from a notional level**; moving any member
re-derives the level and every other member follows. Deriving the level from
the mover each time, rather than accumulating it, is what makes a *symmetric*
link safe — `resolving_is_idempotent_so_a_link_cannot_oscillate` pins that, and
it is the property the whole feature rests on.

Rules the design fixes, all of them tested:

- **Brightness offsets; temperature and power mirror.** The link editor shows
  `brt` in two columns and a single `tmp` spanning both.
- **Power gangs.** The pair collapses to one card because it is one
  instrument. Wanting to switch one lamp alone means the link is wrong for that
  moment — unlink, do not add a per-lamp power affordance.
- **Offset subsumes mirror**, so it is the default: a pair that already matches
  learns an offset of zero and behaves as a mirror. `Mirror` is the explicit,
  destructive choice because it overwrites one lamp's values with another's.
- **The level is not clamped, members are.** Pushing a gang to the ceiling
  compresses it and restores the spacing on the way down, rather than rewriting
  the offsets or leaving dead travel.
- **A lamp is in at most one gang**, which is what lets a panel draw one wire
  per port.
- **The first lamp named is the gang's reference** — `link a b` reads "link b
  onto a" — and it is the lamp whose values win a mirror. Stored explicitly,
  because "which lamp wins" must stay answerable after alt-drags have moved the
  offsets around. `Manager1.Links` carries it **by position**: the first member
  in each gang's list is the reference, which keeps that property's signature
  stable for clients already reading it.

Signalling rule: **emit exactly what changed.** Ganging alters no light property
and no counter, so `Request::LinksChanged` emits `Manager1.Links` and nothing
else. An earlier version emitted `meta` per lamp — claiming Name, Model,
Firmware and Address had changed when none had — and a client was left treating
that false burst as the bell for a gang appearing. If a mode change *moves*
lamps, those travel as an ordinary `Applied`, so brightness is announced through
the normal path.

Propagation happens once, inside `World::apply`, and never re-enters. Gangs are
deliberately *not* expanded when the reconciler adopts hardware state: links
propagate user intent, not observations, or a light changed from Elgato's own
app would fight the 15 s refresh.

## Security Model

gaffer runs unprivileged in the user's session and holds no credentials. Two
properties worth preserving:

- **The session bus is the trust boundary.** Any process in the user's session
  can control the lights, which is the same access it would have by talking to
  them directly over the LAN. There is no privilege to escalate.
- **Endpoints come from mDNS, which is unauthenticated.** Anything on the local
  link can advertise `_elg._tcp.local` and make gafferd issue Elgato-shaped HTTP
  requests to a host and port of its choosing. This is inherent to zero-config
  discovery — Avahi, luz and Photon all share it — and the attacker must already
  be on the link, where they could reach the lights unaided. Worth knowing before
  adding anything that acts on discovered data more consequentially than a
  brightness change.

The daemon contains no `unsafe`, and no `unwrap`/`expect`/`panic` outside tests:
a panic in a session service is a denial of service, so keep it that way. No TLS
stack is linked in, because the devices speak plain HTTP on the LAN.

**`crates/gafferd/src/discovery.rs` `resolve()` is the trust boundary.** Every
string arriving from mDNS or from a device's HTTP response is canonicalised or
sanitised there and in the `Io::Info` handler — never later, and never in a
renderer. Three invariants depend on it:

- `Endpoint` holds a parsed `IpAddr`, never a hostname. Do not reintroduce a
  hostname fallback: mDNS does not validate name characters, and a target such
  as `127.0.0.1:11434/api/pull#` injects a path into the request URL. Checking
  for a `.local.` suffix does *not* fix it — `#` and `?` end the authority
  first.
- Device text goes through `gaffer_core::sanitize` before it is stored, so no
  sink can receive a control character.
- Ids go through `gaffer_core::normalize_id`, which keeps punctuation variants
  of one MAC as one record and guarantees a valid, non-empty object path.

`elgato::client()` sets `Policy::none()`; a redirect-following client hands a
hostile device the same path control by another route.

## Commit & Pull Request Guidelines

Imperative subjects with a crate scope: `core:`, `daemon:`, `cli:`, `data:`,
`docs:` — e.g. `daemon: emit PropertiesChanged for SetState commands`. Keep
commits focused.

PRs should say what was verified and how. Call out protocol changes, D-Bus API
changes, and anything touching the reconcile loop explicitly — the last is where
a plausible-looking change quietly stops clients from seeing updates.
