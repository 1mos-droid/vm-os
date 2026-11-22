# Strata: the HyperOS design system

`strata.slint` is the one place where HyperOS's look is defined. It supplies the
colour, type and spacing tokens and every control used by `desktop-shell` and
VM Studio (`vm-control-ui`). Both crates import it via their `build.rs`
(`../ui-kit` in the repository, or `./ui-kit` after Buildroot copies it in).

## Where the ideas come from

| Source | What Strata takes | What it leaves |
| --- | --- | --- |
| **macOS** | Material layers instead of outlines, a strict type ladder, sidebar apps, a dock, sheet dialogs | Live blur (too costly for a CPU-only renderer) |
| **GNOME / libadwaita** | Flat header bars, grouped "boxed list" rows, one accent-filled primary action per view, switches for on/off settings | Hiding everything behind an overview |
| **elementary OS** | Sensible defaults, concise wording, typography as a mark of quality | — |
| **Windows 11 (Mica)** | Opaque surfaces tinted by the wallpaper: depth without blur | Acrylic transparency |
| **COSMIC** | Tiling built into the shell (snap buttons in every header) | — |
| **Qubes OS** | Every window carries a host-drawn origin colour | Loud, thick borders: Strata uses a thin edge plus a labelled chip, and dims both when the window is not focused |

## Principles

1. **Materials, not borders.** Depth comes from layered, tinted surfaces over the
   aurora wallpaper (`Backdrop`). The software renderer draws gradients and
   translucency cheaply but has no blur or drop shadows, so Strata uses none.
2. **One type family, one ladder.** Inter, embedded in the binaries, so text
   looks the same on every machine. Sizes come from `Theme.size-*`
   (caption → large title); hierarchy comes from size and weight, not colour.
3. **Chrome recedes.** Toolbar buttons are ghosts until hovered. Each view has at
   most one accent-filled button.
4. **Calm but unspoofable trust cues.** Origin colours and Space names on windows
   are drawn by the host shell, never by a guest.
5. **Controls centre themselves.** A control draws its visual centred in whatever
   height a layout gives it, so pages never need alignment hacks.

## Tokens

The `Theme` property names are a fixed contract with the Rust theme loader and
the JSON themes (`*/themes/*.json`). Values come from the theme file; everything
under `// Derived` in `strata.slint` (separators, fills, warning, type ladder…) is
computed from those values and the light/dark flag.

## Components

`Backdrop` · `Card` · `Separator` · `LargeTitle` · `Title` · `Headline` · `Body` ·
`Caption` · `SectionTitle` · `PushButton` (primary, secondary, ghost, danger,
danger-solid) · `IconButton` · `Chip` · `StatusDot` · `Switch` · `Segmented` ·
`Meter` · `Field` · `RangeSlider` · `Tile` · `ListGroup` / `ListRow` ·
`OptionCard` · `Toast` · `Sheet` · `Icon` (about 45 stroke glyphs drawn with
`Path`, so no image decoders are needed).

## Fonts

`fonts/` holds Inter 4.1 (Regular, Medium, SemiBold, Bold) by The Inter Project
Authors, under the SIL Open Font License 1.1 (`fonts/OFL.txt`). Monospace text
uses the system's DejaVu Sans Mono.
