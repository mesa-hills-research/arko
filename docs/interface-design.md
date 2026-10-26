# Interface design

Arko has three interfaces, Arko Studio (GPUI and GPUI Kit), the web page (Rust and Leptos, in
`web/`) and the terminal view (ratatui). This guide keeps them one product. It follows GPUI Kit's
[design guide](https://github.com/mesa-hills-research/gpui_kit/blob/main/website/docs/design-guides.md),
and where the two differ for Arko, this guide wins. When something isn't covered, do what Studio
does.

## Principles

1. The transcript is the product. Chrome stays small and carries only what the user acts on.
2. Show what is unusual, not what is normal. A failed call, a waiting question, outside access or a
   cache about to lapse shows. "Connected" and "Live" don't.
3. Nothing moves under the reader. Streaming never shifts what is being read, the view follows the
   bottom only while the reader is there, and a finished row never changes height.
4. Disclose progressively. Details are one click or tap away, never a toolbar of settings.
5. Everything works from the keyboard, focus always shows, and Esc closes the innermost layer.
6. Theme tokens only. No raw colors, radii or spacing in feature code.
7. Motion carries state and nothing else (see [Motion](#motion)).
8. Few words (see [Text](#text)).

## Theme tokens

Colors come from GPUI Kit's theme files in `themes/`, the same files and names in every interface.
Studio reads them through `cx.theme()`, the terminal through `tui::theme::Roles`, and the web page
as CSS variables that `web/theme-css` writes. Each variable takes the first token a theme sets.

| CSS variable | Theme tokens, in order | Used for |
|---|---|---|
| `--bg` | `background` | The window or page |
| `--bar` | `title_bar.background`, `background` | Header and status bars |
| `--bar-line` | `title_bar.border`, `border` | The bars' hairline |
| `--surface` | `highlight.editor.background`, `background` | Fields, cards, docked blocks |
| `--code` | `list.even.background`, `muted.background` | Code, command output |
| `--secondary`, `--secondary-hover` | `secondary.background`, `secondary.hover.background` | Standard buttons |
| `--text` | `foreground` | Body text |
| `--muted` | `muted.foreground` | Metadata, tool lines, placeholders |
| `--line` | `border` | Hairlines between and around things |
| `--input-line` | `input.border`, `border` | A field's border |
| `--accent`, `--on-accent` | `primary.background`, `primary.foreground` | Primary buttons |
| `--link` | `link`, `base.blue` | Links, and accent text |
| `--tint` | `list.active.background` | Selection, the user's messages |
| `--ring` | `ring`, `base.blue` | Focus |
| `--warn` | `warning.background`, `base.yellow` | Text that needs attention |
| `--warn-mark` | `base.yellow` | A question's bar and marks (not text) |
| `--error` | `base.red`, `danger.background` | Failures, destructive actions |
| `--success` | `success.background`, `base.green` | A call that succeeded, used sparingly |
| `--switch-off`, `--switch-thumb` | `switch.background`, `switch.thumb.background` | A switch that is off, and its knob |
| `--thumb`, `--thumb-hover` | `scrollbar.thumb.background`, `…hover.background` | Scrollbars |
| `--popover`, `--popover-text` | `popover.background`, `popover.foreground` | Menus, popovers, sheets |
| `--outline` | `input.border` where it isn't `border`, else `muted.foreground` mixed half into `border` | A chosen segment's outline, a medium grey |
| `--muted-bg` | `muted.background`, `secondary.background` | A chosen segment's fill |
| `--radius`, `--radius-lg` | `radius` (6), `radius.lg` (8) | See [Shape](#shape) |
| `--syn-…` | `highlight.syntax.…`, else its first word's (`string.escape` takes `string`) | Code's highlighting |

A theme that sets none of a variable's tokens takes the default theme's of the same mode, except a
syntax style, which shows in the code's own color. A gradient gives its first color. The defaults
are macOS Classic Light and Dark, which follow the system's setting. The web page keeps a chosen
theme in the browser's localStorage and loads the other themes' variables only once one is chosen.

Every color on the page is one of these tokens. Something brought over from another interface or
an earlier page keeps its shape but not its colors. It takes the token that means the same thing
there, so it follows the chosen theme. The one fixed color is the white behind a QR code, which
reads only dark on light.

Status colors only carry their meaning, and never alone. A failed call has its red mark and the
word ("exit 1").

## Scale

Spacing goes 2, 4, 8, 12, 16, 24 and 32 px (GPUI Kit's `xxs` to `xxl`). Parts of one control are
4 apart, related controls 8, groups in a section 16, sections 24. A component owns its padding,
and its parent owns the gaps between components.

Type sizes are 12, 13, 14, 16, 18 and 20 px, with 16 px body text. The web page's fields are
16 px so iOS doesn't zoom on focus.

| Use | Face |
|---|---|
| Interface and prose | The system's UI font (Studio offers Work Sans) |
| Code, tool lines, worker IDs, paths, shortcuts | JetBrains Mono |
| Figures that line up (cost, time, tokens) | Tabular figures |

Headings in a reply stay small. Markdown's levels 1 and 2 show at body size in bold.

## Shape

| Tier | Value | Used for |
|---|---|---|
| `sm` | radius ÷ 2 (3 px) | Inline code, small tags |
| `md` | radius (6 px) | Buttons, menu rows, user messages, code blocks |
| `lg` | radius.lg (8 px) | Cards, popovers, menus |
| `xl` | radius × 2 (12 px) | Sheets, docked blocks |
| `full` | a pill or circle | The message field (40 px tall, 20 px radius), round icon buttons |

A theme with radius 0 (Windows) squares every tier, `full` included, but for the web page's message
field and its send button, which stay round so they match. Nested shapes are concentric, so an
inner radius is the outer one minus the gap. The terminal keeps square corners.

Borders are 1 px hairlines in `--line`. A region's boundary belongs to one side only.

Focus rings are 2 px of `--ring` with a 2 px offset on buttons and controls. A text field shows
focus by its 1 px border turning `--ring`, with no ring outside it.

## Components

Each web component matches a GPUI Kit component and its variants.

| Web | GPUI Kit | Notes |
|---|---|---|
| `.btn` and variants | `Button` | See the table below |
| `.chip` | `Button` small ghost with a label, or `Tag` | Short state or choice in a bar |
| `.field` | `Input`, `InputGroup` | Pill on the web page, `md` in Studio |
| `Select` | `Select` | A button whose list floats over the page, anchored to it, below or above it where there's more room, drawn as the command menu with its shadow, and a filter for a long list. Arrows, Home, End, Enter, Esc and typing a name work |
| `.views` | `ButtonGroup` of ghost buttons | The chosen view outlined in `--outline` on `--muted-bg`, the others plain. A menu on phones |
| `.menu` | `PopupMenu` | Rows 32 px, a check slot on the leading side |
| `.popover` | `Popover` | `lg` radius, the popover surface, a shadow |
| `.answers` | Studio's answer panel | Above the message box, non-modal, at most 60 % of the screen with its questions scrolling, numbered in their ask, never by id |
| `.sheet` | `Sheet` | From the bottom on phones, centered on wide screens, with a hairline in `--line` as the popovers have |
| `.card` | `GroupBox` | Surface and hairline, no shadow |
| `title` and `aria-label` | `Tooltip` | Required on icon-only buttons |
| `kbd` | `Kbd` | One key cap per chord, mono |

### Buttons

| Variant | When | Examples |
|---|---|---|
| Standard | An ordinary visible action | Retry, Copy link, Show all lines |
| Outline | An action that needs a boundary but less weight, beside a standard one | Move to background on a call |
| Primary | The one default action of a view, the one Enter takes | Send, Answer, Send answers |
| Ghost | Secondary actions in dense rows and bars | Hide, a status bar figure, a view switch |
| Icon | A well-known symbol with a frame | Send's arrow in its primary circle |
| Ghost icon | Toolbar and row actions | Settings, Expand, Copy, Close |
| Danger | A destructive commitment | Stop all workers, New key |

A view has at most one primary button. A destructive action names what it ends and asks first
when it can't be undone. Links are only for web addresses and email. Buttons that open a sheet or
dialog end with "…".

Sizes are 24, 32 and 40 px. On touch screens a button's target is at least 40 px even when it
draws smaller.

## Icons

Lucide only (`IconName` in GPUI Kit, `lucide-leptos` on the web page). One icon per concept, used
the same way everywhere. An icon without a label always has an accessible name, and a tooltip
when its meaning, scope or shortcut needs one.

| Concept | Icon |
|---|---|
| Settings | `Settings` |
| Workers, a worker | `Bot` |
| Stop (a turn, a job, a worker) | `Square` |
| Move to background | `ArrowDownToLine` |
| Copy, then copied | `Copy`, then `Check` |
| Send, Answer | `ArrowUp` |
| Expand and collapse the message field | `Maximize2`, `Minimize2` |
| A question | `MessageCircleQuestion` |
| Latest | `ArrowDown` |
| Close | `X` |
| Selected | `Check` |
| Disclosure | `ChevronRight`, `ChevronDown` |
| Warning, outside access | `TriangleAlert` |
| Failed | `CircleX` |
| Information | `Info` |
| Running (only where no shimmer shows) | `LoaderCircle` |
| Cache timer | `Timer` |
| The web page and its links | `Globe` |
| Theme | `Palette` |
| Attach | `Paperclip` |
| Shell, read, edit or write, list, search | `SquareTerminal`, `FileText`, `FilePen`, `FolderTree`, `Search` |
| Monitor, web, MCP, skill, another tool | `Activity`, `Globe`, `Plug`, `BookOpen`, `Wrench` |

Icons are 16 px in rows and bars and 18 px in the header, drawn with Lucide's 2 px stroke.

The web page's calls, thinking and call counts use text marks rather than icons. ▸ starts each
one and turns down as it opens. After a call come ✓ in `--success` once it succeeded, ✗ in
`--error` once it failed, or the running spinner. A command's output that warns has a rule in
`--warn-mark` beside it.

## States

| State | Treatment |
|---|---|
| Hover | The ghost background (`--secondary-hover`). Never the only cue, and only on devices that hover |
| Pressed | The same background, darker, at once |
| Open | A trigger stays pressed while its menu or popover is open |
| Focus | The focus ring above |
| Selected | `--tint` fill on the item itself, never a side bar |
| Disabled | `--secondary` fill and `--muted` text, no hover |
| Loading | The control keeps its place and label, and refuses a second press |
| Pending | A user's note the agent hasn't read is muted |
| Error | A line near the action that says what happened and what to do |

## Motion

Motion is for a change of state. It is quiet, short and never decorative, and it stops entirely
with reduced motion (the system setting, or Studio's Settings › Appearance › Motion). Nothing needs
motion to be understood.

| State | Motion |
|---|---|
| Working, thinking | A shimmer across the label: one sweep every 2 s, the band's half-width 30 % of the label, its color the text's mixed 20 % toward the background (light themes) or the text color (dark ones), at most 12 frames a second in Studio. The terminal sweeps its highlight the same way |
| A call running | A spinner on its row, at most 30 frames a second |
| Streaming text | Appears as it comes, with no fade |
| A new row arriving | Appears in place. The view follows only if the reader is at the bottom |
| Latest | Jumps to the bottom at once |
| Mouse wheel (Windows, Linux, Android) | A critically damped spring, about 0.2 s. Apple systems keep their own momentum |
| Moving to the background, done, failed | The row changes in place, with no motion |
| Menus, popovers, the Latest button | A fade of 150 ms |
| Sheets and panels | Appear without sliding |

Not allowed are staggered entrances, pulses, bouncing, sliding panels and animated layout.

## Layout

| Width | Studio | Web page |
|---|---|---|
| Under 700 px (phones) | Sidebar and panels as sheets | Full width, 16 px margins (wider than a notch in landscape) |
| 700 to 1200 px | Sidebar docked, panels as sheets | Full width up to 820 px |
| From 960 px (web) or 1200 px (Studio) | Everything docked, column up to 46 rem | Column `min(max(820px, 76vw), 1100px)` |

Phones in landscape (height up to 500 px) get a 40 px header and a message field of three lines.
The web page uses the dynamic viewport height, and its field grows to six lines before it scrolls.

- What's primary is reachable without hunting. The message field, Send, Stop and the waiting
  question are always in view.
- Dense lists stay calm. Rows keep stable columns, numbers align right in tabular figures, and
  only unusual states take color.
- Secondary actions on a row appear on hover or focus, in space the row already has, so nothing
  shifts. On touch screens they stay visible, or move into the row's menu.
- Only the region that overflows scrolls. A scrollbar sits at that region's edge. On Windows,
  Linux and Android it is thin and rounded and appears only when needed. Apple systems keep theirs.
- The web page doesn't refresh when pulled down and doesn't zoom on a double tap. Pinch zoom stays.

## Text

Everything a user reads takes the fewest words that keep the meaning.

- Labels are a word or two and don't repeat their surroundings.
- A state is named only when it's unusual, in one word where one fits ("Pending", "Failed").
- Sentences are plain, with no colons or semicolons joining clauses, no "Note:" prefixes and no
  bold lead-ins.
- Errors say what happened and what to do ("Couldn’t send. Arko isn’t reachable.").
- Sentence case, the "…" character, and no final period on labels.
- The same thing has the same name in every interface, in the words Arko's commands use.
