# Convenience targets wrapping the workflows in README.md. Recipes use
# Unix-style commands (cp, mkdir -p), matching the shell the README's own
# "Developing without Factorio" section already assumes, so this needs a
# POSIX shell on PATH to run `make` from -- Git Bash's sh.exe covers it on
# Windows.
#
# Override any variable on the command line, e.g.:
#   make viewer FRAMES=frames_multi

MOD_INSTALL ?= $(APPDATA)/Factorio/mods/save-timelapse
FRAMES      ?= frames

# Read once at parse time rather than re-derived in every recipe that needs
# it: Make runs each recipe line in its own shell, so a shell variable set
# in `package`'s first line wouldn't survive to its later lines anyway.
VERSION      := $(shell sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' mod/info.json)
PACKAGE_NAME := save-timelapse_$(VERSION)

.PHONY: help build test test-lua run viewer drawcalls install-mod package clean

help:
	@echo "Targets:"
	@echo "  build        cargo build --release --workspace"
	@echo "  test         cargo test --workspace"
	@echo "  test-lua     run mod/tests/encode_test.lua (needs a lua interpreter on PATH)"
	@echo "  run          save-timelapse: interactive, asks what to do and opens the viewer"
	@echo "  viewer       open the interactive viewer on FRAMES"
	@echo "  drawcalls    headless draw-call report on FRAMES"
	@echo "  install-mod  copy mod/ into MOD_INSTALL (your Factorio mods folder)"
	@echo "  package      zip mod/ into dist/$(PACKAGE_NAME).zip, ready for the mod portal"
	@echo "  clean        cargo clean"
	@echo ""
	@echo "Variables (current value, override with make VAR=... target):"
	@echo "  MOD_INSTALL  $(MOD_INSTALL)"
	@echo "  FRAMES       $(FRAMES)"

build:
	cargo build --release --workspace

test:
	cargo test --workspace

# Separate from `test` on purpose: cargo test's whole point is needing
# nothing beyond the Rust toolchain, and that shouldn't start silently
# depending on a lua interpreter (see mod/tests/encode_test.lua).
test-lua:
	lua mod/tests/encode_test.lua

run:
	cargo run --release --bin save-timelapse

viewer:
	cargo run -p viewer --release --bin viewer -- $(FRAMES)

drawcalls:
	cargo run -p viewer --release --bin drawcalls -- $(FRAMES)

# Mirrors what stage_mods (src/export.rs) copies for a real export, minus the
# rest of the user's mods folder: just this mod's own files, loose rather
# than zipped, overwriting whatever is already installed.
install-mod:
	mkdir -p "$(MOD_INSTALL)/tests" "$(MOD_INSTALL)/locale/en"
	cp mod/control.lua mod/encode.lua mod/info.json mod/settings.lua mod/changelog.txt "$(MOD_INSTALL)/"
	cp mod/tests/encode_test.lua "$(MOD_INSTALL)/tests/"
	cp mod/locale/en/settings.cfg "$(MOD_INSTALL)/locale/en/"
	@echo "installed to $(MOD_INSTALL)"

# The portal requires the zip's top-level folder to be named exactly
# "<name>_<version>", matching info.json -- uploading a zip shaped any other
# way is rejected. Built fresh into a scratch dir each time rather than
# zipping mod/ in place, since mod/ itself is named "mod", not that.
#
# Lists files explicitly rather than copying mod/ wholesale (unlike
# install-mod, which deliberately mirrors a real export's staging): mod/tests/
# is a development-only lupa/lua test suite with no reason to ship to anyone
# who installs this from the portal, and an explicit list means a future
# dev-only file added under mod/ doesn't silently end up public by default.
package:
	rm -rf "dist/$(PACKAGE_NAME)"
	mkdir -p "dist/$(PACKAGE_NAME)/locale/en"
	cp mod/control.lua mod/encode.lua mod/info.json mod/settings.lua mod/changelog.txt "dist/$(PACKAGE_NAME)/"
	cp mod/locale/en/settings.cfg "dist/$(PACKAGE_NAME)/locale/en/"
	cd dist && zip -rq "$(PACKAGE_NAME).zip" "$(PACKAGE_NAME)"
	rm -rf "dist/$(PACKAGE_NAME)"
	@echo "packaged to dist/$(PACKAGE_NAME).zip"

clean:
	cargo clean
