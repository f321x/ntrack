# ntrack branding assets

Standalone renders of the app icon for use in social media, READMEs, store
listings, and avatars.

## Source of truth

These are **derived** from the in-app adaptive launcher icon — edit the app
icon, not these files, when changing the brand:

- Foreground vector: `android/app/src/main/res/drawable/ic_launcher_foreground.xml`
  (purple `#8b5cf6` location pin + green `#3ddc84` broadcast arcs)
- Background color: `ic_launcher_background` = `#0f1117`
  (in `android/app/src/main/res/values/themes.xml`)

The `.svg` files here re-create that composition and are the editable sources;
the `.png` files are rendered from them.

## Files

| File | Description |
|------|-------------|
| `ntrack-rounded-1024.png` / `-512.png` | Rounded-square app-icon look — best general pick (OG images, posts). |
| `ntrack-square-1024.png` | Hard-cornered square, opaque — when the platform applies its own mask. |
| `ntrack-circle-1024.png` | Circle-masked — profile pictures / avatars. |
| `ntrack-foreground-1024.png` | Pin + arcs only, **transparent** background — overlays. |
| `*.svg` | Editable vector sources for each variant. |

## Regenerating the PNGs

Requires `rsvg-convert` (Debian/Ubuntu: `librsvg2-bin`). Re-render at any size:

```sh
cd docs/branding
for v in rounded square circle foreground; do
  rsvg-convert -w 1024 -h 1024 "ntrack-$v.svg" -o "ntrack-$v-1024.png"
done
rsvg-convert -w 512 -h 512 ntrack-rounded.svg -o ntrack-rounded-512.png
```
