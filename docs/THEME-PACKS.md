# Make a Hestia RS theme pack

1. Download and extract the Theme Builder. Open `theme-builder.html` in a browser. The builder works offline. [Download Theme Builder](../tools/theme-packs/theme-builder.zip)
2. Pick a starting palette, give your theme a name, and edit the colors. Use the swatches or enter hex values. The preview updates as you edit.
3. Select **Download ZIP** and share the file as-is. You can also download the editable JSON. Keep the same pack ID when publishing a new version of an existing theme.

## Starter ZIPs

Each ZIP has a complete palette you can edit by hand or use as a reference.

| Starter | Appearance | Download |
| --- | --- | --- |
| Hearth | Warm charcoal and ember orange | [hearth.zip](../tools/theme-packs/hearth.zip) |
| Daylight | Cool white and indigo | [daylight.zip](../tools/theme-packs/daylight.zip) |
| Midnight | Near-black with violet accents | [midnight.zip](../tools/theme-packs/midnight.zip) |
| Electric | Deep navy and bright cyan | [electric.zip](../tools/theme-packs/electric.zip) |
| Frosted Glass | Ice blue and slate | [frosted-glass.zip](../tools/theme-packs/frosted-glass.zip) |
| Harbor | Dark teal and mint | [harbor.zip](../tools/theme-packs/harbor.zip) |

The five built-ins can be used as the manifest's `base`. Harbor is a complete palette built on Hearth. The Frosted Glass starter includes the original translucent panel colors.

## Edit a manifest directly

Extract `manifest.json`, set a distinct lowercase `id` such as `your-name.seaglass`, and update the name and colors. Choose one of `hearth`, `daylight`, `midnight`, `electric`, or `frosted-glass` as the base. Omitted colors follow the base. Compress `manifest.json` itself so it sits at the root of the ZIP.

This example changes the buttons and focus glow.

```json
{
  "format_version": 1,
  "id": "your-name.seaglass",
  "name": "Seaglass",
  "version": "1.0.0",
  "base": "hearth",
  "components": {
    "palette": {
      "accent": "#62e5cd",
      "accent-hover": "#8cf5df",
      "accent-pressed": "#49c9b3",
      "on-accent": "#072622",
      "glow": "#62e5cd66"
    }
  }
}
```

Use `#RRGGBB` for colors. The `surface`, `card`, `card-hover`, `glow`, and `scrim` roles also accept `#RRGGBBAA` for transparency. Check text against its surface, and check accent text against all three accent button states. [Full starter manifest](../tools/theme-packs/harbor/manifest.json)

## Build or check from a checkout

The helper uses Python 3 and the standard library.

```sh
python3 tools/theme-packs/pack.py build manifest.json -o seaglass.zip
python3 tools/theme-packs/pack.py check seaglass.zip
```

Version 1 covers sharing palettes. Importing packs inside Hestia RS requires the upcoming ZIP picker.
