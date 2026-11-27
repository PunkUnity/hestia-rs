# Make a Hestia RS theme pack

1. Download and extract the Theme Builder. Open `theme-builder.html` in a browser. The builder works offline. [Download Theme Builder](../tools/theme-packs/theme-builder.zip)
2. Pick a starting palette, give your theme a name, and edit the colors. Use the swatches or enter hex values. The preview updates as you edit.
3. Select **Download ZIP** and share the file as-is. Use **Open ZIP or JSON** to check an existing pack and keep editing it. Keep the same pack ID when publishing a new version.

## Starter ZIPs

Each ZIP has a complete palette you can edit by hand or use as a reference.

| Starter | Appearance | Download |
| --- | --- | --- |
| Hearth | Warm charcoal and ember orange | [hearth.zip](../tools/theme-packs/hearth.zip) |
| Daylight | Cool white and indigo | [daylight.zip](../tools/theme-packs/daylight.zip) |
| Midnight | Near-black with violet accents | [midnight.zip](../tools/theme-packs/midnight.zip) |
| Electric | Violet-navy panels with a cyan voltage trace | [electric.zip](../tools/theme-packs/electric.zip) |
| Frosted Glass | Translucent winter-blue panes over a dark ice backdrop | [frosted-glass.zip](../tools/theme-packs/frosted-glass.zip) |
| Blue | Classic navy panels and cyan controls | [blue.zip](../tools/theme-packs/blue.zip) |
| Dark Rainbow | Jet black, deep violet panels and a slow rainbow across each heading | [dark-rainbow.zip](../tools/theme-packs/dark-rainbow.zip) |
| Harbor | Dark teal and mint | [harbor.zip](../tools/theme-packs/harbor.zip) |

The seven built-ins can be used as the manifest's `base`. Harbor is a complete palette built on Hearth. ZIPs carry palette values, including translucent panels. The corresponding built-in themes draw the ice, voltage, and spectrum effects.

## Edit a manifest directly

Extract `manifest.json`, set a distinct lowercase `id` such as `your-name.seaglass`, and update the name and colors. Use a built-in starter's `base` value. Omitted colors follow the base. Compress `manifest.json` itself so it sits at the root of the ZIP, then open it in the builder to validate and preview.

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

Version 1 covers sharing palettes. Importing packs inside Hestia RS requires the upcoming ZIP picker.
