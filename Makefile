# gaffer — user-scoped install.
#
# Everything lands under $HOME by design: gaffer is a session daemon that talks
# to lights on your LAN, so it has no business being a system service.

PREFIX  ?= $(HOME)/.local
BINDIR   = $(PREFIX)/bin
DBUSDIR  = $(PREFIX)/share/dbus-1/services
UNITDIR  = $(HOME)/.config/systemd/user

CARGO   ?= cargo

.PHONY: all build test check install uninstall clean

all: build

build:
	$(CARGO) build --release

test:
	$(CARGO) test --workspace

check:
	$(CARGO) fmt --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

install: build
	install -Dm755 target/release/gafferd $(BINDIR)/gafferd
	install -Dm755 target/release/gaffer  $(BINDIR)/gaffer
	mkdir -p $(UNITDIR) $(DBUSDIR)
	sed 's|@BINDIR@|$(BINDIR)|g' data/gaffer.service.in > $(UNITDIR)/gaffer.service
	sed 's|@BINDIR@|$(BINDIR)|g' data/io.mineiro.gaffer.service.in \
		> $(DBUSDIR)/io.mineiro.gaffer.service
	systemctl --user daemon-reload
	@echo
	@echo "installed. gaffer starts on demand — just run:"
	@echo "    gaffer list"
	@echo
	@echo "to keep it warm from login (optional):"
	@echo "    systemctl --user enable --now gaffer.service"

uninstall:
	-systemctl --user disable --now gaffer.service
	rm -f $(BINDIR)/gafferd $(BINDIR)/gaffer
	rm -f $(UNITDIR)/gaffer.service $(DBUSDIR)/io.mineiro.gaffer.service
	systemctl --user daemon-reload

clean:
	$(CARGO) clean
