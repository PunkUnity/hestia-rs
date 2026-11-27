# Make a Hestia RS theme pack

[Download the Harbor starter ZIP](../tools/theme-packs/harbor.zip). It uses standard ZIP compression, so you can open it with your usual archive app and share it as a regular `.zip` attachment.

1. Extract `manifest.json` and give the pack a new `id` and `name`. Use an ID like `your-name.harbor` and keep it for later versions of the same pack.
2. Choose `hearth` or `daylight` as the base. Set the colors you want to change under `components.palette`. Any role you omit follows the base.
3. Compress `manifest.json` itself to make a ZIP with that file at the root. Send the ZIP as an attachment or share a direct download link.

For example, this palette changes only the buttons and focus glow.

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

The starter shows every supported color role. Use `#RRGGBB` for colors and optionally `#RRGGBBAA` for `glow` and `scrim`. Keep text readable on every surface and check `on-accent` against all three accent button states. [Full starter manifest](../tools/theme-packs/harbor/manifest.json)

## Build or check from a checkout

The helper uses Python 3 and the standard library.

```sh
python3 tools/theme-packs/pack.py build manifest.json -o seaglass.zip
python3 tools/theme-packs/pack.py check seaglass.zip
```

Version 1 covers palettes. The built-in theme build supports Hearth and Daylight. The app’s ZIP picker and preview are a follow-up.
