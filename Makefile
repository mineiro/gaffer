# gaffer — build and install.
#
# Two audiences, two entry points:
#
#   make && make install-user     you, installing into ~/.local
#   make && make DESTDIR=... install    a distro package, staging into a buildroot
#
# The packaging path is the default one, because getting it wrong is the
# expensive mistake: `install` must never touch the live system, never run
# systemctl, and never bake DESTDIR into an installed file.

DESTDIR ?=
PREFIX  ?= /usr/local
BINDIR  ?= $(PREFIX)/bin
UNITDIR ?= $(PREFIX)/lib/systemd/user
DBUSDIR ?= $(PREFIX)/share/dbus-1/services

CARGO   ?= cargo

.PHONY: all build test check install install-user uninstall uninstall-user clean vendor

all: build

build:
	$(CARGO) build --release

test:
	$(CARGO) test --workspace

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

# Staged install. Deliberately does *not* depend on `build`: a packager compiles
# in its own step, with its own flags, and re-entering cargo here would either
# be wasted work or silently rebuild with the wrong ones.
#
# Note the @BINDIR@ substitution uses $(BINDIR), never $(DESTDIR)$(BINDIR) —
# DESTDIR is a staging prefix that must not survive into an installed file.
install:
	install -Dm755 target/release/gafferd $(DESTDIR)$(BINDIR)/gafferd
	install -Dm755 target/release/gaffer  $(DESTDIR)$(BINDIR)/gaffer
	install -d $(DESTDIR)$(UNITDIR) $(DESTDIR)$(DBUSDIR)
	sed 's|@BINDIR@|$(BINDIR)|g' data/gaffer.service.in \
		> $(DESTDIR)$(UNITDIR)/gaffer.service
	sed 's|@BINDIR@|$(BINDIR)|g' data/io.mineiro.gaffer.service.in \
		> $(DESTDIR)$(DBUSDIR)/io.mineiro.gaffer.service

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/gafferd $(DESTDIR)$(BINDIR)/gaffer
	rm -f $(DESTDIR)$(UNITDIR)/gaffer.service
	rm -f $(DESTDIR)$(DBUSDIR)/io.mineiro.gaffer.service

# Convenience install for a single user. Target-specific variables propagate to
# prerequisites, so `install` below picks up these paths.
install-user: PREFIX  = $(HOME)/.local
install-user: UNITDIR = $(HOME)/.config/systemd/user
install-user: build install
	systemctl --user daemon-reload
	@echo
	@echo "installed. gaffer starts on demand — just run:"
	@echo "    gaffer list"
	@echo
	@echo "to keep it warm from login (optional):"
	@echo "    systemctl --user enable --now gaffer.service"

uninstall-user: PREFIX  = $(HOME)/.local
uninstall-user: UNITDIR = $(HOME)/.config/systemd/user
uninstall-user:
	-systemctl --user disable --now gaffer.service
	$(MAKE) PREFIX=$(HOME)/.local UNITDIR=$(HOME)/.config/systemd/user uninstall
	systemctl --user daemon-reload

# Offline dependency tree for distro builds, which have no network in the
# build root. Ship the result alongside the source tarball.
vendor:
	$(CARGO) vendor --versioned-dirs vendor
	@echo "vendored. tar it up as Source1 and add .cargo/config.toml to the build."

clean:
	$(CARGO) clean
	rm -rf vendor
