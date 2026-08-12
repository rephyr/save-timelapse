# Requirements

## Where did they come from?

I came across timelapse mods while browsing the in-game Factorio mod portal and
became particularly interested in the Factory Timelapse mod. I tried it out and
could not get it to produce anything.

It was not one bug. Driving Factorio headlessly to make a mod export something
turns out to have several ways to fail quietly, and this hit most of them at
once:

- The check used to tell a headless run from an ordinary game was whether any
  players were connected. Under `benchmark` mode that is always true, so the
  condition never distinguished anything.
- The mods folder was set up as a directory junction, which is not reliable on
  Windows.
- The mod waited two ticks before exporting, and the benchmark was not given
  enough ticks to reach that.
- Separately, the fix for getting the export flag set patched Factorio's binary
  settings file with a hardcoded version number, which stops working the next
  time the game updates.

None of those are interesting on their own. What they add up to is that the
fragile part of a timelapse tool is not the timelapse: it is the handshake with
Factorio, and every one of those four failures is something this tool now goes
out of its way to do differently. The export is triggered by a startup setting
rather than inferred from who is connected. The mods folder is a staged tree the
tool builds and deletes, with archives hardlinked rather than junctioned. The
benchmark is given enough ticks to reach the export. The settings file is parsed
properly and its version header is read from the file itself, or from the
executable, and never assumed.

At the time, the alternatives also had some significant drawbacks. Some
generated hundreds of high-resolution screenshots and quickly consumed enormous
amounts of disk space, while others required a tedious setup process involving
Python scripts, external dependencies, and additional software.

Rather than settling for those limitations, I decided to build my own timelapse
tool.

## What should a timelapse tool I actually enjoy using provide?

### Use as little disk space as possible

Factorio save files can grow to many gigabytes, so storing the entire state of
the save at every point in time is not practical.

Fortunately, most of a factory's structure remains unchanged throughout a
playthrough. Buildings generally stay in the same place, while only a relatively
small portion of the world changes between moments in time.

The timelapse format should therefore take advantage of this and store only what
has changed instead of repeatedly storing the entire factory.

The goal is to make long-running timelapses practical without requiring hundreds
of gigabytes of storage.

**How it turned out.** An hour of heavily modded play costs about 80 KB, plus a
one-time snapshot of whatever was already standing. A screenshot every ten
seconds at 1080p would be closer to 700 MB for the same hour. The format is a
custom binary one rather than JSON (which was my first solution but the binary solution used around 3 times less space), and grouping entities into runs with delta encoded positions took one megabase surface from 200 MB to 38 MB.

### Render large bases quickly

Storage efficiency is only half of the problem. A timelapse is not very useful
if the resulting data cannot be rendered efficiently.

Factorio factories can contain hundreds of thousands or even millions of
entities. The renderer therefore needs to be capable of drawing very large bases
at interactive frame rates without bringing the system to its knees.

The goal is to make it possible to freely explore the factory and scrub through
the timelapse while keeping rendering fast and responsive.

**How it turned out.** Prototype names are resolved once at load time and never
touched again while drawing, items are grouped so the GPU can draw a whole type
in one go, and anything too small to perceive is aggregated. On a real capture
that took a frame from ten thousand draw calls to under a hundred.

### Easy to install, with no dependencies

Most people (myself included) hate having to run a collection of Python scripts,
install external libraries such as FFmpeg, configure a video-editing
application, and troubleshoot dependencies just to watch a timelapse of a
Factorio playthrough.

The installation process should therefore be as fast and painless as possible.

I considered two main approaches: a WebGL-based application where the user could
simply drop a save file into a website, or a standalone executable.

The WebGL approach initially sounded appealing because it would eliminate
installation entirely. However, it introduced another usability problem: finding
the correct Factorio save file and identifying the relevant files or folders can
be surprisingly tedious.

A standalone application provides a better balance. It can automatically detect
the Factorio installation and save directory, while still requiring no external
dependencies from the user.

**How it turned out.** One executable, and a mod from the portal if you want to
record as you play. The video encoder is written by hand inside the tool for
exactly this reason: linking or shelling out to FFmpeg would have broken the
promise for the one feature most likely to be used by somebody who just wants
something to post.

## What I did not know I wanted until I built it

The three above are what I started with. These four came out of actually
recording a playthrough and living with the result.

### Never make the game stutter

This turned out to matter more than anything else on this page.

A timelapse is recorded while you are playing, which means every millisecond it
spends is a millisecond taken out of somebody's game. Time inside Factorio and
time in a desktop tool are not worth the same: you feel a stutter immediately,
and nobody minds an external tool taking a minute longer.

So the rule is to move work out of the game wherever it can go, even when that
means doing more work overall. Reading the ground from a save afterwards scans a
whole surface again, which is strictly more work than recording it while
playing, and it is still the right trade because none of it happens while you
are at the keyboard.

Everything the mod does during play is measured against this. It writes a
compact binary format because formatting JSON for every entity is real CPU time
during real gameplay, and it batches its writes because each one is a syscall.

### Two ways in, because a save file has no memory

I started out reading existing save files, which is the obvious approach: point
the tool at a folder and it builds a timelapse from what is already there.

The problem is that a save knows what your factory looks like now, and nothing
about how it got there. Reading a hundred saves gives you a hundred moments, and
the gaps between them are however often you happened to save.

So there is a second way in. The mod takes one snapshot when you turn it on and
then records only what changes, which gives a frame every few seconds instead of
every few hours. The cost is that it only knows what happened after you enabled
it, so the two ways complement each other rather than one replacing the other.

The ideal experience is still the same:

Install, launch, pick a recording, watch.

No Python environment, no FFmpeg installation, no video editor, and no
complicated setup.

### Work with whatever mods I am playing with

I play modded, and a tool that only understands Wube's own buildings is a tool
that does not work for me.

Alien Biomes alone adds a couple of hundred terrain types. Krastorio adds belt
tiers, ores and pipes that a viewer built around the base game cannot even
recognise as belts, ore or pipes. Writing all of that down by hand, once per
mod, forever, is not a plan.

The answer is to have the mod ask the running game what its own prototypes are,
and write that down beside the recording: every colour, every entity's type, how
far a modded underground belt reaches. Nothing to configure, and mods added
partway through a playthrough are picked up the next time you load the save.

### Do not break the recordings I already have

This one only became obvious after shipping a few versions.

Factorio updates mods automatically, but a desktop tool does not update itself,
so "the mod is newer than the tool" is the normal state rather than an unusual
one. A recording somebody left running for a week has to still open afterwards.

So the file layout is frozen, and anything added later is written in a form an
older version knows to skip over. Every released format is kept as real bytes in
the test suite, and a failure there is not a test to adjust: it means somebody's
recording stopped loading.
