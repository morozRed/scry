# Scry's icon

An orb you look into, holding one agent session's lineage: the session at the top, the two
processes it started below, and the amber one a leftover it left running. The colours are
the terminal UI's accent and warning hues.

| File | Use |
|---|---|
| `scry.svg` | The icon, from 64 px up |
| `scry-small.svg` | 32 px and under: the orb is left out so the lineage stays legible |
| `scry-1024.png` … `scry-64.png` | `scry.svg` rendered |
| `scry-32.png`, `scry-16.png` | `scry-small.svg` rendered |

To render them again: `rsvg-convert -w 512 -h 512 scry.svg -o scry-512.png`.
