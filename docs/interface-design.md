# Interface design

Arko has three interfaces, Arko Studio (GPUI and GPUI Kit), the web page (Rust and Leptos, in
`web/`) and the terminal view (termelon). This guide keeps them one product. It follows GPUI Kit's
[design guide](https://github.com/mesa-hills-research/gpui_kit/blob/main/website/docs/design-guides.md),
and where the two differ for Arko, this guide wins. When something isn't covered, do what Studio
does. Every choice here has a reason, and a change to a rule comes with a rendering of what it
changes.

## Principles

1. The transcript is the product. Chrome stays small and carries only what the user acts on.
2. Show what is unusual, not what is normal. A failed call, a waiting question, outside access or a
   cache about to lapse shows. "Connected" and "Live" don't.
3. Nothing moves under the reader. Streaming never shifts what is being read, the view follows the
   bottom only while the reader is there, and a finished row never changes height.
4. Disclose progressively. Details are one click or tap away, never a toolbar of settings, and
   never more than two levels deep.
5. Everything works from the keyboard, focus always shows, and Esc closes the innermost layer.
6. Theme tokens only. No raw colors, radii or spacing in feature code.
7. Motion carries state and nothing else (see [Motion](#motion)).
8. Few words (see [Text](#text)).
9. The same thing has the same words and the same shape on every surface. When two surfaces
   differ, one of them is wrong.

## Transcript rows

The transcript has three views. Full shows every call as its own row. Quiet merges neighbouring
calls into a row that says what they did ("Read 4 files in src/tui", "Ran 2 commands (rg, sed)").
Messages shows the text and one line per turn ("▸ 8 tool calls") that opens to the turn's work as
Quiet shows it. The three views share one model of a row.

What is said (the user's message, the agent's text) is bright, in the body size, and stands apart.
The agent's work (its calls, its thoughts, its workers' lines and Arko's notices) is a quiet list
of single lines under it.

- Everything shows where it happened, at every level, so "above" always means "before".
- Closed, every item of work is one line at any width. Its words are cut to fit with "…", and its
  outcome stays whole ("(24 passed)", "· running", ✗). A cut line opens to the rest. In Quiet a row
  of one call opens to the call as written, unless its detail already says it whole.
- A row's line is its words and its outcome, nothing else. In Quiet the words are a sentence in
  the past tense ("Read engine.rs", "Messaged r3"), or the present while the call runs ("Running
  cargo build · 41s"). In Full they are the call as written ("read src/engine.rs:100-112"). The line
  carries no quote or preview of what the call held.
- The outcome is news only. A test run's count ("(24 passed)"), "no results", a job's or a
  monitor's state ("· running", "· finished"), a spinner and a timer while a call runs. In Quiet a
  success shows nothing, since the past tense says the call ended, and a failure shows one ✗ at the
  right of the row. In Full the line ends with ✓ or ✗ and the outcome's words ("✓ 13 lines",
  "✗ exit 101"). A job's row says its state, never its ID.
- A row holds calls of one kind that follow one another, or the reads and changes of one file, and
  its title names every kind it holds ("Read transcript.rs and made 3 edits"). A failure, a test run,
  a call of another kind and anything said end a row, and so does a row of one call the reader
  opened.
- Opening a row shows the call's content, which is its result in the form that fits the tool. It
  sits directly under the row, with no call line between them.

| Call | Content when opened |
|---|---|
| Read | The lines read, numbered as the tool numbered them, the numbers muted |
| Edit | The diff, with its context lines, removed lines on the error tint and added ones on the success tint |
| Write | The file's first lines |
| Command, test run | Its output. In Full the output shows under the line without opening |
| Search | The hits |
| List | The entries |
| Message to a worker | The message |
| Wait | The mail that came |
| Worker start | The task, muted (a worker started at once has the worker's line instead) |
| Note | The note's text |
| MCP call | The arguments, then the result |
| Monitor, terminal | One muted line with what the words left out (the whole command, its settings) |
| Skill | The skill's text |
| Any other tool | Its result, muted |

- Detail is the exception, and it is one muted line. Where the words leave out something the
  reader may need (a command's variables, its folder or its other lines, a long search), that whole
  thing shows as one muted line above the content. Never a list of labelled parameters, and never
  the path or target the words already said.
- A merged row opens to one Full line per call, muted, with its outcome, and each of those opens to
  its content. Two levels at most, the row, then what it holds on its rail, then a call's content.
- Status and controls live on the row's line only. Stop, Move to background, the timer and the
  spinner appear once per call, whatever is open.
- A row has a triangle exactly while opening shows something. One with no content and no detail
  has none (in the terminal, a dot in its place), also while its call runs, and gains ▸ in the same
  column when its result comes. Long content shows its first 16 lines and one line that says how
  much more there is ("… 97 more lines"), which opens the rest. That line is muted text on every
  surface. A line too long to show whole ends the preview where it's cut, so nothing hidden sits
  above text that shows, and the count is of the characters after the cut ("… 1,240 more
  characters").
- A row never moves. It keeps its place, its calls and its height once shown, and opening it adds
  below it. A row's words change in place while calls join it, and only the last row of a run takes
  new calls.

### Thoughts

- A thought is the agent's words to itself, muted and italic in the text column, with no label,
  marker or timer. Closed it is its first line, ending with "…" where it leaves something out, and a
  click, a tap or Enter shows the whole. One that fits on its line has nothing to open. It looks the
  same in Quiet, in Full, inside a row and in a worker's view.
- A thought between two calls of one Quiet row sits inside that row, between them, so a closed row
  hides only what happened between its first call and its last. Every other thought is a line of its
  own. The last one stands at the end until the next call comes, and goes in the row above if that
  call joins it. One the reader opened stays where it is, and the next call starts a row of its own.

### Markers, the live line and gaps

- There is one marker column and one text column. ▸ opens, ▾ is open, · is a call with nothing more
  to show (the terminal only), the spinner marks the one line that is live, ▎ is the user's message
  and ✗ a failure at the right of the line. The agent's text and its thoughts have no marker.
- In the terminal markers stand at the left edge and words two columns in. An open row's rail hangs
  from its marker, a merged row's inner markers stand two columns in, and their words, content and
  thoughts four in. On the page the marker column is 10 px wide, words start 16 px in, the rail is
  2 px wide under the marker's centre, and inner words and content start 32 px in.
- Exactly one line is live. While the agent waits on its model it is "Working…". While it thinks it
  is the thought itself, whose words grow in place. While a call runs, its row says so with its
  timer, and there is no Working line beside it. The spinner turns in the marker column.
- Lines of work stack. A gap stands between what is said and the work, and on both sides of
  anything open, also on a rail, and gaps never add up. The terminal's gap is one blank line. On the
  page work stacks 4 px apart, what is said and anything open stand 12 px apart, the user's message
  24 px below what's above it, and an open thought's paragraphs 8 px apart. A rail never starts or
  ends with a gap. The line the reader opened stays where it is on screen, so the gap above it comes
  from what is above moving up.
- Working… has as much space below it as above. In the terminal it has no blank row among the
  agent's calls, and one on each side after what was said, after anything open, or above a message
  waiting to be read. Such a message has a blank row above it and one below it, before the status
  line. The page keeps the line's room at the end while a call or a thinking shows that the agent
  works.
- In the terminal only the reader's own scroll moves the view. While it follows the end, nothing
  above moves down. When the end gets shorter (Working… giving way to a shorter row, a closing mark
  re-wrapping a line, a fold closing), up to three blank rows stand below it until more comes, and
  past three the end comes down to the last row. Scrolled up, the row at the top keeps its text
  through streaming, folds elsewhere and another width, and the view follows again once the reader
  scrolls down to the end or goes there with End.
- A tag never sits between a message's rows. A message that fits on one row has its tag ("pending",
  "queued", "from the web page") at the right of that row. One that wraps has the tag on a row of
  its own under the last.
- On the page and in Studio, the line after a turn (its time and cost) is left-aligned with the text.
  The terminal shows a turn's time and cost in its status bar instead.
- Content ends where it ends. A final table or code block has no gap below it, and a reply's actions
  sit at the same distance from any last block.

### Type

| Item | Page | Terminal |
|---|---|---|
| The user's message, the agent's text | UI font, 16 px on 24 | Plain |
| A thought | UI font, 14 px on 20, italic, muted | Dim italic |
| Working… | UI font, 14 px on 20, muted, with the shimmer | Dim |
| A row's words | Mono, 13 px on 20, the text colour | Plain |
| A row's outcome and timer, an inner call's line | Mono, 13 px on 20, muted (✓ and ✗ in the success and error colours) | Dim |
| Detail, a door | Mono, 12 px on 18, muted | Dim |
| A file's lines, a diff, output, a message | Mono, 12 px on 18, the text colour, line numbers muted | Plain, line numbers dim |
| Other content | Mono, 12 px on 18, muted | Dim |
| A worker's ID | UI font, 12 px on 16, weight 600 | Bold |
| Markers | Muted | Dim |

## Theme tokens

Colors come from GPUI Kit's theme files in `themes/`, the same files and names in every interface.
Studio reads them through `cx.theme()`, the terminal through `tui::theme::Roles`, and the web page
as CSS variables that `web/theme-css` writes. Each variable takes the first token a theme sets.

| CSS variable | Theme tokens, in order | Used for |
|---|---|---|
| `--bg` | `background` | The window or page |
| `--bar` | `title_bar.background`, `background` | Header and status bars |
| `--bar-line` | `title_bar.border`, `border` | The bars' hairline |
| `--surface` | `highlight.editor.background`, else the input background as GPUI Kit's `editor_background` gives it (`background` in light themes, `input.border` at 30 % over it in dark ones) | Fields, cards, docked blocks, code blocks |
| `--code` | `list.even.background`, `muted.background` | Inline code, command output, table heads |
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
| `--success` | `success.background`, `base.green` | A call that succeeded in Full, an added line in a diff, used sparingly |
| `--switch-off`, `--switch-thumb` | `switch.background`, `switch.thumb.background` | A switch that is off, and its knob |
| `--thumb`, `--thumb-hover` | `scrollbar.thumb.background`, `…hover.background` | Scrollbars |
| `--popover`, `--popover-text` | `popover.background`, `popover.foreground` | Menus, popovers, sheets |
| `--outline` | `input.border` where it isn't `border`, else `muted.foreground` mixed half into `border` | A chosen segment's outline, a medium grey |
| `--muted-bg` | `muted.background`, `secondary.background` | A chosen segment's fill |
| `--radius`, `--radius-lg` | `radius` (6), `radius.lg` (8) | See [Shape](#shape) |
| `--syn-…` | `highlight.syntax.…`, else its first word's (`string.escape` takes `string`) | Code's highlighting |

A theme that sets none of a variable's tokens takes the default theme's of the same mode, except a
syntax style, which shows in the code's own color. A gradient gives its first color. The defaults
are macOS Classic Light and Dark, which follow the system's setting. A theme being chosen previews
after a 350 ms rest, with a one-time notice that previews can flash. The web page keeps a chosen
theme in the browser's localStorage and loads the other themes' variables only once one is chosen.

The terminal colors code with the same `highlight.syntax` styles, chosen the same way, in 24-bit
color or the nearest of xterm's 256, as it paints its other colors. A diff's removed and added lines
take the error and success colors faintly over the background, and the code's colors are shaded
until they read there. The `terminal` theme, which keeps the terminal's own colors, shows code in
the text's color, and so does a terminal with 16 colors or `NO_COLOR` set.

Every color on the page is one of these tokens. Something brought over from another interface or
an earlier page keeps its shape but not its colors. It takes the token that means the same thing
there, so it follows the chosen theme. The one fixed color is the white behind a QR code, which
reads only dark on light.

Status colors only carry their meaning, and never alone. A failed call has its red mark and the
word ("exit 1"). What needs attention takes the warning color, such as an "API Error" line or
"every project" in an approval. Contrast follows GPUI Kit's levels (text 4.5, a control's state
3.0, input borders 1.4, selection 1.2, dividers 1.12, hover 1.05) in all of the kit's themes, which
`script/themes check` in the kit measures. Find matches reach 4.5 on plain text, and code blocks
sit on the editor surface with a hairline.

## Scale

Spacing goes 2, 4, 8, 12, 16, 24 and 32 px (GPUI Kit's `xxs` to `xxl`). Parts of one control are
4 apart, related controls 8, groups in a section 16, sections 24. A component owns its padding,
and its parent owns the gaps between components. Gaps meant to be equal are equal to the rendered
pixel. One pixel off is a defect.

Type sizes are 12, 13, 14, 16, 18 and 20 px, with 16 px body text. The web page's fields are
16 px so iOS doesn't zoom on focus. Users choose the family, the size and the zoom.

| Use | Face |
|---|---|
| Interface and prose | Work Sans in Studio, bundled as a variable font. The system's UI font on the web page |
| Code, tool lines, worker IDs, paths, shortcuts | JetBrains Mono, bundled in Studio and on the web page |
| Figures that line up (cost, time, tokens, counts) | Tabular figures, aligned right |

Headings in a reply stay small. Markdown's levels 1 and 2 show at body size in bold. Times are
12-hour in the local zone with its name ("Resets at 8:00 PM MDT"), with summer time named.

## Shape

| Tier | Value | Used for |
|---|---|---|
| `sm` | radius ÷ 2 (3 px) | Inline code, small tags |
| `md` | radius (6 px) | Buttons, menu rows, user messages, code blocks |
| `lg` | radius.lg (8 px) | Cards, popovers, menus |
| `xl` | radius × 2 (12 px) | Sheets, docked blocks |
| `full` | a pill or circle | The message field (40 px tall, 20 px radius), round icon buttons |

A theme with radius 0 (Windows) squares every tier, `full` included, but for the web page's message
field and its send button, which stay round so they match. The terminal draws rounded corners for a
theme with a radius and square ones for radius 0, in the symbols the terminal shows.

Nested shapes are concentric. The inner radius is the outer one minus the inset, with a floor for
rows and tiles, `max(outer − inset, min(3 px, outer ÷ 2))`, so menu rows, select rows and image
tiles come out at 3 px like the command menu's rows. GPUI Kit has `concentric_radius`,
`enclosing_radius` and `rounded_inside`, and on the page a rounded parent sets `--outer` (its
radius) and `--inset` (its padding plus border), and its children take `--r-inner`. GPUI doesn't
clip children to a rounded parent, so every fill inside a rounded border needs its own radius.

No cards inside cards, and shadows only on what sits above other content. The composer's subtle
shadow sets it apart from the transcript, and on the web page a line along its top does. Selection
is a fill on the item itself, never a bar at its side. A segmented selector is a window-colored
track with a border and the chosen option in grey with a soft outline. The web page's view switch
has no track, plain options and the chosen one outlined in `--outline` on `--muted-bg`.

Borders are 1 px hairlines in `--line`. A region's boundary belongs to one side only.

Focus rings are 2 px of `--ring` with a 2 px offset on buttons and controls. A text field shows
focus by its 1 px border turning `--ring`, with no ring outside it. Clicking elsewhere in a card or
popover takes focus away from its field.

## Keycaps

A shortcut shows as one key cap for the whole chord ("Ctrl+C" together, not a box per key), in
the mono font, as compact as stays readable, and often in a tooltip rather than in the main view.
Studio uses GPUI Kit's `Kbd` in tooltips and menus, and a button that carries its shortcut shows it
as a cap inside the button, as Reject and Approve do on the approval tray. The web page's `kbd` is
one cap per chord with a 2 px bottom border, shown only on devices with a fine pointer that
hovers. The terminal draws " Ctrl+O " as one cap, reversed in a terminal-colors theme or on the
surface color otherwise, and shows a few key hints in the status line ("Ctrl+C  stop").

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
| `.record-box` | The terminal's settled ask | A question card once any of its questions is settled, boxed as the panel is but in the hairline, its text as a muted heading, then each settled question with its outcome |
| `.sheet` | `Sheet` | From the bottom on phones, centered on wide screens, with a hairline in `--line` as the popovers have |
| `.card` | `GroupBox` | Surface and hairline, no shadow |
| `.copy` | `Clipboard` | A ghost icon button. Under a message or reply it shows on hover or focus, and always on touch screens. A code block has one in its corner, after its language |
| `.jump` | Studio's jump-to-latest button | Says "N new" for what came while the reader was up the page, counting each reply, text, thinking, message, question, user message, call and turn-starting notice once, with a call's results in the call. A view counts what it shows, so Messages counts a turn's calls as one. With none new it says "New text" if text grew, else "Latest" |
| `.thumb` | Studio's image tile | 160 by 120 px, the image contained in it, loaded as it nears the view |
| `.md .table` | Markdown table | Keeps the width its cells ask for and scrolls sideways inside its row, as in Studio. A cell wraps past 40 characters. A table that fits has no scrollbar |
| `dialog.viewer` | `ImageViewer` | One image as large as the screen allows, with its name and size |
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
when it can't be undone ("Delete…" names the session). Links are only for web addresses and
email. Buttons that open a sheet or dialog end with "…". Buttons keep the arrow cursor, and the
pointing hand is only for links.

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

The web page's and the terminal's calls, thinking and call counts use text marks rather than icons.
▸ starts each one and turns down as it opens. In Full a call's line ends with ✓ in `--success`
once it succeeded or ✗ in `--error` once it failed, and in Quiet only ✗ shows. A running call has
the spinner, Lucide's loader circle as two arcs half a turn apart, so it turns without wobbling. A
command's output that warns has a rule in `--warn-mark` beside it.

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

A row's secondary actions appear on hover or focus in space the row already has, so nothing
shifts, and stay visible on touch screens. Tooltips first show after 500 ms. While one is open a
neighbour's shows after 100 ms of rest, the open state ends 400 ms after the pointer leaves, and
the box moves and resizes over 80 ms with the text never moving or clipped. No tooltip shows over
an open menu or popover, its own trigger's included.

## Motion

Motion is for a change of state. It is quiet, short and never decorative, and it stops entirely
with reduced motion (the system setting, or Studio's Settings › Appearance › Motion, with System as
the default). The terminal has no switch for it. Nothing needs motion to be understood.

| State | Motion |
|---|---|
| Working, thinking | A shimmer across the label, one sweep every 2 s. On the web page and in the terminal a band about 60 % of the label wide, in the text color, sweeps over the muted label. In Studio the band's half-width is 30 % of the label and its color the text's mixed 20 % toward the background (light themes) or the text color (dark ones), at most 12 frames a second |
| A call running | A spinner on its row. Studio's is GPUI Kit's eased spokes at 1.2 s a turn, the web page's two arcs at 1.2 s a turn, the terminal's the balanced braille frames ⣷⣯⣟⡿⢿⣻⣽⣾ at 100 ms each |
| Streaming text | Appears as it comes, with no fade. Smooth typing in text fields is on by default |
| A new row arriving | Appears in place. The view follows only if the reader is at the bottom |
| Latest | Jumps to the bottom at once |
| Mouse wheel (Windows, Linux, Android) | A critically damped spring, about 0.2 s. Apple systems keep their own momentum |
| Moving to the background, done, failed | The row changes in place, with no motion |
| Menus, popovers, the Latest button | A fade of 150 ms |
| Send and Stop trading places on the web page | A cross-fade of 150 ms |
| Copied | The copy icon cross-fades to a check in `--success` (120 ms), which holds for a second, then fades back |
| The context ring | Its value changes smoothly |
| Sheets and panels | Appear without sliding, since a glide would lay out the transcript again every frame |

Not allowed are staggered entrances, pulses, bouncing, sliding panels and animated layout.
Something that moves never redraws the content around it. On the web page only `transform` and
`opacity` animate, and in GPUI an animation is a paint effect that replays the last frame.

## Layout

| Width | Studio | Web page |
|---|---|---|
| Under 700 px (phones) | Sidebar and panels as sheets | Full width, 16 px margins (wider than a notch in landscape) |
| 700 to 1200 px | Sidebar docked, panels as sheets | Full width up to 820 px |
| From 960 px (web) or 1200 px (Studio) | Everything docked, column up to 46 rem | Column `min(max(820px, 76vw), 1100px)` |

Phones in landscape (height up to 500 px) get a 40 px header and a message field of three lines.
The web page uses the dynamic viewport height, and its field grows to six lines before it scrolls.

- What's primary is reachable without hunting. The message field, Send, Stop and the waiting
  question are always in view. On the web page Stop takes Send's place while a turn runs and the
  message box is empty, so the field never moves.
- Dense lists stay calm. Rows keep stable columns, numbers align right in tabular figures, and
  only unusual states take color.
- Only the region that overflows scrolls. A scrollbar sits at that region's edge. On Windows,
  Linux and Android it is thin and rounded and appears only when needed. Apple systems keep theirs.
- The web page doesn't refresh when pulled down and doesn't zoom on a double tap. Pinch zoom stays.

## Notices and status

- No "Live" or "Connected" badges and no decorative dots. A dot that means something stays, such as
  the sidebar's dot in the info color for a session with news.
- A state shows only when it's unusual. The status bar has Working and its time only while a turn
  runs, a worker count only above 0, plan usage only from 80 %, and outside access or a shared web
  page only while on.
- One loading indicator per turn, the status bar's Working plus the row of whatever runs. A screen
  reader hears "Working" once per turn.
- A state takes one word where one fits ("Pending" for a note the agent hasn't read).
- A setting that took shows nothing, and one that failed says why.
- No row explains the obvious, such as a hint that answers go to the agent.
- "Stopping the turn…" becomes "Stopped the turn." in place, and turn notices say who and what
  ("r1 reported").
- A mistyped command stays in the message box and says it's unknown.
- Notifications show the current session by default, with a Session | All selector.
- Cache reminders come 5 minutes before expiry and at 1 minute with a countdown and Refresh, and
  they give both costs.

## Questions, approvals and confirmations

- A question shows in one place at a time. While it's open it's in the answer panel or a chip above
  the message box, and its card in the transcript is one quiet line. Once one settles, the card
  becomes a boxed record with each question's outcome.
- The answer panel opens by itself only while the message box is empty and no key was pressed for
  about 2 s. Otherwise a chip waits and leaves the keyboard alone.
- Questions are numbered from 1 within each ask, never by id. "Choose any" sits muted right after
  the question's words. Each has its choices, "You decide" with the agent's default, and an Other
  field.
- Esc or Answer in chat attaches the open questions to the message box ("Replying to 2 questions
  ×"). The user's answers show as a chat bubble under the question card, so they read as the user's
  own words.
- Approvals have their own tray above the message field, one row per request that opens in place. A
  batch has a checkbox per row, all ticked, and one button that counts ("Approve all", "Approve 2,
  reject 1"), plus Reject all. Text in the message field goes with the answer when Include message
  is on. Studio binds ⇧⌘A and ⇧⌘R.
- A confirmation names what it ends and what it costs. Switching between Chat and Agent says what
  changes and what the lost cache costs ("about 180k tokens, ≈ $0.90"), and continuing a worker
  whose cache expired asks first and gives the cost.
- "Don't ask again" is remembered only when the user picks the action that goes ahead.

## Keyboard

- Every action has a keyboard path. Esc backs out of the innermost layer and returns focus to what
  opened it.
- Enter sends by default, with Mod+Enter and Shift+Enter as options, and the chosen chord never also
  adds a new line. On the web page Enter sends only on devices with a fine pointer that hovers, and
  on touch screens it makes a new line.
- ↑ on the first line and ↓ on the last walk the sent messages. Alt+↑ on an empty box edits the last
  one.
- Alt+Q (Option+Q on a Mac) opens the answer panel from anywhere, Alt+A the approval tray, and
  Ctrl+B moves a running call to the background. In the terminal Ctrl+O switches the view and Ctrl+T
  reaches the agent list.
- Dictation is Ctrl+R in the terminal. In Studio and on the web page a click toggles it and a press
  held past about 300 ms is hold-to-talk.

## Text

Everything a user reads takes the fewest words that keep the meaning, as the built-in `prose` skill
asks.

- Labels are a word or two and don't repeat their surroundings.
- A state is named only when it's unusual, in one word where one fits ("Pending", "Failed").
- Sentences are plain, with no colons or semicolons joining clauses, no "Note:" prefixes and no
  bold lead-ins.
- Errors say what happened and what to do ("Couldn’t send. Arko isn’t reachable.").
- Sentence case, the "…" character, and no final period on labels.
- The same thing has the same name in every interface, in the words Arko's commands use.

## Where the surfaces differ

They share the theme files, the spacing and type scales, the icon set, the principles, the words
and the names of things. These differences come from the medium.

| | Studio | Web page | Terminal |
|---|---|---|---|
| Spinner | Eased spokes, 1.2 s a turn | Two arcs, 1.2 s a turn | Braille, 100 ms a frame |
| Shimmer | Band half-width 30 %, 12 fps | Band 60 % wide, to the text color | Band 60 % wide |
| Message field | `md` corners | A pill, round even at radius 0 | A line with a prompt |
| Shortcuts | In tooltips and menus | Hints on devices that hover | In the status line |
| A row's secondary actions | On hover or focus | On hover or focus, always on touch | None |
| Panels | Docked from 1200 px, sheets below | Sheets at every width | In place |
| Reduced motion | System or Settings | System | None |
| Row markers | Under the Working spinner | In the spinner's column, 10 px wide | At the left edge, two columns before the agent's text, as the spinner is |
