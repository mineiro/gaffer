# gaffer

> The gaffer is the crew chief who runs the lights on a film set.

A small Linux daemon that discovers Elgato Key Lights on your network and puts
them on the D-Bus session bus, plus a CLI that makes them bindable to a key.

```console
$ gaffer list
NAME                    STATE    BRIGHT    TEMP  ADDRESS
Elgato Key Light Left   on          42%   4200K  http://192.168.1.63:9123
Elgato Key Light Right  on          42%   4200K  http://192.168.1.137:9123
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

Needs Rust 1.88+ and a session bus. No system libraries.

```sh
make && make install-user    # → ~/.local/bin, ~/.config/systemd/user, ~/.local/share/dbus-1
```

Distro packages use `make DESTDIR=… PREFIX=/usr install`, which stages into a
buildroot and touches nothing live.

There is nothing to start. gaffer is **D-Bus activated**, so the first command
launches it, and the activation file defers to systemd so the daemon gets a
proper cgroup, journal capture and restart policy.

Optionally keep it warm from login, so discovery has already settled by the time
you press a key:

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
gaffer list --json           # for scripts
```

`set` changes exactly what you name — it never implicitly powers a light on.
Use `on` when you mean on.

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

```sh
busctl --user tree io.mineiro.gaffer
busctl --user set-property io.mineiro.gaffer \
    /io/mineiro/gaffer/lights/all io.mineiro.gaffer.Light1 Brightness y 42
```

## Hardware

Elgato Key Light, Key Light Air, Ring Light — anything advertising
`_elg._tcp.local` with the Key Light HTTP API on port 9123. Lights are keyed on
the MAC from their mDNS TXT record, so renaming one in Elgato's app does not
make it reappear as a stranger.

## Status

Discovery, control, grouping, the D-Bus API, the CLI and panel integration all
work and are verified against real hardware. Scenes (`gaffer scene "on camera"`)
and camera-follow — turning the key light on when the webcam goes live — are the
next pieces, and are why this is a daemon rather than a script.

## Licence

GPL-3.0-or-later.
