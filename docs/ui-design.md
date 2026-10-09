# UI design

rigbat's UI conventions as the code implements them. Every rule names the file that implements
it; when the two disagree, the code wins and this document is out of date. Four surfaces share
these rules: the settings window, the dashboard, the tray menu and the tray icon. Where their data
comes from is in [`architecture.md`](architecture.md#data-flow-per-surface).

## Principles

| Principle                                | What the code does                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Where                                                                                   |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| Follow the session, never theme it       | Both windows take the scheme and accent from xdg-desktop-portal's standard `color-scheme` and `accent-color` keys, and the text scale from `org.gnome.desktop.interface` `text-scaling-factor` (a desktop-specific key the portal passes through, so a host may not send it); every change re-applies live. The accent becomes egui's selection colour; the text scale becomes the zoom factor and grows the window size, up to the smallest screen's width. The portal's `contrast` and `reduced-motion` keys raise contrast and stop animation ([Contrast and reduced motion](#contrast-and-reduced-motion)). The one override, the windows' theme (`WindowTheme`, System by default), forces egui's stock light or dark visuals and the palette's matching scheme (`Appearance::with_theme`); accent and text scale stay the portal's. The tray icon always follows the portal's scheme: it sits on the system's panel (project choice: SNI carries no scheme, and the panel need not match it). The palette picks status colours inside the scheme; window surfaces and text are egui's for that scheme ([Palettes](#palettes)). | `src/gui/mod.rs`, `src/appearance/mod.rs`, `src/palette.rs`                             |
| Cap the content width                    | Every settings tab is a centred column at most `CONTENT_MAX_WIDTH` wide (`page`); a wider window adds margin, not longer rows: the pattern of libadwaita's `AdwClamp` (default 600; 640 is rigbat's value). The dashboard has a fixed width.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | `src/settings/widgets.rs`                                                               |
| One trailing control per row             | Project choice. `Rows::row` takes exactly one `control`. A secondary action on the same setting sits in the subtitle as a button (the pin's "Clear" in the Tray group, a device override's "↺").                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | `src/settings/widgets.rs`, `src/settings/general_tab.rs`, `src/settings/devices_tab.rs` |
| Status text carries a sign, not only hue | `charge_value` prefixes a low reading with `LOW_SIGN` (⚠) and a charging one with `CHARGING_SIGN` (⚡). The dashboard also colours a low value; the tray menu has no colour and relies on the sign. The tray icon marks charging by shape in every display mode (`status_mark`: a bolt) and low by colour on the pixels: a triangle at 22 px is one-pixel cells that read as a dot and would shrink the digits. Low also sets SNI `Status=NeedsAttention`, which hosts may show without colour.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | `src/domain/text.rs`, `src/icon/mod.rs`                                                 |
| Same words on every surface              | The dashboard row, the Devices tab row, the tray menu row, the tray tooltip and the `--waybar` tooltip are built from `charge_value` and `status_note` (`device_line` joins them; English for waybar), so a state reads the same everywhere; both windows draw a device's kind as the emoji from `gui::kind_glyph`, the tray icon's `DeviceAndBattery` style as an `icon::silhouette` bitmap; both windows use one value style (`gui::charge_value_text`). Devices are ordered by `roster_order`: online first, then by name, ignoring case (project choice).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `src/domain/text.rs`, `src/domain/roster.rs`, `src/gui/mod.rs`                          |
| A low reading never gets quieter         | A retained (not live) reading is dimmed by `palette::DIM` on the tray icon's fill and the dashboard bar, the tray icon marks it with a dashed outline or dotted digits ([Tray icon](#tray-icon)), and both windows name the presence instead of a percentage (`charge_value`) — except when it is low, which renders exactly as a live one.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `src/icon/mod.rs`, `src/dashboard/mod.rs`                                               |
| Contrast is measured, not eyeballed      | `palette::contrast_ratio` (WCAG 2.2; the formula is 2.1's) backs tests: secondary text (`gui::secondary_text`, one rule for both windows) ≥ 4.5:1 on the panel and the group fill, text on the accent ≥ 4.5:1 (`readable_on` picks black or white). egui's weak text colour misses 4.5:1 on dark, so secondary text is the body colour at a smaller size. Palette colours pass through `palette::readable`: text ≥ 4.5:1, bars and icon marks ≥ 3:1 as graphical objects, dimmed included ([Readability](#readability)).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | `src/palette.rs`, `src/gui/mod.rs`, `src/settings/widgets.rs`, `src/icon/mod.rs`        |
| Custom widgets are accessible widgets    | `switch`, `tile`, the tabs and an expander row report a role and label through `widget_info` (checkbox, radio button, selectable label, collapsing header with its expanded state), take keyboard focus and draw a focus ring. A glyph-only button announces a word ("↺" is "Use the default"). Both windows export an AT-SPI tree through eframe's `accesskit` feature; status changes are live regions ([Screen readers](#screen-readers)).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `src/settings/widgets.rs`, `Cargo.toml`                                                 |
| Levels are always visible                | The charge levels are the product, so a shown device keeps its tray icon while nothing is wrong. KDE asks a tray icon to appear only on abnormal status; rigbat departs from that on purpose.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | `src/tray/manager.rs`                                                                   |

Without a portal, windows use `Appearance::default`: light scheme, no accent, text scale 1.0; a
forced theme still applies. A portal reporting no preference maps to light (`map_scheme`), as
libadwaita and the GNOME HIG default to; only "prefer dark" turns a window dark. The tray icon
uses the same mapping.

## Tokens

Values are egui points, multiplied by the session text scale through the zoom factor. Settings
tokens are private constants in `src/settings/widgets.rs` unless marked `pub`; dashboard tokens
are private constants in `src/dashboard/mod.rs`; tokens both windows use are `pub` in
`src/gui/mod.rs` (marked `gui::`).

### Spacing and size

| Token                       | Value     | Use                                                                                                                                                        |
| --------------------------- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CONTENT_MAX_WIDTH` (`pub`) | 640       | Widest a settings tab's column gets                                                                                                                        |
| `PANEL_MARGIN` (`pub`)      | 16        | Settings window panel margin                                                                                                                               |
| `TAB_BAR_GAP` (`pub`)       | 8         | Tab bar to page                                                                                                                                            |
| `PAGE_PADDING`              | 8         | Above a page's first line and below its last                                                                                                               |
| `TOOLBAR_GAP` (`pub`)       | 8         | Devices toolbar and status line to the first group                                                                                                         |
| `gui::ROW_HEIGHT`           | 48        | Minimum row height in a group; a dashboard row; an expander row                                                                                            |
| `NESTED_ROW_HEIGHT`         | 36        | Minimum height of a row under an expanded row                                                                                                              |
| `gui::GLYPH_COLUMN`         | 30        | Kind-glyph column, dashboard and Devices rows; the indent of nested rows                                                                                   |
| `gui::GLYPH_SIZE`           | 20        | Kind emoji                                                                                                                                                 |
| `CHEVRON_SIZE`              | 10        | Expander chevron box                                                                                                                                       |
| `ROW_PADDING_X`             | 14        | Row inset from the group box; half of it insets group titles and footers                                                                                   |
| `ROW_PADDING_Y`             | 8         | Least vertical padding around a row's text                                                                                                                 |
| `ROW_GAP`                   | 16        | Between a row's text and its control                                                                                                                       |
| `GROUP_TITLE_GAP`           | 6         | Group title to box                                                                                                                                         |
| `GROUP_GAP`                 | 20        | After each group                                                                                                                                           |
| `FOOTER_GAP`                | 6         | Box to group footer text                                                                                                                                   |
| `FOOTER_SPACING`            | 4         | Between the page footer's items and their `·` separators                                                                                                   |
| `SWITCH_SIZE`               | 44 × 24   | Switch track, `gui::MIN_TARGET` tall                                                                                                                       |
| `gui::MIN_TARGET`           | 24        | Least width and height of a pointer target; egui's `interact_size.y` (`gui::apply`)                                                                        |
| `KNOB_INSET`                | 3         | Switch knob inset from the track                                                                                                                           |
| `TILE_HEIGHT`               | 76        | Picture tile                                                                                                                                               |
| `TILE_GAP` (`pub`)          | 10        | Between tiles                                                                                                                                              |
| `TILE_CAPTION_GAP`          | 8         | Tile image to caption, and caption side inset                                                                                                              |
| `TAB_PADDING`               | 12 × 6    | Around a tab label                                                                                                                                         |
| `TAB_UNDERLINE`             | 3         | Selected tab's underline thickness                                                                                                                         |
| `FOCUS_GAP`                 | 2.5       | Focus ring distance from the widget                                                                                                                        |
| `WINDOW_DEFAULT_SIZE`       | 720 × 640 | Settings window opening size (`src/settings/mod.rs`); grows with the text scale, its width capped like the minimum's                                       |
| `WINDOW_MIN_SIZE`           | 672 × 360 | Settings window minimum: `CONTENT_MAX_WIDTH` + 2 × `PANEL_MARGIN` (`src/settings/mod.rs`); × text scale, width at most `gui::SMALLEST_SCREEN_WIDTH` (1024) |
| `SEARCH_MIN_DEVICES`        | 8         | Devices listed before the tab offers a search field (`src/settings/devices_tab.rs`)                                                                        |
| `WINDOW_WIDTH`              | 380       | Dashboard width                                                                                                                                            |
| `MARGIN`                    | 12        | Dashboard panel margin                                                                                                                                     |
| `ROW_PADDING`               | 5         | Dashboard row's vertical inset                                                                                                                             |
| `GAP`                       | 8         | Dashboard gap between glyph, name, value, bar and note                                                                                                     |
| `BAR_HEIGHT`                | 4         | Dashboard charge bar                                                                                                                                       |
| `FOOTER_HEIGHT`             | 32        | Dashboard footer                                                                                                                                           |
| `MAX_VISIBLE_ROWS`          | 10        | Dashboard rows before the list scrolls                                                                                                                     |

### Radii and strokes

| Element                     | Radius                      | Stroke                                                                                                                     |
| --------------------------- | --------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Group box                   | `GROUP_RADIUS` (8)          | `visuals.widgets.noninteractive.bg_stroke`                                                                                 |
| Row separator, tab bar rule | —                           | `visuals.widgets.noninteractive.bg_stroke`                                                                                 |
| Tile                        | `GROUP_RADIUS` − 2          | Idle: `noninteractive.bg_stroke`; hovered: `widgets.hovered.bg_stroke`; selected: `SELECTED_TILE_STROKE` (2) in the accent |
| Switch track                | Half its height (pill)      | `noninteractive.bg_stroke`                                                                                                 |
| Switch knob                 | Track radius − `KNOB_INSET` | `noninteractive.bg_stroke`                                                                                                 |
| Expander hover fill         | `GROUP_RADIUS` − 1          | —                                                                                                                          |
| Expander chevron            | —                           | `CHEVRON_WIDTH` (1.5) in the secondary text colour                                                                         |
| Tab underline               | `TAB_UNDERLINE` / 2         | —                                                                                                                          |
| Dashboard bar               | `BAR_HEIGHT` / 2            | —                                                                                                                          |
| Focus ring                  | Widget radius + `FOCUS_GAP` | `FOCUS_WIDTH` (1.5) in `widgets.hovered.fg_stroke.color`                                                                   |

### Colours

Window surfaces and text come from egui `Visuals` for the current scheme; rigbat only replaces the
selection colour with the accent (`gui::apply`). Status colours come from the palette through
`gui::status_colors` (`StatusColors`).

| Role             | Source                                                                    | Used by                                                                   |
| ---------------- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Accent           | `visuals.selection.bg_fill`                                               | Switch on-track, selected tile outline, tab underline                     |
| Group fill       | `visuals.faint_bg_color`                                                  | Group box                                                                 |
| Tile fill        | `visuals.extreme_bg_color`                                                | Tile                                                                      |
| Switch off-track | `visuals.widgets.inactive.bg_fill`                                        | Switch                                                                    |
| Switch knob      | The lighter of `extreme_bg_color` and `strong_text_color()` (`knob_fill`) | Switch                                                                    |
| Row hover        | `widgets.hovered.weak_bg_fill` × `HOVER_TINT` (0.5)                       | Expander row                                                              |
| Body text        | `visuals.text_color()`                                                    | Everything, and secondary text in both windows (`gui::secondary_text`)    |
| Strong text      | `visuals.strong_text_color()`                                             | Selected tab, selected tile caption                                       |
| Weak text        | `visuals.weak_text_color()`                                               | Idle tab                                                                  |
| Ordinary reading | Palette `fg` (`StatusColors::normal`)                                     | Dashboard bar                                                             |
| Charging         | Palette `charging` (`StatusColors::charging`)                             | Dashboard bar                                                             |
| Low              | Palette `low` (`StatusColors::low`, `gui::charge_value_text`)             | A low charge value and its bar, dashboard and Devices tab; armed "Remove" |
| Warning          | Palette `warn` (`StatusColors::warn`)                                     | Devices tab: unanswered tray                                              |
| Bar track        | Palette `track` (`StatusColors::track`)                                   | Dashboard bar                                                             |

#### Palettes

`palette` in `config.json` (`domain::Palette`, default `catppuccin`), picked on the Appearance tab.
Light or dark is the windows' theme in the windows and the portal's scheme on the tray icon; the
palette picks the colours inside it. The tables are
`palette::swatches`, in colours each palette's authors publish (one role substituted, see below):

| Palette    | Scheme | bg        | fg        | charging  | low       | warn      | track     |
| ---------- | ------ | --------- | --------- | --------- | --------- | --------- | --------- |
| Catppuccin | Light  | `#eff1f5` | `#4c4f69` | `#40a02b` | `#d20f39` | `#df8e1d` | `#ccd0da` |
| Catppuccin | Dark   | `#1e1e2e` | `#cdd6f4` | `#a6e3a1` | `#f38ba8` | `#f9e2af` | `#45475a` |
| Everforest | Light  | `#fdf6e3` | `#5c6a72` | `#35a77c` | `#f85552` | `#dfa000` | `#e6e2cc` |
| Everforest | Dark   | `#2d353b` | `#d3c6aa` | `#a7c080` | `#e67e80` | `#dbbc7f` | `#475258` |
| GNOME      | Light  | `#fafafb` | `#2e3436` | `#26a269` | `#c01c28` | `#e5a50a` | `#deddda` |
| GNOME      | Dark   | `#222226` | `#ffffff` | `#33d17a` | `#f66151` | `#f6d32d` | `#3d3846` |
| Nord       | Light  | `#eceff4` | `#3b4252` | `#456035` | `#bf616a` | `#d08770` | `#d8dee9` |
| Nord       | Dark   | `#2e3440` | `#d8dee9` | `#a3be8c` | `#bf616a` | `#ebcb8b` | `#434c5e` |

- **Surfaces stay the session's.** Windows keep egui's system-derived surfaces and text, so they
  look native next to other apps. The palette sets the status colours (charging, low, warn), the
  neutral for an ordinary reading, the bar track and the tray icon's colours. Its `bg` is painted
  only behind the swatches of the Appearance tab's palette tiles.
- **One dim factor.** `palette::DIM` (0.70) dims the icon's retained fill, the offline icon, and a
  dashboard bar and track that are not live. A low reading never dims.
- **Substitutions.** Everforest light charging is the palette's aqua `#35a77c`, not its green
  `#8da101`: lifted to 3:1 on a light panel the green turned olive (`#5a6701` on the icon), the
  aqua stays a green (`#236e52`). Nord dark low stays red `#bf616a` (4.08:1 on the nominal dark
  panel as is); orange `#d08770` would read louder (5.86:1) but it is Nord light's warn, and it
  needs a lift as window text too (3.88:1 → `#d69783`), so it would not be less muted there.

#### Readability

`palette::readable(color, surface, min_ratio, opacity)` moves a colour along its own hue — a mix
toward black on a light surface, toward white on a dark one — until, painted at `opacity` over
`surface`, it reaches `min_ratio`. A colour that already passes stays as the table has it.

- **Windows** (`gui::status_colors`): `low` and `warn` are text, ≥ 4.5:1 opaque on the panel, the
  group fill and a button (`gui::text_surfaces`). `fg` and `charging` are bars, ≥ 3:1 on the
  panel at `DIM`. `track` is used as is.
- **Tray icon** (`Theme::new`): the panel is the host's, which rigbat cannot see. The icon keeps
  its approach: outline, nub, digits and silhouette in the full status colour, the neutral `fg` for an
  ordinary reading and for offline. Each colour reaches ≥ 3:1 on a nominal panel (`#1e1e1e` dark,
  `#f0f0f0` light), an assumption standing in for a measurement; `fg` and `charging` are measured at `DIM`, `low` opaque.

What the rule changes — windows on egui's default surfaces, the icon on the nominal panel (lowest
ratio before → after):

| Palette, scheme  | Role (use)      | Table → rendered      | Ratio       |
| ---------------- | --------------- | --------------------- | ----------- |
| Catppuccin light | low (text)      | `#d20f39` → `#cd0f38` | 4.35 → 4.52 |
| Catppuccin light | warn (text)     | `#df8e1d` → `#905b13` | 2.10 → 4.56 |
| Catppuccin light | charging (bar)  | `#40a02b` → `#2f7520` | 2.19 → 3.02 |
| Catppuccin light | charging (icon) | `#40a02b` → `#2d701e` | 2.09 → 3.02 |
| Everforest light | low (text)      | `#f85552` → `#b43e3c` | 2.62 → 4.55 |
| Everforest light | low (icon)      | `#f85552` → `#f25350` | 2.87 → 3.01 |
| Everforest light | warn (text)     | `#dfa000` → `#866100` | 1.84 → 4.52 |
| Everforest light | fg (bar)        | `#5c6a72` → `#58656d` | 2.88 → 3.02 |
| Everforest light | fg (icon)       | `#5c6a72` → `#546169` | 2.79 → 3.01 |
| Everforest light | charging (bar)  | `#35a77c` → `#257356` | 2.05 → 3.01 |
| Everforest light | charging (icon) | `#35a77c` → `#236e52` | 1.95 → 3.01 |
| Everforest dark  | low (text)      | `#e67e80` → `#e98c8d` | 4.02 → 4.52 |
| GNOME light      | warn (text)     | `#e5a50a` → `#866106` | 1.73 → 4.52 |
| GNOME light      | charging (bar)  | `#26a269` → `#1b754c` | 2.16 → 3.03 |
| GNOME light      | charging (icon) | `#26a269` → `#1a7049` | 2.07 → 3.03 |
| GNOME dark       | low (text)      | `#f66151` → `#f88579` | 3.54 → 4.53 |
| Nord light       | low (text)      | `#bf616a` → `#9e5057` | 3.28 → 4.50 |
| Nord light       | warn (text)     | `#d08770` → `#8c5b4b` | 2.28 → 4.53 |
| Nord dark        | low (text)      | `#bf616a` → `#d4959b` | 2.70 → 4.50 |

Every other table colour is used as published.

### Typography

The windows draw in egui's bundled fonts, not the session's: a deliberate exception to "Follow the
session", for the reasons the tray digits embed League Gothic. Cyrillic coverage is guaranteed,
painted-text tests are deterministic, and there is no fontconfig dependency. GNOME asks for the
system font; rigbat does not follow it here. The emoji fonts in egui's default set stay:
`LOW_SIGN`, `CHARGING_SIGN`, `REFRESH`, "↺" and the kind emoji (`gui::kind_glyph`) are text and
come from them. Every catalogue character must have a glyph in them
(`every_catalogue_character_is_in_the_bundled_fonts`). egui breaks a line at U+202F, which is why
a unit joins its number with U+00A0 ([Text](#text)).

| Role                     | Style                                                         | Where                          |
| ------------------------ | ------------------------------------------------------------- | ------------------------------ |
| Body, row title, caption | `TextStyle::Body`                                             | `src/settings/widgets.rs`      |
| Tab label                | `TextStyle::Button`                                           | `tab_bar`                      |
| Group title              | Body, `.strong()`                                             | `group`                        |
| Secondary                | Body × `SECONDARY_SCALE` (0.88), `gui::secondary_text`        | `secondary`, `footer`          |
| Charge value             | Body; `.strong()` when live or low (`gui::charge_value_text`) | `render_row`, `Rows::expander` |
| Dashboard name           | Body, `.strong()`, truncated                                  | `render_row`                   |
| Devices row name         | Body, truncated                                               | `Rows::expander`               |
| Dashboard note           | `NOTE_SIZE` (12), `gui::secondary_text`                       | `render_row`                   |
| Kind emoji               | `gui::GLYPH_SIZE` (20)                                        | `render_row`, `Rows::expander` |

## Components

The settings building blocks live in `src/settings/widgets.rs`.

| Component                             | Draws                                                                                                                                                                                                                                                                                  | Use for                                                                                  |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `page(ui, id_salt, add)`              | A centred column at most `CONTENT_MAX_WIDTH` wide in a vertical scroll area, `PAGE_PADDING` above and below                                                                                                                                                                            | Every settings tab                                                                       |
| `group(ui, title, footer, add_rows)`  | Strong title, rounded filled box of rows split by hairlines, optional secondary footer under the box                                                                                                                                                                                   | Every section of a settings page                                                         |
| `Rows::row(title, subtitle, control)` | Title and subtitle on the left, vertically centred; one control on the right; at least `ROW_MIN_HEIGHT`                                                                                                                                                                                | A single setting                                                                         |
| `Rows::expander(header, control)`     | Kind glyph, title with an optional note under it, the value, a chevron and one `control`; a click anywhere but on `control` is in the returned response                                                                                                                                | A device, expanding in place to its settings                                             |
| `Rows::nested(add)`                   | Rows at `NESTED_ROW_HEIGHT`, indented by `GLYPH_COLUMN`, no separators                                                                                                                                                                                                                 | An expanded row's settings                                                               |
| `Rows::block(title, content)`         | Title above content that spans the row                                                                                                                                                                                                                                                 | A control too wide for the right edge (the tile picker)                                  |
| `subtitle(text)` / `none`             | One secondary line that wraps if it must / nothing                                                                                                                                                                                                                                     | The `subtitle` argument of `Rows::row`                                                   |
| `secondary(ui, text)`                 | Text at `SECONDARY_SCALE` in `gui::secondary_text`                                                                                                                                                                                                                                     | Hints and footers, including custom subtitles                                            |
| `command(ui, command, copy, copied)`  | A shell command in monospace secondary text and under it a "Copy" button that reads "Copied" for `COPIED_FOR`                                                                                                                                                                          | A command the user needs to run (the systemd hand-off, the udev fix)                     |
| `switch(ui, id, on, label)`           | Animated pill switch; `id` is global so a test can find it, `label` is what a screen reader announces                                                                                                                                                                                  | A boolean that applies at once                                                           |
| `tile(…)` + `tile_width`              | Image above caption, accent outline when selected                                                                                                                                                                                                                                      | A choice whose options are best shown as pictures (icon style, palette)                  |
| `tab_bar(ui, labels, selected)`       | Centred tabs, accent underline on the selected one, full-width rule below                                                                                                                                                                                                              | A view switcher (GNOME's term): a window's fixed top-level views, not a set of documents |
| `trailing(ui, width, control)`        | A fixed-width, left-to-right box at the row's right edge                                                                                                                                                                                                                               | A composite control, such as a slider with its value                                     |
| `footer(ui, text, commit, link)`      | One centred secondary line: text, an optional `CommitHash`, a link, split by `·`; returns the link's response, the caller opens the target. The hash copies itself on click and reads "Copied" for `COPIED_FOR` (1.5 s) in a slot as wide as the wider text, so the line does not move | Version, commit and GitHub link at the bottom of General                                 |

Stock egui widgets fill the other roles inside a row: `egui::ComboBox` for a list of values (poll
interval, offline period, language, theme), `egui::Slider` with a unit suffix inside `trailing` for a range (low-battery
threshold), `ui.button` for an action, in a subtitle too (never `ui.small_button`, which is
shorter than `MIN_TARGET`).
The poll interval and the threshold are the same control wherever they appear: `interval_combo`
over the one `POLL_INTERVAL_PRESETS` list and `threshold_slider`, both in
`src/settings/general_tab.rs`. Every on/off control is a `switch`; the settings window has no
checkbox.

Patterns from `src/settings/general_tab.rs`:

- **Save on commit.** Switches, tiles and combo boxes save on change; a slider saves on
  `drag_stopped()` or `lost_focus()`, never per dragged pixel (GNOME applies a text field on Return or focus loss; a slider follows the same rule). Every save goes through
  `SettingsApp::persist`, which re-reads `config.json`, changes one field and writes it back; if
  the write fails, the controls keep showing what is on disk and a banner says why
  ([Errors and empty states](#errors-and-empty-states)).
- **A setting rigbat does not own is shown, disabled, with the owner named.** With
  `rigbat.service` enabled, the autostart switch is disabled, and its subtitle names the service
  and shows the command that turns it off, with a button that copies it (`widgets::command`,
  `render_autostart_row`).
- **Hover text repeats, never holds the only copy.** A fact the user needs — a command, a
  device's kind, transport or last-seen date — is painted in the window; COSMIC shows no
  tooltips and a tooltip needs a pointer.
- **A setting owned by another tab is named where it takes effect** (project choice). The "One icon per device"
  subtitle names the pinned device and points at the Devices tab (`aggregate_icon_hint`).
- **Defaults with per-device overrides** (project choice) say so in the group footer (`defaults-hint`). On the
  Devices tab an override row shows the effective value; its subtitle says "The default for all
  devices" until the device has its own value, then "Default: 20%" and a "↺" button that
  clears it (`default_subtitle`). A value equal to the default is not stored
  (`apply_device_override`).

## Surfaces

### Settings window

`rigbat settings`, a separate process (`src/settings/`). A decorated window titled "rigbat —
Settings" (`settings-title`, retitled when the language changes), a `tab_bar` over three views
(`Tab::General`, `Tab::Appearance`, `Tab::Devices`), panel margin `PANEL_MARGIN`.
`rigbat settings appearance` (or `general`, `devices`) opens on that tab.

- **General** (`src/settings/general_tab.rs`), behaviour: groups Tray (one icon per device; hide
  an offline device after, a combo of `HIDE_OFFLINE_PRESETS`, 30 min to 24 h),
  Battery (low-battery threshold; check every; notifications; footer on defaults), System (start
  with session; language), then the `footer`. The column scrolls as a whole.
- **Appearance** (`src/settings/appearance_tab.rs`), look: groups Windows (a theme combo box,
  System / Light / Dark, its subtitle saying it covers this window and the device overview while
  the tray icon follows the system), Colours (palette tiles, each the palette's name over its
  `fg`, `charging`, `warn` and `low` on its own `bg` for the window's scheme; footer naming where
  the colours show), Tray icon (icon style tiles, drawn in the chosen palette). The settings
  window restyles as soon as the theme is saved (`SettingsApp::follow_theme` feeds `gui::follow`);
  the dashboard reads the theme when it opens. "One icon per device" stays on General: it is
  behaviour, not look.
- **Devices** (`src/settings/devices_tab.rs`): a toolbar (a search field past
  `SEARCH_MIN_DEVICES`, Refresh on the right: disabled while a scan runs, and reading "Refreshing…" once it has run `gui::PROGRESS_DELAY`), the unanswered-tray line in the warning colour,
  then two groups of `Rows::expander` rows in `roster_order`: "Connected now" (in the current
  scan) and "Seen before" (inventory only). An empty group is not drawn; no devices at all shows
  `devices-empty`.
  - A connected row: the value and note from `charge_value` / `status_note`, and a "Show in tray"
    `switch` (the inverse of `hidden_devices`). A seen-before row: "seen 2 d ago" and no switch.
    Kind and transport, and a seen-before row's last date, are the first row of the expanded
    device ("Type and connection", `device_about`); the collapsed row's hover text repeats them.
  - A click on the row (not on the switch) expands it in place; one row at a time. Expanded: kind and
    transport, "Pin to the tray icon" (the pin, with a subtitle saying it applies only to the single icon
    while per-device icons are on), the threshold and interval overrides, and "Remove from the
    list" with "Remove…", which arms an inline confirmation: "Cancel", then "Remove" in the low
    colour (cancel first, as GNOME orders dialog buttons); the first click never deletes. A device
    the inventory has not recorded has no Remove row.
  - Confirm, not undo, on purpose: GNOME prefers undo, but removal deletes the reading history
    for good, and a deferred delete with an undo line is more machinery than a rare action earns.
    GNOME allows a confirmation for an irreversible action.
- Esc cancels an armed removal, then collapses the open row, then clears the search, then closes
  the window (`escape_action`). Esc closing a window that is not a dialog is a project choice;
  GNOME binds Esc to a dialog's cancel and Ctrl+W to closing a window.

### Dashboard

`rigbat dashboard`, opened by a left click on a tray icon (`src/dashboard/mod.rs`). It reads the
running tray's state over the session bus and never polls a device.

- Titled "rigbat — Device overview" (`dashboard-title`), the tray menu's item. Undecorated,
  `WINDOW_WIDTH` wide, exactly as tall as its rows plus footer (`window_size`), and it
  resizes when a device comes or goes (`fit_window`), up to `MAX_VISIBLE_ROWS` rows. The footer is
  pinned to the bottom and the rows scroll above it whenever the window is shorter than they are:
  past the cap, or when the compositor ignores the resize (COSMIC, see `CLAUDE.md`).
- Closes like a popup: Esc, or losing focus after having had it (`close_like_a_popup`). A second
  click on the icon closes it. Project choice: no guideline covers closing a toplevel window on
  focus loss.
- A row (`render_row`): kind glyph (`kind_glyph`) in its own column; the name (strong, truncated,
  never wrapped) and the value (`charge_value`, right-aligned) on the first line; the charge bar
  and the note (`status_note`, right-aligned) on the second. The value is strong when online,
  coloured `StatusColors::low` when low, secondary otherwise. The bar fills in the status colour,
  the palette neutral for an ordinary reading, over the palette's track.
- Kind, transport and tray membership are in the row's hover text (`details`), not in the row.
- Footer: a "↻" button (`refresh_button`: the `REFRESH` font glyph, not a symbolic icon) that a
  screen reader announces, and hover shows, as "Refresh"; a spinner once the refresh has run
  `gui::PROGRESS_DELAY` (300 ms, `gui::progress_wait`, shared with the Devices tab's Refresh) and until it ends, at most `REFRESH_SPINNER_LIMIT` (GNOME: a spinner
  shown for a moment distracts); with reduced motion, the still text "Refreshing…" instead
  (`progress`); "Settings" on the right.
- Empty states say why and what to do ([Errors and empty states](#errors-and-empty-states)): the
  tray is not running, with a "Start tray" button; there are no devices, with how one appears.
  The footer stays.

### Tray menu

Built by `RigbatTray::menu` from a `View` (`src/tray/item.rs`).

- One row per visible device, in roster order: `device_line` (name, value, note) with the kind's
  freedesktop icon (`freedesktop_icon_name`); `DeviceKind::Other` is `battery`, as in the windows.
- Single icon (`TrayMode::PrimaryOnly`): "Automatic" then every device as a `CheckmarkItem`. The
  checked item is what the icon shows; clicking a device pins it, "Automatic" clears the pin. A
  one-of choice is a radio group elsewhere; checkmarks are the COSMIC workaround below.
- One icon per device (`TrayMode::PerDevice`): device rows are plain `StandardItem`s that open the
  dashboard.
- The tail is fixed: separator, "Device overview", "Refresh", "Settings", separator, "Quit".
  Quit comes last, as Microsoft orders a notification-area menu; the default action (the
  overview) is the left click, so it is not the first item.
- No devices: one disabled "No devices" item.
- Labels go through `mnemonic_escape` (a single `_` would be swallowed). The fixed items have no
  access keys, which GNOME asks of every menu item. Only `StandardItem` and
  `CheckmarkItem` are used: COSMIC drops clicks on `RadioGroup` and nested submenus (see
  [CLAUDE.md](../CLAUDE.md#platform-gotchas)).

### Tray icon

Rendered by `TinySkiaRenderer` behind the `IconRenderer` port (`src/icon/mod.rs`) at 22, 24, 32,
44 and 64 px, on a square canvas (host practice; the SNI spec only asks for ARGB32 pixmaps).

- Three `DisplayMode`s, in `DisplayMode::ALL` order: `IconOnly` (battery with a fill bar, the
  default), `DeviceAndBattery` (the kind's silhouette over a thin battery), `PercentOnly` (digits
  as large as fit, drawn from the embedded League Gothic outlines in `icon::digits`: `100` in its
  condensed width so it keeps the height of two digits; digits sit apart by their ink, not their
  advances). Only `DeviceAndBattery` shows the kind; `percent_in_icon`, a removed mode,
  loads as `PercentOnly`.
- `DeviceAndBattery` at 22 px: a 14 × 14 `icon::silhouette` bitmap, one pixel per cell,
  centred on the top edge (rows 0–13); the bar's outline over rows 16–21 with the nub in the
  last column, the fill one pixel clear of the outline. Other sizes scale the 22 px layout and
  round to whole pixels. `DeviceKind::Other` has no silhouette and renders as `IconOnly`.
- Colour from `PrimaryStatus` through `Theme::new(palette, scheme)` ([Readability](#readability)).
  Offline is a crossed battery (`draw_cross_line`) in the neutral at `DIM`; in
  `DeviceAndBattery` the silhouette over an empty bar with one diagonal slash.
- Status marks (`status_mark`), drawn whole in the full status colour behind a clear halo: a bolt
  for charging — the icon's `CHARGING_SIGN`. Low has no mark; its colour alone sets it apart.
  `IconOnly` puts the mark in the middle of the body; `DeviceAndBattery` in the top-right corner,
  its halo cut out of the silhouette; `PercentOnly` bottom-left, with the digits above it. Cells
  are whole pixels (1 px at 22 px).
- A retained reading: a dashed outline (`IconOnly`, and the bar in `DeviceAndBattery`), dotted
  digits (`PercentOnly`, which has no outline: a clear 1 px grid cut through them, every 4 px up to
  24 px and every `size / 11` px above), and the fill dimmed by `DIM`; the status mark, nub
  and silhouette stay solid at full colour. `Low` renders as if live.
- SNI `Status` is `NeedsAttention` while a device the icon stands for is low and not charging
  (the single icon: any shown device; a per-device icon: its own), `Active` otherwise
  (`View::attention`). The spec names "battery charge running out" as the example; it marks low
  without colour, which the 22 px icon cannot spare pixels for. Never `Passive`: the levels are
  always shown.
- The icon cache key (`IconKey`) holds the resolved `Theme`, so a palette change re-renders every
  icon.
- The tray icon draws its own silhouettes; the windows use emoji (`gui::kind_glyph`), and the
  menu the freedesktop icon (`battery` for `DeviceKind::Other`).
- COSMIC shows no hover tooltip, so the SNI title names the device (`Tray::title`) and
  `DeviceAndBattery` shows its kind. The spec means `Title` to name the application; a per-device
  title is a COSMIC workaround. The tooltip carries `device_line` for hosts that show it.
- The Appearance tab's style tiles are rendered by the same renderer in the chosen palette
  (`render_style_previews`), so a preview cannot drift from the real icon; the
  `DeviceAndBattery` tile shows a mouse.

### Notifications

The low-battery notification (`src/notifications/`) is the only surface outside the tray and the
windows. Rules follow the
[Desktop Notifications spec](https://specifications.freedesktop.org/notification/latest/) and the
[GNOME HIG](https://developer.gnome.org/hig/patterns/feedback/notifications.html).

- **Urgency.** Normal at the device's threshold; critical only at `CRITICAL_PERCENT` (5 %) or
  below, where the device is about to switch off. The spec reserves critical for that kind of
  emergency, and hosts show it through Do Not Disturb. Expiry is the server's default (`-1`).
- **One per device.** The id `Notify` returns is passed back as `replaces_id`: a crossing that
  deepens to critical replaces its toast instead of stacking a second one.
- **Withdraw when stale.** `CloseNotification` when the device charges, reads `REARM_MARGIN`
  above its threshold, is hidden, or leaves the roster. A toast the server reports closed is
  forgotten, never replaced.
- **Default action.** A click on the body opens the device overview (never closes an open one:
  launching it toggles, so `org.rigbat.Dashboard` having an owner means do nothing), the same window as the
  tray's left click. No other buttons: actions must not duplicate the default one.
- **Identity.** Hints `desktop-entry = rigbat` (the shipped desktop file id) and
  `category = device`, so hosts group rigbat's notifications and can mute them per app.
- **Text.** The title alone names the device and the problem (`{ $name } battery low`); the body
  gives the level and what to do, in full sentences ("15% left. Charge it soon.").
- **Never the only channel.** The tray icon and the overview show the same low state; the
  notification only draws attention to it.

## Text

- Every UI string comes from `i18n/<lang>/rigbat.ftl` through `fl!`; `i18n/en` is the reference
  (project choice). The CLI stays English, and a wire value (`state_str`, `as_str`, `--json`) is
  never translated.
- Symbols are literals, identical in every language: `%`, `—` (no value), `·` between parts of a
  value or note, `: ` after a device name, `LOW_SIGN`, `CHARGING_SIGN`, `REFRESH`. `%` sits on the
  number in Russian too: a choice, since ГОСТ 8.417 spaces it and Russian editorial practice does
  not.
- **Case.** Sentence case for titles, labels and buttons ("Low battery threshold", "Start with
  session"), on purpose: COSMIC, the primary desktop, and Microsoft write it; GNOME, KDE,
  elementary and Apple capitalise control labels as headers. Russian writes sentence case anyway.
  A window title capitalises its part after the dash ("rigbat — Settings").
- **Ellipsis.** "…" ends a label only when the action needs more input or a confirmation before it
  completes: "Remove…" arms a confirmation. A label that only opens a window has none ("Settings",
  "Device overview"), as GNOME and Microsoft write "Preferences" and "Settings"; COSMIC's own apps
  keep it there, and rigbat does not follow them. A placeholder has none ("Search devices"). A
  running action says so with "…" ("Refreshing…"). Enforced by
  `only_a_confirmation_or_a_running_action_ends_with_an_ellipsis`.
- Hints state the consequence or where to change the setting: "Checking more often drains the
  device’s battery.", "Change it on the Devices tab." They end with a period: several hints are two
  sentences, and the one-sentence hints match them (elementary's consistency clause; Microsoft
  ends full sentences with a period).
- One word per thing: Russian calls the tray icon "значок" everywhere, never "иконка", and removal
  «Убрать» on the row title and both buttons (`removal_uses_one_verb_in_every_language`). A window
  title is "rigbat — " and the menu item that opens it (project choice;
  `a_window_title_is_the_menu_item_that_opens_it`). Russian quotes are «ёлочки».
- The language row's title is bilingual on purpose (`section-language`), so it is findable in
  either language. A screen reader reads the other half in the active language's voice: egui does
  not mark the language of a part (WCAG 3.1.2; unverified on Orca).

The value (`charge_value`) and the note (`status_note`), English:

| Device state                  | Value                          | Note                   |
| ----------------------------- | ------------------------------ | ---------------------- |
| Online, discharging, estimate | `62%`                          | `~7 h left`            |
| Online, charging              | `⚡ 40%`                       | —                      |
| Online, full                  | `100% · full`                  | —                      |
| Online, low                   | `⚠ 15%`                        | estimate, if any       |
| Online, no reading            | `—`                            | —                      |
| Unreachable / Disconnected    | `Unreachable` / `Disconnected` | `last reading 2 h ago` |
| Not online and low            | `⚠ Unreachable`                | `last reading …`       |
| No access                     | `No access`                    | `run rigbat doctor`    |

- One wording per state. The tray menu row, the tray tooltip and the `--waybar` tooltip join name,
  value and note as `device_line`; a per-device icon whose device has left the roster reads
  `name: disconnected` (`absent_line`). No surface has its own state words.
- A retained reading shows the presence word, not a stale percentage; the note carries its age
  (project choice).
- The note says only what the value does not (project choice).
- **Capitalised presence words.** A presence word is capitalised only where it starts its own slot
  — the value column of the dashboard and the Devices tab (`Unreachable`, `⚠ Unreachable`). After
  `name: ` in `device_line` it stays lowercase: `mouse: no access · run rigbat doctor`. State words
  are lowercase everywhere (`100% · full`). The catalogues hold the lowercase form;
  `charge_value` capitalises its first letter in code. That is right for English and Russian; a
  language whose case mapping differs (Turkish i/İ) needs its own capitalised messages.
- The Devices tab shows the tray's note for a connected device (`~7 h left`, `last reading 2 h
ago`) when it reads a running tray; its own poll, without one, has no estimate and no note.
- **Numbers and units.** A number and its unit are joined by a no-break space so they never part:
  `~45 min`, `~7 h`, `>4 d` (`format_coarse`), "just now", `5 min ago`, `2 h ago`, `3 d ago`
  (`format_age`), `30 s`, `2 h` (intervals). GNOME prescribes U+202F, but egui (0.36 draws
  it as half a space) breaks a line at it and at every space but U+00A0, so the catalogues write
  U+00A0 (`only_u00a0_keeps_a_number_and_its_unit_on_one_row`; `{" "}`). Estimates and ages are
  coarse, so aged text changes at most every `AGE_STEP`.
- **Plurals.** A count next to a word goes through a Fluent selector over the language's CLDR
  categories (Russian: one, few, many), never a plural built in code. Russian spells durations
  out: `~5 часов`, `2 часа назад`, `3 дня назад`, `>4 дней`; «д» is no standard abbreviation.
  Interval values stay ГОСТ abbreviations (`30 с`, `5 мин`, `1 ч`): after «каждые» and «через» a
  spelled-out «1 минута» would not agree.
- **Width.** Text is painted whole and never overlaps, at the narrowest window, in every language,
  at text scale 1.0 and 2.0 ([Wrapping and text scale](#wrapping-and-text-scale)). A label may
  wrap; a translation is never abbreviated to fit. Device names are data, not translations: the
  dashboard and the Devices rows truncate them instead of wrapping, the case elementary allows
  for user text.

## States and accessibility

### Errors and empty states

A failure or an empty list says what happened and what to do, in the window; a log line alone is
not an error state.

- **Changes not saved.** `SettingsApp::save_problem`, a `widgets::banner` above every tab: the
  config file does not read (checked when the window opens, `SaveProblem::at_open`, and on a
  refused save — `config::Unreadable`, with the parser's message from `config::read`), the
  directory is not writable, or there is no home directory. Each says what to fix. The banner
  stays until a save succeeds.
- **An action that did not happen.** `SettingsApp::action_problem`, a banner under the save
  problem's: "Start with session" could not write or remove its autostart entry (the switch flips
  back; check that the autostart directory is writable), or a confirmed removal could not delete
  the device from the inventory (the row stays; try again, `rigbat doctor` checks the state
  database). Each shows the error under the advice and stays until the same action succeeds.
- **No access.** When any device reads `NoAccess`, the Devices tab shows a banner: how many
  devices rigbat cannot read, that USB devices need a udev rule, the command that installs it
  (`domain::INSTALL_UDEV_RULE`, the same constant `rigbat doctor` prints) with a copy button
  (`widgets::command`), and "How to fix", which opens the README's Permissions section through the
  portal. The row note stays "run rigbat doctor", the wording every surface shares.
- **Tray not running.** The dashboard says so and offers "Start tray", which runs
  `launch::spawn("tray")`; the window follows the tray's bus name and fills in once it serves
  its state.
- **No devices.** The dashboard says "No devices" and "Connect a device and it appears here.";
  the Devices tab says "Connect a device, then press Refresh." An empty dashboard is
  `EMPTY_ROWS` (2) rows tall and keeps its footer.

### Contrast and reduced motion

Both follow the portal's `org.freedesktop.appearance` keys, read and followed with the colour
scheme (`appearance::follow_portal`), live, in both windows.

- **`contrast = 1`** (`Appearance::high_contrast`): `gui::apply` paints every text, weak text,
  widget foreground and hairline in the scheme's strongest text colour (`raise_contrast`), and
  `gui::targets` turns `palette::Targets::NORMAL` (4.5:1 text, 3:1 bars) into `Targets::HIGH`:
  7:1 text (WCAG AAA 1.4.6) and 4.5:1 bars, which `gui::status_colors` reaches through
  `palette::readable`. The tray icon is unchanged: it sits on a panel rigbat cannot see.
- **`reduced-motion = 1`** (`Appearance::reduced_motion`): egui's `animation_time` is zero, so
  the switch and combo boxes jump instead of slide, and the dashboard's spinner is the still text
  "Refreshing…" (`progress`).
- A retained reading never relies on opacity alone, at any contrast: in both windows its value is
  the presence word ("Unreachable", "Disconnected", "⚠ Unreachable"), never a percentage, and
  its note names the reading's age ("last reading 2 h ago"). `DIM` on the dashboard bar is an
  extra cue; under high contrast the dimmed bar still clears 4.5:1.

### Target size

Every pointer target is at least `gui::MIN_TARGET` (24) × 24 points (WCAG 2.2 SC 2.5.8):
`gui::apply` sets egui's `interact_size.y` to it, which every stock button, combo box, slider and
text field takes; `SWITCH_SIZE` is 24 tall; a glyph button ("↻", "↺") has a square minimum
size; the commit hash's slot is 24 tall. `ui.small_button` is not used. The footer's "GitHub"
link is the one exception the criterion allows, a link in a line of text spaced clear of other
targets. `egui_test::assert_targets_at_least` checks every AccessKit node with a control role.

### Wrapping and text scale

- A label may wrap; a translation is never abbreviated to fit. Nothing may be cut or overlap,
  which every surface test asserts (`assert_whole`) in every language at text scale 1.0 and 2.0
  (WCAG 1.4.4).
- A window never needs more than the smallest supported screen (GNOME: 1024 px wide) at any text
  scale: `gui::scaled` grows a window size with the text scale but caps the width at
  `SMALLEST_SCREEN_WIDTH`. At scale 2.0 the settings window's minimum is 1024 px, 512 of its own
  points, and its rows fit that width: titles and subtitles wrap, the control stays at the
  right. The dashboard (380 points) is 760 px wide at 2.0.
- A secondary button in a subtitle sits on its own line under the text, not after it, so a
  wrapped hint never runs into it.

### Screen readers

Both windows export an AT-SPI tree through eframe's `accesskit` feature (egui 0.36.2,
accesskit 0.24.1, accesskit_atspi_common 0.18.1).

- **Hover text is not a description.** egui never calls `set_description`
  (`egui-0.36.2/src/response.rs`, `fill_accesskit_node_from_widget_info`); a tooltip exists only
  while a pointer hovers, as nodes of its own. So hover text reaches no screen reader, which is
  why no fact lives only there.
- **Live regions work.** `accesskit::Node::set_live` is honoured: the AT-SPI adapter emits an
  `Announcement` when a live node appears with a name and whenever its name changes
  (`accesskit_atspi_common-0.18.1/src/adapter.rs`, `node.rs` `notify_property_changes`); a
  `Label` node's name is its value. `gui::live_region` marks a node polite. Live: each dashboard
  row ("MX Anywhere 3: Unreachable", so a device going offline or low is spoken), the dashboard's
  refresh status ("Refreshing…", then "Device list updated"; not painted, the list itself shows
  it), the settings banners' titles, and the Devices tab's unanswered-tray line.
- Unverified on Orca: the announcements are tested on the AccessKit tree, not heard.

## Testing UI

Window tests run headless on `src/egui_test.rs` and assert what was painted, not that code ran.

- `fully_painted_text_at` returns every string drawn whole with its rect and line count;
  `painted_text_at` returns every string at least `MIN_READABLE_WIDTH` visible. Both run two
  frames, because some widgets size themselves from the previous one.
- `assert_whole` fails on an expected string that is missing or cut, or on two overlapping
  strings; wrapping is allowed. `assert_no_overlap` checks only overlap.
- A surface test lists every string it expects, then loops over `Lang::ALL` and
  `egui_test::TEXT_SCALES` (1.0 and 2.0). Examples:
  `general_tab_text_is_whole_in_every_language_and_text_scale`,
  `appearance_tab_text_is_whole_in_every_language_and_text_scale`,
  `device_rows_are_whole_in_every_language_and_text_scale`,
  `an_expanded_row_paints_its_settings_in_every_language_and_text_scale`,
  `every_row_state_renders_whole_in_every_language_and_text_scale`, `every_widget_paints_its_text_whole`.
- Test sizes are the real constraints: every settings tab at the minimum window width in the
  window's own zoomed points (`tab_size(scale, height)`: 672 at 1.0, 512 at 2.0), the widgets at
  `CONTENT_MAX_WIDTH`, the dashboard at its own `wanted_size()`.
- Interaction: `run_frame` lays the page out, `ctx.read_response` finds a widget by its global id
  (`switch_id`, `device_switch_id`, an expander's row id), `click_at` or a key event drives it, and
  the test asserts the saved config and the announced `WidgetInfo`
  (`a_switch_toggles_from_the_keyboard_and_carries_its_label`,
  `a_click_expands_one_row_at_a_time_and_escape_collapses_it`,
  `pin_interval_and_reset_edit_the_config`, `the_refresh_button_announces_the_word_not_the_glyph`).
- The catalogues are tested as text (`i18n::tests`): the same keys in every language, the
  ellipsis rule, one removal verb, window titles that match their menu items; plurals and units
  in `domain::text` and `domain::estimate` (`russian_ages_take_the_plural_form_of_their_count`,
  `a_number_never_parts_from_its_unit`).
- States and accessibility: `a_refused_save_is_shown_until_a_save_succeeds`,
  `no_access_shows_the_fix_command_and_the_docs_in_every_language_and_text_scale`,
  `without_a_tray_or_devices_it_says_why_and_what_to_do_in_every_language`,
  `start_tray_starts_the_tray`, `high_contrast_raises_text_and_hairlines_to_7_to_1`,
  `reduced_motion_turns_animation_off`, `with_reduced_motion_progress_is_still_text_not_a_spinner`,
  `progress_shows_only_after_the_delay`, `every_*_target_is_at_least_24_points` (per surface, on
  the AccessKit tree via `egui_test::targets_at`), `each_row_is_a_live_region_with_its_name_and_value`,
  `a_finished_refresh_is_announced`, `a_low_device_that_is_not_charging_needs_attention` (tray).
- Contrast, colours and glyphs have their own tests: `secondary_text_is_readable_in_both_themes`
  (`gui` for the rule, `widgets` for what `secondary` paints),
  `every_palette_status_colour_is_readable_where_the_window_paints_it`,
  `every_palette_icon_colour_clears_3_to_1_on_the_nominal_panel_dimmed_included`,
  `a_low_reading_paints_its_value_and_bar_in_the_palette_low_colour`,
  `a_switch_knob_takes_its_colours_from_the_theme`,
  `apply_sets_theme_zoom_and_accent_in_both_styles`, `every_glyph_is_in_the_bundled_fonts`,
  `the_reset_glyph_is_in_the_bundled_fonts`.
- The theme: `a_forced_theme_wins_over_the_portal_until_it_is_system_again` and
  `a_theme_fixed_at_launch_still_follows_the_portal` (`gui::follow`),
  `choosing_light_restyles_the_window_over_a_dark_portal` (settings),
  `a_forced_theme_overrides_the_portal_scheme` (dashboard),
  `the_icon_scheme_ignores_the_window_theme` (tray).
- Kind emoji: `every_kind_paints_its_emoji_whole` (dashboard and Devices tab) finds each kind's
  emoji painted whole inside the glyph column.
- The tray icon is tested on its pixels: `charging_differs_from_ok_in_shape_and_low_in_colour_alone`,
  `a_low_reading_keeps_the_full_size_digits` and `a_retained_reading_differs_in_shape_in_every_mode`
  render with a one-colour theme and compare masks; `the_status_mark_is_drawn_whole`,
  `the_silhouette_and_the_bar_keep_to_their_rows`, `every_kind_has_its_own_device_and_battery_icon`,
  `only_device_and_battery_draws_the_kind`.
  `cargo test dump_icons -- --ignored` writes contact sheets of every state, mode, palette and
  scheme, and `rigbat-icons.txt` with each 22 px icon as text, to the temp directory.
- The tray menu is tested as text: `describe` renders it one line per item (`[x]` checkmark,
  `#icon`, `---` separator) and a test compares whole menus per mode.
