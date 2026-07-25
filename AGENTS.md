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

Requires Rust 1.85+ (edition 2024). No system libraries beyond a session bus.

- `cargo build --workspace` — build everything.
- `cargo test --workspace` — run all tests; needs no hardware or network.
- `make install` — install to `~/.local`, wire up systemd + D-Bus activation.
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

## Commit & Pull Request Guidelines

Imperative subjects with a crate scope: `core:`, `daemon:`, `cli:`, `data:`,
`docs:` — e.g. `daemon: emit PropertiesChanged for SetState commands`. Keep
commits focused.

PRs should say what was verified and how. Call out protocol changes, D-Bus API
changes, and anything touching the reconcile loop explicitly — the last is where
a plausible-looking change quietly stops clients from seeing updates.
