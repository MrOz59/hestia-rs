# Make a Hestia RS theme pack

## Install a pack

1. Download a starter ZIP below or choose a pack someone shared. Keep the ZIP intact.
2. In Hestia RS, open **Settings → Appearance → Theme → Load Theme Pack from File** and select the ZIP.
3. Preview the result and tap **Save** to add it to the theme list and use it on this device. **Back** discards the preview. If you import a new revision with the same pack ID, Save replaces that entry.

**Settings → Loading screen** offers Flame, Pulse, Orbit and Still. They use the current theme colors. Preview the selected animation in Settings without connecting to a host.

## Make and share a pack

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
| Electric | Violet-navy panels with branching cyan arcs that wind around the edges | [electric.zip](../tools/theme-packs/electric.zip) |
| Frosted Glass | Translucent winter-blue panes over gently shimmering ice | [frosted-glass.zip](../tools/theme-packs/frosted-glass.zip) |
| Blue | Classic navy panels and cyan controls | [blue.zip](../tools/theme-packs/blue.zip) |
| Dark Rainbow | Jet black, deep violet panels and a rainbow that moves and pulses across headings | [dark-rainbow.zip](../tools/theme-packs/dark-rainbow.zip) |
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

Hestia RS accepts ZIPs up to 256 KB containing a single `manifest.json` at the archive root. The file may be stored or deflated. Version 1 packs carry palette data, and the app supplies the visual effects and loading animations.
