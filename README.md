# hyprsheep

A desktop sheep for Hyprland, after the 1995 Windows toy [eSheep][esheep].

The sheep wanders around your screen, walks along the top edges of your
windows, climbs the sides, falls when the window it was standing on closes,
and occasionally stops for a nap.

## Status

Everything works: the overlay, the animation engine, window collision against
live Hyprland geometry, picking the sheep up with the mouse, and multiple
monitors.

## Running

```sh
cargo run --release
```

Nothing needs installing: the sprite sheet and the animation definitions are
baked into the binary.

## Configuration

All optional. Every setting has both a command-line option and a config file
key, and the command line wins:

```sh
hyprsheep --sheep 3 --monitors eDP-1,HDMI-A-1 --no-draggable
hyprsheep --pet ~/pets/green_sheep.xml --trace
```

```toml
# ~/.config/hyprsheep/config.toml
sheep     = 1      # how many sheep to keep on screen
monitors  = "all"  # "all", one name, or ["eDP-1", "HDMI-A-1"]
draggable = true   # whether the sheep can be picked up with the mouse
trace     = false  # log every animation change and where the sheep is
# pet     = "~/pets/green_sheep.xml"
```

`--help` lists the lot. Booleans can be written `--draggable`,
`--no-draggable` or `--draggable=false`, and values as either `--sheep 3` or
`--sheep=3`. `HYPRSHEEP_TRACE=1` is equivalent to `--trace`.

A bad line in the file is reported and its default kept, so a typo cannot
leave you without a sheep; a bad command-line option stops instead, since you
are standing right there to fix it.

With `draggable = false` the overlay is click-through everywhere, with no
region for the pointer to catch on.

`pet` loads an alternative pet in the same XML format, sprite sheet and all -
these files are self-contained. The green sheep that ships with web-esheep
works, for instance, and brings 186 animations with it.

## How it works

The behaviour is not hand-written. It comes from the original pet format: a
graph of 54 animations in `assets/animations.xml`, each declaring its frames,
its velocity, and a weighted list of animations it may move to next.

Three separate transition tables decide what happens after every step —
`<sequence>` when an animation runs to completion, `<border>` on hitting a
screen edge or landing on a window, and `<gravity>` when there is nothing
underfoot. Motion is data too: `<start>` and `<end>` carry per-step velocities
that ramp across the sequence, which is why falling accelerates without any
physics code.

Window geometry comes from Hyprland's IPC. `.socket.sock` answers `j/clients`
and `j/monitors`; `.socket2.sock` streams an event per compositor change,
which is used only as a hint to re-read the layout.

The sheep live in one global coordinate space spanning every monitor, so a
sheep can walk off one screen and onto the next. Each output gets its own
`wlr-layer-shell` overlay surface, drawing whichever sheep overlap it; one
straddling the seam is drawn on both. Screen edges are only walls where no
other monitor continues past them, and each monitor keeps its own floor, so a
sheep that walks off a short screen onto a taller one falls.

Each surface is anchored to all four with a negative exclusive zone so bars cannot displace it. Its input
region is narrowed to just the sheep, so they can be picked up and dragged
while every other click falls straight through to the window underneath.

## Divergences from the reference

Ported from [web-esheep][web], with three deliberate departures, each covered
by a test:

- The `only` attribute on transitions is honoured, so window- and
  taskbar-specific moves require the matching context. The reference ignores
  it entirely, which lets window-only animations play in mid-air.
- Spawn weights are summed correctly. The reference sums the first weight
  repeatedly, which makes its fourth spawn point unreachable.
- `Convert(x, System.Int32)` evaluates as truncation. It is .NET syntax that
  the JavaScript reference throws on, silently reducing two animations to zero
  repeats.

`offsety` and `opacity` are also applied; the reference parses them and then
never uses them.

## Licence

GPL-3.0. The sprite sheet and animation data come from web-esheep by Adriano
Petrucci. See `NOTICE`.

[esheep]: https://esheep.petrucci.ch/
[web]: https://github.com/Adrianotiger/web-esheep
