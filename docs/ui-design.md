# UI design

rigbat's UI conventions as the code implements them. Every rule names the file that implements
it; when the two disagree, the code wins and this document is out of date. Four surfaces share
these rules: the settings window, the dashboard, the tray menu and the tray icon. Where their data
comes from is in [`architecture.md`](architecture.md#data-flow-per-surface).

## Principles

| Principle                                | What the code does                                                                                                                                                                                                                                                                                                                                                                                                        | Where                                                                                   |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Follow the session, never theme it       | Both windows take the scheme, accent and text scale from xdg-desktop-portal and re-apply every change live. The accent becomes egui's selection colour; the text scale becomes the zoom factor and grows the window size. The tray icon follows the scheme. rigbat has no colour setting of its own.                                                                                                                      | `src/gui/mod.rs`, `src/appearance/mod.rs`                                               |
| Cap the content width                    | Both settings tabs are a centred column at most `CONTENT_MAX_WIDTH` wide (`page`); a wider window adds margin, not longer rows. The dashboard has a fixed width.                                                                                                                                                                                                                                                          | `src/settings/widgets.rs`                                                               |
| One trailing control per row             | `Rows::row` takes exactly one `control`. A secondary action on the same setting sits in the subtitle as a small button (the pin's "Clear" in the Tray group, a device override's "↺").                                                                                                                                                                                                                                    | `src/settings/widgets.rs`, `src/settings/general_tab.rs`, `src/settings/devices_tab.rs` |
| Status text carries a sign, not only hue | `charge_value` prefixes a low reading with `LOW_SIGN` (⚠) and a charging one with `CHARGING_SIGN` (⚡). The dashboard also colours a low value; the tray menu has no colour and relies on the sign.                                                                                                                                                                                                                       | `src/domain/text.rs`                                                                    |
| Same words on every surface              | The dashboard row, the Devices tab row, the tray menu row and the tray tooltip are built from `charge_value` and `status_note` (`device_line` joins them); both windows draw a device with the same kind glyph (`gui::kind_glyph`) and value style (`gui::charge_value_text`). Devices are ordered by `roster_order`: online first, then by name, ignoring case.                                                          | `src/domain/text.rs`, `src/domain/roster.rs`, `src/gui/mod.rs`                          |
| A low reading never gets quieter         | A retained (not live) reading is dimmed on the tray icon and the dashboard bar, except when it is low.                                                                                                                                                                                                                                                                                                                    | `src/icon/mod.rs`, `src/dashboard/mod.rs`                                               |
| Contrast is measured, not eyeballed      | `gui::contrast_ratio` (WCAG 2.1) backs tests: secondary text (`gui::secondary_text`, one rule for both windows) ≥ 4.5:1 on the panel and the group fill, text on the accent ≥ 4.5:1 (`readable_on` picks black or white). egui's weak text colour misses 4.5:1 on dark, so secondary text is the body colour at a smaller size. The icon `Theme` colours are chosen for ≥ 3:1 as graphical objects, dimmed fill included. | `src/gui/mod.rs`, `src/settings/widgets.rs`, `src/icon/mod.rs`                          |
| Custom widgets are accessible widgets    | `switch`, `tile`, the tabs and an expander row report a role and label through `widget_info` (checkbox, radio button, selectable label, collapsing header with its expanded state), take keyboard focus and draw a focus ring. A glyph-only button announces a word ("↺" is "Use the default"). Both windows export an AT-SPI tree through eframe's `accesskit` feature.                                                  | `src/settings/widgets.rs`, `Cargo.toml`                                                 |

Without a portal, windows use `Appearance::default`: dark scheme, no accent, text scale 1.0. A
portal reporting no preference maps to dark (`map_scheme`).

## Tokens

Values are egui points, multiplied by the session text scale through the zoom factor. Settings
tokens are private constants in `src/settings/widgets.rs` unless marked `pub`; dashboard tokens
are private constants in `src/dashboard/mod.rs`; tokens both windows use are `pub` in
`src/gui/mod.rs` (marked `gui::`).

### Spacing and size

| Token                       | Value     | Use                                                                                       |
| --------------------------- | --------- | ----------------------------------------------------------------------------------------- |
| `CONTENT_MAX_WIDTH` (`pub`) | 640       | Widest a settings tab's column gets                                                       |
| `PANEL_MARGIN` (`pub`)      | 16        | Settings window panel margin                                                              |
| `TAB_BAR_GAP` (`pub`)       | 8         | Tab bar to page                                                                           |
| `PAGE_PADDING`              | 8         | Above a page's first line and below its last                                              |
| `TOOLBAR_GAP` (`pub`)       | 8         | Devices toolbar and status line to the first group                                        |
| `gui::ROW_HEIGHT`           | 48        | Minimum row height in a group; a dashboard row; an expander row                           |
| `NESTED_ROW_HEIGHT`         | 36        | Minimum height of a row under an expanded row                                             |
| `gui::GLYPH_COLUMN`         | 30        | Kind-glyph column, dashboard and Devices rows; the indent of nested rows                  |
| `gui::GLYPH_SIZE`           | 20        | Kind glyph                                                                                |
| `CHEVRON_SIZE`              | 10        | Expander chevron box                                                                      |
| `ROW_PADDING_X`             | 14        | Row inset from the group box; half of it insets group titles and footers                  |
| `ROW_PADDING_Y`             | 8         | Least vertical padding around a row's text                                                |
| `ROW_GAP`                   | 16        | Between a row's text and its control                                                      |
| `GROUP_TITLE_GAP`           | 6         | Group title to box                                                                        |
| `GROUP_GAP`                 | 20        | After each group                                                                          |
| `FOOTER_GAP`                | 6         | Box to group footer text                                                                  |
| `FOOTER_SPACING`            | 4         | Between the page footer's text and its link                                               |
| `SWITCH_SIZE`               | 40 × 22   | Switch track                                                                              |
| `KNOB_INSET`                | 3         | Switch knob inset from the track                                                          |
| `TILE_HEIGHT`               | 76        | Picture tile                                                                              |
| `TILE_GAP` (`pub`)          | 10        | Between tiles                                                                             |
| `TILE_CAPTION_GAP`          | 8         | Tile image to caption, and caption side inset                                             |
| `TAB_PADDING`               | 12 × 6    | Around a tab label                                                                        |
| `TAB_UNDERLINE`             | 3         | Selected tab's underline thickness                                                        |
| `FOCUS_GAP`                 | 2.5       | Focus ring distance from the widget                                                       |
| `WINDOW_DEFAULT_SIZE`       | 720 × 640 | Settings window opening size (`src/settings/mod.rs`)                                      |
| `WINDOW_MIN_SIZE`           | 672 × 360 | Settings window minimum: `CONTENT_MAX_WIDTH` + 2 × `PANEL_MARGIN` (`src/settings/mod.rs`) |
| `SEARCH_MIN_DEVICES`        | 8         | Devices listed before the tab offers a search field (`src/settings/devices_tab.rs`)       |
| `WINDOW_WIDTH`              | 380       | Dashboard width                                                                           |
| `MARGIN`                    | 12        | Dashboard panel margin                                                                    |
| `ROW_PADDING`               | 5         | Dashboard row's vertical inset                                                            |
| `GAP`                       | 8         | Dashboard gap between glyph, name, value, bar and note                                    |
| `BAR_HEIGHT`                | 4         | Dashboard charge bar                                                                      |
| `FOOTER_HEIGHT`             | 32        | Dashboard footer                                                                          |
| `MAX_VISIBLE_ROWS`          | 10        | Dashboard rows before the list scrolls                                                    |

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

Window colours come from egui `Visuals` for the current scheme; rigbat only replaces the
selection colour with the accent (`gui::apply`).

| Role             | Source                                                                        | Used by                                                                                      |
| ---------------- | ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Accent           | `visuals.selection.bg_fill`                                                   | Switch on-track, selected tile outline, tab underline, dashboard bar for an ordinary reading |
| Group fill       | `visuals.faint_bg_color`                                                      | Group box                                                                                    |
| Tile fill        | `visuals.extreme_bg_color`                                                    | Tile                                                                                         |
| Bar track        | `extreme_bg_color` leaned `TRACK_TINT` (0.25) toward the fill (`track_color`) | Dashboard bar                                                                                |
| Switch off-track | `visuals.widgets.inactive.bg_fill`                                            | Switch                                                                                       |
| Switch knob      | The lighter of `extreme_bg_color` and `strong_text_color()` (`knob_fill`)     | Switch                                                                                       |
| Row hover        | `widgets.hovered.weak_bg_fill` × `HOVER_TINT` (0.5)                           | Expander row                                                                                 |
| Body text        | `visuals.text_color()`                                                        | Everything, and secondary text in both windows (`gui::secondary_text`)                       |
| Strong text      | `visuals.strong_text_color()`                                                 | Selected tab, selected tile caption                                                          |
| Weak text        | `visuals.weak_text_color()`                                                   | Idle tab                                                                                     |
| Warning / error  | `visuals.warn_fg_color` / `visuals.error_fg_color`                            | Devices tab: unanswered tray; armed "Remove"                                                 |
| Low value        | `Theme.low` (`gui::charge_value_text`)                                        | A low charge value, dashboard and Devices tab                                                |

Status colours are the icon `Theme` (`src/icon/mod.rs`), shared by the tray icon and the dashboard
so both classify a device the same way. RGBA, straight alpha:

| Field      | `Theme::dark()`      | `Theme::light()`     |
| ---------- | -------------------- | -------------------- |
| `normal`   | `#d8dee9`            | `#3b4252`            |
| `low`      | `#bf616a`            | `#bf616a`            |
| `charging` | `#a3be8c`            | `#456035`            |
| `offline`  | `#d8dee9`, alpha 180 | `#3b4252`, alpha 180 |

Dimming a retained reading: `STALE_ALPHA` (0.70) on the icon's fill, `DIMMED` (0.6) on the
dashboard bar.

### Typography

egui's bundled fonts; no font is loaded.

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
| Kind glyph               | `gui::GLYPH_SIZE` (20)                                        | `render_row`, `Rows::expander` |

## Components

The settings building blocks live in `src/settings/widgets.rs`.

| Component                             | Draws                                                                                                                                                   | Use for                                                        |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| `page(ui, id_salt, add)`              | A centred column at most `CONTENT_MAX_WIDTH` wide in a vertical scroll area, `PAGE_PADDING` above and below                                             | Every settings tab                                             |
| `group(ui, title, footer, add_rows)`  | Strong title, rounded filled box of rows split by hairlines, optional secondary footer under the box                                                    | Every section of a settings page                               |
| `Rows::row(title, subtitle, control)` | Title and subtitle on the left, vertically centred; one control on the right; at least `ROW_MIN_HEIGHT`                                                 | A single setting                                               |
| `Rows::expander(header, control)`     | Kind glyph, title with an optional note under it, the value, a chevron and one `control`; a click anywhere but on `control` is in the returned response | A device, expanding in place to its settings                   |
| `Rows::nested(add)`                   | Rows at `NESTED_ROW_HEIGHT`, indented by `GLYPH_COLUMN`, no separators                                                                                  | An expanded row's settings                                     |
| `Rows::block(title, content)`         | Title above content that spans the row                                                                                                                  | A control too wide for the right edge (the tile picker)        |
| `subtitle(text)` / `none`             | One secondary line that wraps if it must / nothing                                                                                                      | The `subtitle` argument of `Rows::row`                         |
| `secondary(ui, text)`                 | Text at `SECONDARY_SCALE` in `gui::secondary_text`                                                                                                      | Hints and footers, including custom subtitles                  |
| `switch(ui, id, on, label)`           | Animated pill switch; `id` is global so a test can find it, `label` is what a screen reader announces                                                   | A boolean that applies at once                                 |
| `tile(…)` + `tile_width`              | Image above caption, accent outline when selected                                                                                                       | A choice whose options are best shown as pictures (icon style) |
| `tab_bar(ui, labels, selected)`       | Centred tabs, accent underline on the selected one, full-width rule below                                                                               | Switching between a window's top-level views                   |
| `trailing(ui, width, control)`        | A fixed-width, left-to-right box at the row's right edge                                                                                                | A composite control, such as a slider with its value           |
| `footer(ui, text, link, url)`         | One centred secondary line ending in a link                                                                                                             | Version and project page at the bottom of General              |

Stock egui widgets fill the other roles inside a row: `egui::ComboBox` for a list of values (poll
interval, language), `egui::Slider` with a unit suffix inside `trailing` for a range (low-battery
threshold), `ui.small_button` for a secondary action in a subtitle, `ui.button` for an action.
The poll interval and the threshold are the same control wherever they appear: `interval_combo`
over the one `POLL_INTERVAL_PRESETS` list and `threshold_slider`, both in
`src/settings/general_tab.rs`. Every on/off control is a `switch`; the settings window has no
checkbox.

Patterns from `src/settings/general_tab.rs`:

- **Save on commit.** Switches, tiles and combo boxes save on change; a slider saves on
  `drag_stopped()` or `lost_focus()`, never per dragged pixel. Every save goes through
  `SettingsApp::persist`, which re-reads `config.json`, changes one field and writes it back; if
  the write fails, the controls keep showing what is on disk.
- **A setting rigbat does not own is shown, disabled, with the owner named.** With
  `rigbat.service` enabled, the autostart switch is disabled, its subtitle names the service and
  its hover text gives the command to turn it off (`render_autostart_row`).
- **A setting owned by another tab is named where it takes effect.** The "One icon per device"
  subtitle names the pinned device and points at the Devices tab (`aggregate_icon_hint`).
- **Defaults with per-device overrides** say so in the group footer (`defaults-hint`). On the
  Devices tab an override row shows the effective value; its subtitle says "The default for all
  devices" until the device has its own value, then "Default: 20%" and a "↺" small button that
  clears it (`default_subtitle`). A value equal to the default is not stored
  (`apply_device_override`).

## Surfaces

### Settings window

`rigbat settings`, a separate process (`src/settings/`). A decorated window titled "rigbat —
settings" (`settings-title`, retitled when the language changes), a `tab_bar` over two views
(`Tab::General`, `Tab::Devices`), panel margin `PANEL_MARGIN`. `rigbat settings devices` (or
`general`) opens on that tab.

- **General** (`src/settings/general_tab.rs`): groups Tray (icon style tiles; one icon per device),
  Battery (low-battery threshold; check every; notifications; footer on defaults), System
  (start with session; language), then the `footer`. The column scrolls as a whole.
- **Devices** (`src/settings/devices_tab.rs`): a toolbar (a search field past
  `SEARCH_MIN_DEVICES`, Refresh on the right), the unanswered-tray line in the warning colour,
  then two groups of `Rows::expander` rows in `roster_order`: "Connected now" (in the current
  scan) and "Seen before" (inventory only). An empty group is not drawn; no devices at all shows
  `devices-empty`.
  - A connected row: the value and note from `charge_value` / `status_note`, and a "Show in tray"
    `switch` (the inverse of `hidden_devices`). A seen-before row: "seen 2d ago" and no switch.
    Kind and transport, and a seen-before row's last date, are the row's hover text.
  - A click on the row (not on the switch) expands it in place; one row at a time. Expanded:
    "Show on the single icon" (the pin, with a subtitle saying it applies only to the single icon
    while per-device icons are on), the threshold and interval overrides, and "Remove from the
    list" with "Remove…", which arms "Remove" / "Cancel" (never deletes on the first click). A
    device the inventory has not recorded has no Remove row.
- Esc cancels an armed removal, then collapses the open row, then clears the search, then closes
  the window (`escape_action`).

### Dashboard

`rigbat dashboard`, opened by a left click on a tray icon (`src/dashboard/mod.rs`). It reads the
running tray's state over the session bus and never polls a device.

- Titled "rigbat — device overview" (`dashboard-title`), the tray menu's noun. Undecorated,
  `WINDOW_WIDTH` wide, exactly as tall as its rows plus footer (`window_size`), and it
  resizes when a device comes or goes (`fit_window`). Past `MAX_VISIBLE_ROWS` the list scrolls.
- Closes like a popup: Esc, or losing focus after having had it (`close_like_a_popup`). A second
  click on the icon closes it.
- A row (`render_row`): kind glyph (`kind_glyph`) in its own column; the name (strong, truncated,
  never wrapped) and the value (`charge_value`, right-aligned) on the first line; the charge bar
  and the note (`status_note`, right-aligned) on the second. The value is strong when online,
  coloured `Theme.low` when low, secondary otherwise. The bar fills in the status colour, the
  accent for an ordinary reading.
- Kind, transport and tray membership are in the row's hover text (`details`), not in the row.
- Footer: a "↻" button (`REFRESH`, hover text "Refresh") with a spinner while the refresh is in
  flight, at most `REFRESH_SPINNER_LIMIT`; "Settings…" on the right.
- Empty states say why in one line: the tray is not running; there are no devices.

### Tray menu

Built by `RigbatTray::menu` from a `View` (`src/tray/item.rs`).

- One row per visible device, in roster order: `device_line` (name, value, note) with the kind's
  freedesktop icon (`freedesktop_icon_name`).
- Single icon (`TrayMode::PrimaryOnly`): "Automatic" then every device as a `CheckmarkItem`. The
  checked item is what the icon shows; clicking a device pins it, "Automatic" clears the pin.
- One icon per device (`TrayMode::PerDevice`): device rows are plain `StandardItem`s that open the
  dashboard.
- The tail is fixed: separator, "Device overview…", "Refresh", "Settings…", separator, "Quit".
- No devices: one disabled "No devices" item.
- Labels go through `mnemonic_escape` (a single `_` would be swallowed). Only `StandardItem` and
  `CheckmarkItem` are used: COSMIC drops clicks on `RadioGroup` and nested submenus (see
  [CLAUDE.md](../CLAUDE.md#platform-gotchas)).

### Tray icon

Rendered by `TinySkiaRenderer` behind the `IconRenderer` port (`src/icon/mod.rs`) at 22, 24, 32,
44 and 64 px, on a square canvas.

- Three `DisplayMode`s: `IconOnly` (battery with a fill bar), `PercentOnly` (digits),
  `PercentInIcon` (battery outline with digits inside).
- Colour from `PrimaryStatus` through the `Theme` table above. Offline is a crossed battery
  (`draw_cross_line`), never dimmed.
- A retained reading dims only the fill, by `STALE_ALPHA`; outline, nub, digits and glyph stay at
  full colour. `Low` is never dimmed.
- The device-kind corner glyph (`maybe_draw_kind_glyph`) sits bottom-right; `DeviceKind::Other`
  gets none. In the percent modes the digits are drawn after the glyph and win the overlap.
- COSMIC shows no hover tooltip, so the SNI title names the device (`Tray::title`) and the glyph
  identifies its kind. The tooltip carries `device_line` for hosts that show it.
- The General tab's style tiles are rendered by the same renderer (`render_style_previews`), so a
  preview cannot drift from the real icon.

## Text

- Every UI string comes from `i18n/<lang>/rigbat.ftl` through `fl!`; `i18n/en` is the reference.
  The CLI stays English, and a wire value (`state_str`, `as_str`) is never translated.
- Symbols are literals, identical in every language: `%`, `—` (no value), `·` between parts of a
  value or note, `: ` after a device name, `LOW_SIGN`, `CHARGING_SIGN`, `REFRESH`.
- Sentence case for titles, labels and buttons ("Low battery threshold", "Start with session").
  An item or button that opens a window ends with "…" ("Settings…", "Device overview…"); a
  running action says so ("Refreshing…").
- Hints state the consequence or where to change the setting: "Checking more often drains the
  device's battery.", "Change it on the Devices tab."
- One word per thing: Russian calls the tray icon "значок" everywhere, never "иконка"; a window
  title uses the noun of the menu item that opens it.
- The language row's title is bilingual on purpose (`section-language`), so it is findable in
  either language.

The value (`charge_value`) and the note (`status_note`), English:

| Device state                  | Value                          | Note                  |
| ----------------------------- | ------------------------------ | --------------------- |
| Online, discharging, estimate | `62%`                          | `~7h left`            |
| Online, charging              | `⚡ 40%`                       | —                     |
| Online, full                  | `100% · full`                  | —                     |
| Online, low                   | `⚠ 15%`                        | estimate, if any      |
| Online, no reading            | `—`                            | —                     |
| Unreachable / Disconnected    | `Unreachable` / `Disconnected` | `last reading 2h ago` |
| Not online and low            | `⚠ Unreachable`                | `last reading …`      |
| No access                     | `No access`                    | `run rigbat doctor`   |

- A retained reading shows the presence word, not a stale percentage; the note carries its age.
- The note says only what the value does not.
- Estimates are coarse (`format_coarse`: `~Nm`, `~Nh`, `>Nd`); ages likewise (`format_age`: "just
  now", `Nm ago`, `Nh ago`, `Nd ago`), so aged text changes at most every `AGE_STEP`.
- **Width.** Text is painted whole, on one line, at the narrowest window, in every language.
  Russian runs 20–35% longer than English; when a string does not fit, shorten the translation
  rather than widen the layout ([CONTRIBUTING](../CONTRIBUTING.md#adding-a-translation)). Device
  names are data, not translations: the dashboard truncates them instead of wrapping.

## Testing UI

Window tests run headless on `src/egui_test.rs` and assert what was painted, not that code ran.

- `fully_painted_text_at` returns every string drawn whole with its rect and line count;
  `painted_text_at` returns every string at least `MIN_READABLE_WIDTH` visible. Both run two
  frames, because some widgets size themselves from the previous one.
- `assert_single_lines_without_overlap` fails on a wrapped string or two overlapping ones;
  `assert_no_overlap` checks only overlap.
- A surface test lists every string it expects, then loops over `Lang::ALL`: each must appear with
  `lines == 1`. Examples: `general_tab_text_is_whole_on_one_line_in_every_language`,
  `device_rows_are_whole_on_one_line_in_every_language`,
  `an_expanded_row_paints_its_settings_in_every_language`,
  `every_row_state_renders_whole_on_one_line_in_every_language`,
  `every_widget_paints_its_text_whole_on_one_line`. Subtitles may wrap, so they are checked for
  being painted whole, not for one line.
- Test sizes are the real constraints: both settings tabs at the `WINDOW_MIN_SIZE` width
  (`GENERAL_TAB_TEST_SIZE`, `TEST_SIZE` in `devices_tab`), the widgets at `CONTENT_MAX_WIDTH`, the
  dashboard at its own `wanted_size()`.
- Interaction: `run_frame` lays the page out, `ctx.read_response` finds a widget by its global id
  (`switch_id`, `device_switch_id`, an expander's row id), `click_at` or a key event drives it, and
  the test asserts the saved config and the announced `WidgetInfo`
  (`a_switch_toggles_from_the_keyboard_and_carries_its_label`,
  `a_click_expands_one_row_at_a_time_and_escape_collapses_it`,
  `pin_interval_and_reset_edit_the_config`).
- Contrast, colours and glyphs have their own tests: `secondary_text_is_readable_in_both_themes`
  (`gui` for the rule, `widgets` for what `secondary` paints),
  `a_switch_knob_takes_its_colours_from_the_theme`,
  `apply_sets_theme_zoom_and_accent_in_both_styles`, `every_glyph_is_in_the_bundled_fonts`,
  `the_reset_glyph_is_in_the_bundled_fonts`.
- The tray menu is tested as text: `describe` renders it one line per item (`[x]` checkmark,
  `#icon`, `---` separator) and a test compares whole menus per mode.
