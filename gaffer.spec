%global forgeurl https://github.com/mineiro/gaffer

# Snapshot builds (see .copr/Makefile) override this with
#
#     0.<HEAD commit timestamp>.git<short sha>
#
# so that every commit produces a distinct NVR — dnf keys on NEVRA, and a
# repository that rebuilds the same 0.2.0-1 forever looks frozen to clients.
#
# The leading 0 is load-bearing: RPM compares release segment by segment, so
# 0.<anything> sorts *below* a plain 1, and the first tagged release therefore
# supersedes every snapshot that preceded it. The obvious 1.<date>git<sha> form
# would invert that and make the real release look older than the snapshots.
#
# .copr/Makefile rewrites the line below when it assembles the SRPM, so the
# value travels *inside* the source package. Passing it as `rpmbuild --define`
# is not enough: that only affects SRPM creation, and the binary RPM is built
# by rebuilding the SRPM later, where the macro is gone and the release would
# silently revert to 1.
#
# Left at 1 here, so a local rpmbuild or rpmlint behaves like a release build.
%global snaprel 1

Name:           gaffer
Version:        0.2.0
Release:        %{snaprel}%{?dist}
Summary:        Daemon and CLI for controlling Elgato Key Lights

# gaffer's own code is GPL-3.0-or-later. Rust binaries statically link their
# whole dependency tree, so this is the *effective* licence of the shipped
# artefacts, with every "OR" in the tree already resolved to one choice:
#
#   GPL-3.0-or-later  gaffer itself
#   MIT               chosen for every dual/tri-licensed crate
#   Apache-2.0        unavoidable: sync_wrapper is Apache-2.0 only
#   Unicode-3.0       unavoidable: the ICU data crates
#
# Nothing here needs BSD-3-Clause, LLVM-exception or LGPL: each appears only as
# one branch of an OR whose MIT branch was taken instead.
#
# VERIFY after any dependency change against the LICENSE SUMMARY block that
# %%{cargo_license_summary} prints into the build log. Fedora treats a wrong
# License field as a review blocker.
License:        GPL-3.0-or-later AND MIT AND Apache-2.0 AND Unicode-3.0
URL:            %{forgeurl}
Source0:        %{forgeurl}/archive/v%{version}/%{name}-%{version}.tar.gz
# Produced by `make vendor`; build roots have no network access.
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo-rpm-macros >= 24
BuildRequires:  systemd-rpm-macros
BuildRequires:  make

# gaffer is a *session* service: it needs a user D-Bus session bus, and is
# activated on demand rather than being a system daemon.
Requires:       dbus-common
Requires:       systemd

ExclusiveArch:  %{rust_arches}

%description
gaffer discovers Elgato Key Lights on the local network over mDNS and owns
their state, exposing them on the D-Bus session bus so that any desktop client
— a panel module, a hotkey, a GTK or Qt application — controls the same lights
without re-implementing the protocol.

The package installs two programs: gafferd, the daemon, which is started on
demand through D-Bus activation; and gaffer, a command-line client suitable for
binding to compositor hotkeys or driving a status-bar module.

%prep
%autosetup -n %{name}-%{version} -p1 -a1
%cargo_prep -v vendor

%build
# Bake the exact NVR into the daemon, readable at runtime as
# Manager1.BuildId and `gafferd --version`. The vendored tarball has no git
# metadata, so without this a packaged build could only report the crate
# version — which is identical across every snapshot between two releases, and
# so cannot answer "is the running daemon the one I just installed?". That
# matters because RPM scriptlets run as root and cannot restart a *user*
# service: an upgrade replaces the binary while the old process keeps running.
export GAFFER_BUILD_ID="%{version}-%{release}"
%cargo_build
# Record what is statically linked in, for the licence audit trail.
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies
%{cargo_vendor_manifest}

%install
# ARTIFACTDIR points at the profile %%cargo_build actually used, which is not
# target/release. UNITDIR is a *user* unit directory: gaffer is per-session.
%make_install \
    PREFIX=%{_prefix} \
    BINDIR=%{_bindir} \
    UNITDIR=%{_userunitdir} \
    DBUSDIR=%{_datadir}/dbus-1/services \
    ARTIFACTDIR=target/rpm

%check
# The whole suite is hermetic: no hardware, no network, no session bus.
%cargo_test

%post
%systemd_user_post %{name}.service

%preun
%systemd_user_preun %{name}.service

%postun
%systemd_user_postun %{name}.service

%files
%license COPYING
%license LICENSE.dependencies
%doc README.md
%{_bindir}/gaffer
%{_bindir}/gafferd
%{_userunitdir}/%{name}.service
%{_datadir}/dbus-1/services/io.mineiro.gaffer.service

%changelog
* Sat Aug 08 2026 Jose Tiburcio Ribeiro Netto <jnetto@mineiro.io> - 0.2.0-1
- Gangs: link lights so they move as one instrument, keeping the brightness
  difference they had. Offset and mirror modes; the first lamp named leads.
- Scenes: save and restore the whole desk, storing gangs and their spacing
  rather than a flat list of brightnesses.
- Manager1 gains Link, Unlink, SetLinkMode, SetLinkLevel, LinkLevel,
  SaveScene, ApplyScene, DeleteScene, and the Links, Scenes and BuildId
  properties. All additive; nothing existing changed shape.
- BuildId and `gaffer version` report which build is actually running, and
  the CLI warns when the daemon was upgraded but never restarted.
- Require mdns-sd 0.20.3, which fixes a panic in the mDNS packet write path
  reachable from a hostile advertisement on the local network.

* Sat Jul 25 2026 Jose Tiburcio Ribeiro Netto <jnetto@mineiro.dev> - 0.1.0-1
- Initial package
