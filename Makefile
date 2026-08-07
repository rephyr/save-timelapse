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

.PHONY: help build test test-lua run viewer drawcalls install-mod clean

help:
	@echo "Targets:"
	@echo "  build        cargo build --release --workspace"
	@echo "  test         cargo test --workspace"
	@echo "  test-lua     run mod/tests/encode_test.lua (needs a lua interpreter on PATH)"
	@echo "  run          save-timelapse: interactive, asks what to do and opens the viewer"
	@echo "  viewer       open the interactive viewer on FRAMES"
	@echo "  drawcalls    headless draw-call report on FRAMES"
	@echo "  install-mod  copy mod/ into MOD_INSTALL (your Factorio mods folder)"
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
	mkdir -p "$(MOD_INSTALL)/tests"
	cp mod/control.lua mod/encode.lua mod/info.json mod/settings.lua "$(MOD_INSTALL)/"
	cp mod/tests/encode_test.lua "$(MOD_INSTALL)/tests/"
	@echo "installed to $(MOD_INSTALL)"

clean:
	cargo clean
