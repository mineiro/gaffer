# Security Policy

## Reporting a vulnerability

Please use GitHub's [private vulnerability reporting][pvr] on this repository
rather than opening a public issue. That keeps the details out of the open until
there is something to upgrade to.

[pvr]: https://github.com/mineiro/gaffer/security/advisories/new

gaffer is a personal project maintained by one person. Expect an initial reply
within a week or so, and please assume good faith about the pace rather than
about the priority.

## What gaffer is

An unprivileged daemon running inside a desktop login session. It holds no
credentials, opens no listening TCP socket, and needs no elevated privileges.
It speaks mDNS on the local link and plain HTTP to lights on the LAN.

## Known and accepted properties

These are design consequences, not oversights. Reports about them are welcome as
discussion, but they will not be treated as vulnerabilities.

**Any process in your session can control the lights.** The D-Bus session bus is
the trust boundary. A process already running as you could talk to the lights
directly over the network anyway, so there is no privilege to escalate.

**Discovery is unauthenticated.** mDNS has no notion of identity, so anything on
your local link can advertise `_elg._tcp.local` and cause gafferd to send
Elgato-shaped HTTP requests to a host and port of its choosing. This is inherent
to zero-configuration discovery — Avahi and every other mDNS client share it —
and an attacker in that position can already reach the lights unaided. gaffer
does not restrict discovered addresses to private ranges, because doing so would
break legitimate setups (a light reachable over a VPN, for instance) for no real
gain.

**Light traffic is unencrypted.** The Key Light HTTP API offers no TLS and no
authentication. Anyone on the LAN can read and change light state regardless of
gaffer. No TLS stack is linked into the binaries at all.

## What would be a vulnerability

- Anything letting a process *outside* your session control the lights or reach
  the daemon.
- Memory corruption, or a panic reachable from network input — a panic in a
  session service is a denial of service. The daemon contains no `unsafe`, and
  no `unwrap`/`expect`/`panic` outside tests; keeping it that way is the point.
- The daemon acting on discovered data in a way that goes beyond issuing
  Elgato-protocol requests to the advertised endpoint.
- Anything that leaks the D-Bus session address, the GitHub Actions token, or
  other credentials.

## Supported versions

The tip of `main` is what gets fixed. There are no maintained release branches.
