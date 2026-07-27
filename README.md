# gaffer

> The gaffer is the crew chief who runs the lights on a film set.

A small Linux daemon that discovers Elgato Key Lights on your network and puts
them on the D-Bus session bus, plus a CLI that makes them bindable to a key.

```console
$ gaffer list
NAME                    STATE    BRIGHT    TEMP  ADDRESS
Elgato Key Light Left   on          42%   4200K  http://192.0.2.10:9123
Elgato Key Light Right  on          42%   4200K  http://192.0.2.11:9123
All Lights              on          42%   4200K  2 of 2 online

$ gaffer set left 42% 4200k
$ gaffer set all -10%
$ gaffer toggle
```

## Why a daemon

Because light state should outlive a window. A GUI that owns discovery loses
everything when you close it, and every new client — a hotkey, a panel module,
OBS, a future app — has to re-implement mDNS and the protocol from scratch.

gaffer owns the state once. Everything else is a thin client.

## Install

### Fedora

```sh
sudo dnf copr enable mineiro/gaffer
sudo dnf install gaffer
```

Fedora 43, 44 and rawhide, on x86_64 and aarch64.

Builds track `main`, and every commit produces a distinct version — so use
`--refresh` when you upgrade:

```sh
sudo dnf --refresh upgrade gaffer
```

Without it dnf serves cached repository metadata, which for a repo that
rebuilds on every push routinely means being told there is nothing to do while
a newer build sits in the repo.

### NixOS

```nix
{
  inputs.gaffer.url = "github:mineiro/gaffer";

  # in your configuration:
  imports = [ inputs.gaffer.nixosModules.default ];
  services.gaffer.enable = true;
}
```

`services.gaffer.autoStart = true` keeps the daemon resident from login rather
than activating on demand; `openFirewall` (on by default) permits inbound mDNS.

### From source

Needs Rust 1.88+ and a D-Bus session bus. Beyond glibc it links nothing — no
GUI toolkit, no Avahi, no OpenSSL.

```sh
make && make install-user    # → ~/.local/bin, ~/.config/systemd/user, ~/.local/share/dbus-1
make uninstall-user          # removes all of it again
```

Packagers want `make DESTDIR=… PREFIX=/usr install`, which stages into a
buildroot and touches nothing live.

### Running it

There is nothing to start. gaffer is **D-Bus activated**, so the first command
launches it, and the activation file defers to systemd so the daemon gets a
proper cgroup, journal capture and restart policy.

Activation returns as soon as the daemon claims its bus name, which is *before*
mDNS has found anything — so the very first command after a cold start can
report no lights. If something is always watching, such as a status-bar module,
keep the daemon resident instead and discovery will have settled long before
anything asks:

```sh
systemctl --user enable --now gaffer.service
```

Logs: `journalctl --user -u gaffer -f`. Raise verbosity with
`systemctl --user set-environment GAFFER_LOG=debug`.

## Using it

Value suffixes carry the unit, so order never matters and everything composes:

| Token | Meaning |
|-------|---------|
| `42%` | set brightness |
| `+10%` / `-10%` | adjust brightness, clamped |
| `4200k` | set colour temperature (2900–7000K) |
| `-200k` | warm by 200K |
| `on` `off` `toggle` | power |

A **selector** is a name substring (`left`), a hardware id, or `all` — and it
defaults to `all`, so `gaffer on 60%` addresses everything.

```sh
gaffer set left 42% 4200k    # one light, absolute
gaffer set all -10%          # dim everything by 10
gaffer on right 80%          # power on and set, in one command
gaffer identify left         # blink it, to tell which is which
gaffer list --json           # for scripts; carries gang membership
```

`set` changes exactly what you name — it never implicitly powers a light on.
Use `on` when you mean on.

### Ganging lights

```sh
gaffer link left right    # they now move as one instrument
gaffer set left +10%      # both rise, keeping their difference
gaffer link --mirror a b  # snap b onto a instead of keeping the difference
gaffer unlink left        # break the gang
```

`link` learns the brightness difference the lamps have *now*, so a key/fill
ratio survives — set your fill 7 points below the key and it stays 7 points
below wherever you take the pair. Moving *either* lamp moves both. Colour
temperature and power mirror; only brightness carries the offset. A pair that
already matches learns an offset of zero, which behaves exactly like a mirror,
so the friendly default is also the non-destructive one.

Gangs live in the daemon, so a compositor hotkey moves the pair with no panel
running, and they survive a restart — they are stored in
`~/.config/gaffer/config.toml`, which is meant to be readable.

### Hyprland

```conf
bind = SUPER, K, exec, gaffer toggle
bind = SUPER SHIFT, K, exec, gaffer set all +10%
bind = SUPER CTRL,  K, exec, gaffer set all -10%
```

Global hotkeys need no portal, because the binding target is a program.

### Waybar

`gaffer watch --waybar` prints one JSON object per line, forever — which is
exactly Waybar's `custom/` module protocol. On Wayland there is no system tray,
so this *is* the panel integration.

```jsonc
"custom/gaffer": {
    "exec": "gaffer watch --waybar",
    "return-type": "json",
    "on-click": "gaffer toggle",
    "on-scroll-up": "gaffer set all +5%",
    "on-scroll-down": "gaffer set all -5%"
}
```

Output carries `text`, `percentage`, a `class` of `on`/`off`/`offline`/`empty`
for styling, and a per-light `tooltip`.

## The D-Bus API

Any language that speaks D-Bus is a first-class client — GTK via `Gio.DBus`, Qt
via `QtDBus`, Rust via `zbus`. Flatpak apps need one line:
`--talk-name=io.mineiro.gaffer`.

```text
io.mineiro.gaffer                      bus name
 /io/mineiro/gaffer                    Manager1 + org.freedesktop.DBus.ObjectManager
   ├── /lights/00005E005301            Light1
   └── /lights/all                     Light1   (the group, as a light)
```

The group implements the **same interface** as a single light, so controlling
everything at once needs no special case. `ObjectManager` gives hotplug-aware
enumeration for free, and writable `On`/`Brightness`/`Kelvin` properties emit
`PropertiesChanged` when the hardware confirms.

Treat `ObjectManager` as a subscription, not a one-shot query: read
`GetManagedObjects` **and** stay on `InterfacesAdded`/`InterfacesRemoved`, or a
client started before discovery finishes will show an empty list forever.

The exact contract is committed as `crates/gafferd/api/*.xml` and pinned by a
test, so a property cannot be renamed or retyped without a failing build and a
visible diff. Note the types: `Brightness` is `y` (byte), `Kelvin` is `q`
(uint16), `OnlineCount` is `u` (uint32).

```sh
busctl --user tree io.mineiro.gaffer
busctl --user set-property io.mineiro.gaffer \
    /io/mineiro/gaffer/lights/all io.mineiro.gaffer.Light1 Brightness y 42
```

## Hardware

**Verified:** Elgato Key Light MK.2 (`20GAK9902`).

**Should work, untested:** Key Light, Key Light Air, Ring Light, Light Strip —
anything advertising `_elg._tcp.local` and serving the Key Light HTTP API on
port 9123. They share one protocol, so the odds are good, but nobody has run
gaffer against them. Reports either way are welcome.

Lights are keyed on the MAC from their mDNS TXT record, so renaming one in
Elgato's app does not make it reappear as a stranger.

## Troubleshooting

**No lights found.** Confirm the network sees them at all:

```sh
avahi-browse -rtp _elg._tcp
```

If that comes up empty, the problem is below gaffer. Lights must be on the same
layer-2 network — mDNS does not cross subnets or most guest/IoT VLAN isolation —
and inbound mDNS must be permitted. Fedora Workstation allows it by default;
check with `firewall-cmd --list-services | grep mdns` and add it with
`firewall-cmd --add-service=mdns --permanent` if it is missing.

**Commands fail with "name not activatable."** The D-Bus activation file did not
install. Re-run `make install-user` and check
`~/.local/share/dbus-1/services/io.mineiro.gaffer.service` exists.

**A light shows `offline`.** gaffer discovered it but cannot reach its HTTP API.
`gaffer list` prints the transport error next to the address; the daemon retries
every 15 s and recovers on its own once the light is reachable.

For anything else, `journalctl --user -u gaffer -f` with
`systemctl --user set-environment GAFFER_LOG=debug`.

## Scope

gaffer controls LAN studio lights from a Linux desktop session, and deliberately
stops there. Not planned: Windows or macOS, Elgato's cloud or mobile app,
Stream Deck integration, or Bluetooth/Zigbee bulbs. Support for other LAN light
protocols is plausible — the discovery, backend and device layers are separate
seams — but nothing beyond Elgato is implemented today.

## Status

Working today, verified against real hardware: discovery, control, grouping,
the D-Bus API, the CLI, and the Waybar module.

**Not implemented yet** — listed so nobody goes looking for them:

- `gaffer scene "on camera"` — named scenes saved to TOML.
- Camera-follow — turning the key lights on when the webcam goes live, via
  PipeWire. This is the feature that makes a daemon worth having over a script.
- `gaffer link left right --offset` — a leader/follower relationship between
  lights. Deferred because naive two-way linking oscillates; links have to be
  directional and propagation has to be marked so it never round-trips.

## Licence

GPL-3.0-or-later.
