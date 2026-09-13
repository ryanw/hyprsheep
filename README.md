# hyprsheep

A desktop sheep for Hyprland, after the 1995 Windows toy [eSheep][esheep].

The sheep wanders around your screen, walks along the top edges of your
windows, climbs the sides, falls when the window it was standing on closes,
and occasionally stops for a nap.

## Status

Working: the overlay, the animation engine, and window collision against live
Hyprland geometry. Not yet wired up: grabbing the sheep with the mouse, and
multi-monitor support.

## Running

```sh
cargo run --release
```

Set `HYPRSHEEP_TRACE=1` to log each animation change, where the sheep is, and
which window it is standing on.

There is nothing to configure and no data files to install; the sprite sheet
and the animation definitions are baked into the binary.

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

The sheep is drawn on a `wlr-layer-shell` overlay surface anchored to all four
edges, with a negative exclusive zone so bars cannot displace it and an empty
input region so every click falls through to the window underneath.

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
