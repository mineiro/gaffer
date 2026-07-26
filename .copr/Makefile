# COPR entry point.
#
# COPR's "Make SRPM" build method runs `make -f .copr/Makefile srpm outdir=<dir>`
# inside a container that has network access — which matters, because this is
# the only stage where `cargo vendor` can reach crates.io. The resulting SRPM
# carries the whole dependency tree, so the actual RPM build stays offline as
# Fedora requires.

srpm:
	dnf -y install cargo rpm-build rpmdevtools git
	$(MAKE) -f .copr/Makefile _srpm outdir=$(outdir)

_srpm:
	@set -eu; \
	name=gaffer; \
	version=$$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2); \
	stage=$$(mktemp -d); \
	echo "building $$name-$$version"; \
	mkdir -p "$$stage/$$name-$$version"; \
	git archive --format=tar HEAD | tar -x -C "$$stage/$$name-$$version"; \
	tar czf "$$stage/$$name-$$version.tar.gz" -C "$$stage" "$$name-$$version"; \
	cargo vendor --versioned-dirs "$$stage/vendor" >/dev/null; \
	tar cJf "$$stage/$$name-$$version-vendor.tar.xz" -C "$$stage" vendor; \
	rpmdev-setuptree; \
	cp "$$stage/$$name-$$version.tar.gz" "$$stage/$$name-$$version-vendor.tar.xz" \
		"$$(rpm -E %{_sourcedir})/"; \
	mkdir -p "$(outdir)"; \
	rpmbuild -bs gaffer.spec \
		--define "_srcrpmdir $(outdir)" \
		--define "_disable_source_fetch 1"; \
	rm -rf "$$stage"

.PHONY: srpm _srpm
