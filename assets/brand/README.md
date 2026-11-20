# tutti brand files

The mark is three people lifting the one in the middle, each drawn as a lowercase t: the stem
is the body, the crossbar the arms, the curled foot a step, the head a ball above the stem.
Everyone together, which is what *tutti* means.

## Files

| File | Use |
|---|---|
| `tutti-mark-{black,white}.svg` | The mark, 48 px tall and up |
| `tutti-mark-small-{black,white}.svg` | The mark from 24 to 48 px: no free arms, wider gaps |
| `tutti-favicon-{black,white}.svg` | Below 24 px: the middle figure alone |
| `tutti-wordmark-{black,white}.svg` | The name alone |
| `tutti-lockup-horizontal-{black,white}.svg` | Mark and name side by side: the default |
| `tutti-lockup-stacked-{black,white}.svg` | Mark above name, for square spaces |
| `tutti-banner-{light,dark}.svg` | The README banner: the name alone, in reeded glass |
| `tutti-avatar.png` | Organisation avatar, 1024 px, the mark inside a round crop's safe area |
| `favicon.ico`, `icon-192.png`, `icon-512.png`, `apple-touch-icon.png` | Site icons |
| `tutti-icon-square{,-white,-dark}.png` | Square icon, mark only, 1024 px, on paper, white or ink: avatars and app icons; sized to stay whole in a round crop |
| `tutti-social.png`, `tutti-social-mark.png` | Social preview cards, 1280 × 640: with the name, or the mark only on ink |

The white marks are separate drawings, not recoloured copies: light on dark reads heavier, so
they are drawn 3 % thinner. Use them on dark backgrounds.

## Rules

- **Clear space:** one head diameter on every side, already built into every file's frame.
- **Colour:** ink `#111111` on paper `#f2f1ed`, or white on ink. One colour only.
- **Size:** each file within its own range in the table above.
- **Leave it as drawn:** no stretching, rotating, outlines, shadows, gradients or re-spacing
  of the lockups, and no recolouring of single figures.

## Sources

The name is set in Inter Display Black (SIL Open Font License 1.1), outlined, with the `tt`
crossbar joined. The mark is generated from distance fields and refit as cubic Béziers;
every file here is a plain filled path, with no text, masks or embedded images. The
banner's glass is SVG filters over the outlined name.
