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

# Windows Lua builds are usually named for their version (lua52.exe,
# lua5.2.exe) rather than a bare `lua`, so this is overridable rather than
# hardcoded at the one call site. See test-lua below for which version.
LUA         ?= lua

# Every Lua file that is part of the shipped mod, named once and used by both
# install-mod and package below.
#
# Explicit rather than a mod/*.lua glob, so a development-only file added
# under mod/ never silently ends up installed or published. The cost of that
# is a file being forgotten instead, which is worse: control.lua `require`s
# each of these, so a missing one is not a degraded mod but one Factorio
# refuses to load at all. `check-mod-files` below closes that gap by failing
# when mod/ holds a .lua this list does not mention.
MOD_LUA := control.lua capture.lua export.lua gui.lua data.lua snapshot.lua encode.lua milestones.lua
MOD_META := info.json settings.lua changelog.txt

# Read once at parse time rather than re-derived in every recipe that needs
# it: Make runs each recipe line in its own shell, so a shell variable set
# in `package`'s first line wouldn't survive to its later lines anyway.
VERSION      := $(shell sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' mod/info.json)
PACKAGE_NAME := save-timelapse_$(VERSION)

.PHONY: help build test test-lua check-mod-syntax run viewer drawcalls stress stress-save check-mod-files install-mod package clean

help:
	@echo "Targets:"
	@echo "  build        cargo build --release --workspace"
	@echo "  test         cargo test --workspace"
	@echo "  test-lua     run mod/tests/encode_test.lua (needs Lua 5.2 on PATH, see LUA)"
	@echo "  run          save-timelapse: interactive, asks what to do and opens the viewer"
	@echo "  viewer       open the interactive viewer on FRAMES"
	@echo "  drawcalls    headless draw-call report on FRAMES"
	@echo "  stress       benchmark the whole pipeline against the saved baseline"
	@echo "  stress-save  run the benchmark and record it as the new baseline"
	@echo "  install-mod  copy mod/ into MOD_INSTALL (your Factorio mods folder)"
	@echo "  package      zip mod/ into dist/$(PACKAGE_NAME).zip, ready for the mod portal"
	@echo "  clean        cargo clean"
	@echo ""
	@echo "Variables (current value, override with make VAR=... target):"
	@echo "  MOD_INSTALL  $(MOD_INSTALL)"
	@echo "  FRAMES       $(FRAMES)"
	@echo "  LUA          $(LUA)"

build:
	cargo build --release --workspace

test:
	cargo test --workspace

# Separate from `test` on purpose: cargo test's whole point is needing
# nothing beyond the Rust toolchain, and that shouldn't start silently
# depending on a lua interpreter (see mod/tests/encode_test.lua).
#
# Wants Lua 5.2 specifically, the version Factorio's modding API is, not
# merely whichever Lua is easiest to install. The suite passes under 5.1
# through 5.5 alike, so a newer interpreter looks perfectly healthy while
# accepting things Factorio would not run, and those are exactly the
# features encode.lua works around by hand (see its module comment on
# packing integers without string.pack or bit32).
#
# The two kinds of 5.3+ feature fail differently here, which is worth
# knowing when reading a green run:
#
#   `//`, `&`, `~`     new syntax, so 5.2 refuses to even load the file.
#                      Caught the moment anything requires it.
#   string.pack,       not syntax, just absent: the call parses fine and
#   math.type          fails only when that line actually runs, so this
#                      catches it only where a test covers that path.
test-lua: check-mod-syntax
	$(LUA) mod/tests/encode_test.lua

# Compiles every shipped Lua file without running it. The unit suite above
# only dofile()s encode.lua, so a syntax error anywhere else would otherwise
# surface for the first time as Factorio refusing to load the mod, and a mod
# that will not load at all is a far worse failure than any one feature being
# wrong. `loadfile` compiles without executing, which is what makes this work
# for files like capture.lua that reference `game` and `defines` and could
# never actually be run outside the game.
check-mod-syntax:
	@for f in $(addprefix mod/,$(MOD_LUA)); do \
	  $(LUA) -e "local c,e=loadfile('$$f'); if not c then io.stderr:write('syntax error: '..tostring(e)..'\n'); os.exit(1) end" || exit 1; \
	  echo "ok   $$f parses"; \
	done

run:
	cargo run --release --bin save-timelapse

viewer:
	cargo run -p viewer --release --bin viewer -- $(FRAMES)

drawcalls:
	cargo run -p viewer --release --bin drawcalls -- $(FRAMES)

# Development benchmark: run the whole pipeline at megabase scale and compare
# every number against a saved baseline, to answer "did the change I just made
# help or hurt". Take a baseline first, edit, then re-run:
#
#   make stress-save     # record the current code as the baseline
#   ...edit...
#   make stress          # current vs baseline, with deltas
#
# Sizes and counts are exact, so any delta there is real. Timings swing a few
# percent between identical runs and are marked as noise below 10%.
#
# A baseline is only comparable against the same shape, so re-record after
# changing any dimension:
#
#   make stress STRESS_ARGS="--surfaces 1 --entities 2000000"
stress:
	cargo run -q -p viewer --release --bin stress -- $(STRESS_ARGS)

stress-save:
	cargo run -q -p viewer --release --bin stress -- $(STRESS_ARGS) --save

# Fails if mod/ holds a .lua that neither list names, which is exactly how a
# newly added, required file would otherwise reach a player: control.lua
# `require`s these, so a missing one is not a degraded mod but one Factorio
# refuses to load at all. A prerequisite of both install-mod and package
# rather than something to remember, since those are the two moments it
# matters.
check-mod-files:
	@for f in mod/*.lua; do case " $(MOD_LUA) $(MOD_META) " in *" $$(basename $$f) "*) ;; *) echo "mod/$$(basename $$f) is in neither MOD_LUA nor MOD_META, so it would not be installed or published"; exit 1;; esac; done

# Mirrors what stage_mods (src/export.rs) copies for a real export, minus the
# rest of the user's mods folder: just this mod's own files, loose rather
# than zipped, overwriting whatever is already installed.
install-mod: check-mod-files
	mkdir -p "$(MOD_INSTALL)/tests" "$(MOD_INSTALL)/locale/en" "$(MOD_INSTALL)/graphics"
	cp $(addprefix mod/,$(MOD_LUA) $(MOD_META)) "$(MOD_INSTALL)/"
	cp mod/tests/encode_test.lua "$(MOD_INSTALL)/tests/"
	cp mod/locale/en/settings.cfg mod/locale/en/gui.cfg "$(MOD_INSTALL)/locale/en/"
	cp mod/graphics/shortcut-x32.png mod/graphics/shortcut-x24.png "$(MOD_INSTALL)/graphics/"
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
package: check-mod-files
	rm -rf "dist/$(PACKAGE_NAME)"
	mkdir -p "dist/$(PACKAGE_NAME)/locale/en" "dist/$(PACKAGE_NAME)/graphics"
	cp $(addprefix mod/,$(MOD_LUA) $(MOD_META)) "dist/$(PACKAGE_NAME)/"
	cp mod/locale/en/settings.cfg mod/locale/en/gui.cfg "dist/$(PACKAGE_NAME)/locale/en/"
	cp mod/graphics/shortcut-x32.png mod/graphics/shortcut-x24.png "dist/$(PACKAGE_NAME)/graphics/"
	if [ -f mod/thumbnail.png ]; then cp mod/thumbnail.png "dist/$(PACKAGE_NAME)/"; fi
	cd dist && zip -rq "$(PACKAGE_NAME).zip" "$(PACKAGE_NAME)"
	rm -rf "dist/$(PACKAGE_NAME)"
	@echo "packaged to dist/$(PACKAGE_NAME).zip"

clean:
	cargo clean
