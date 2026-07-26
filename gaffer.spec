%global forgeurl https://github.com/mineiro/gaffer

Name:           gaffer
Version:        0.1.0
Release:        1%{?dist}
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
* Sat Jul 25 2026 Jose Tiburcio Ribeiro Netto <jnetto@mineiro.dev> - 0.1.0-1
- Initial package
