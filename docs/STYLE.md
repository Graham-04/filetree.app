# UI style

Keep the interface native, restrained, and proportional. Reuse the shared font sizes and radii instead of introducing a new value for each view. All dimensions below are logical points at 100% zoom unless stated otherwise.

## Sources of truth

| File | Responsibility |
|------|----------------|
| [src/ui.rs](../src/ui.rs) | Font-size tokens, radius tokens, `dp`, compact row dimensions, zoom limits, and the shared `Reveal` animation |
| [src/search_index.rs](../src/search_index.rs) | App-owned fsearch cache, scoped crawling, progress, fuzzy queries, and safe clearing |
| [src/search_input.rs](../src/search_input.rs) | Single-line text input for the command menu and titlebar search: native input, selection, and clipboard |
| [src/native_icons.rs](../src/native_icons.rs) | SF Symbols, system file icons, rasterization, and caching |
| [src/window_chrome.rs](../src/window_chrome.rs) | Traffic-light positioning and titlebar geometry |
| [src/sidebar.rs](../src/sidebar.rs) | Sidebar dimensions and collapse animation |
| [src/file_list.rs](../src/file_list.rs) | Metadata scan, sortable columns, per-tab selection, virtual rows, and drag edge scrolling |
| [src/main.rs](../src/main.rs) | Tab layout, file rows, and keyboard bindings |

Update this document when these shared rules change.

## Font and font-size palette

Use `.SystemUIFont`, GPUI's macOS system-font family, which resolves to San Francisco. Inherit it from the root view. Use normal character spacing; do not add wide tracking.

Keep the font-size palette limited to the shared roles:

| Token | Size | Current use |
|-------|------|-------------|
| `text_size::DETAIL` | 12 | Tab labels and secondary interface text |
| `text_size::BODY` | 14 | File names, sidebar rows, and ordinary interface text |
| `text_size::INPUT` | 15 | Command-menu search field |

Choose by role, not by eye. Do not use arbitrary literal font sizes or create a new token for one control. If a new role requires another size, define it centrally in `ui::text_size` and document its purpose here.

```rust
.font_family(".SystemUIFont")
.text_size(dp(text_size::BODY))
```

`BODY` sits one point above the macOS system body size so sidebar and file rows read comfortably at 100% zoom. The 16-point default rem is the zoom baseline, not another interface font size.

## Scaling

Use `dp(points)` for text, padding, gaps, dimensions, icons, and radii. It returns `rems(points / 16)`, so the whole interface follows the window's rem size.

```rust
div()
    .gap(dp(8.))
    .px(dp(20.))
    .rounded(dp(radius::SM))
```

| Shortcut | Behavior |
|----------|----------|
| ⌘+ or ⌘= | Increase the rem size by 1 |
| ⌘− | Decrease the rem size by 1 |
| ⌘0 | Reset the rem size to 16, or 100% |

The rem size is clamped to 8–40: 50%–250% zoom, in steps of 6.25 percentage points. Zoom is currently held in memory for the window.

For calculations outside layout, resolve `dp(value).to_pixels(window.rem_size())`. Compare tab widths against thresholds in the same coordinate system. Do not scale text while leaving its icon, padding, or hit area fixed.

Reserve `px(...)` for resolved geometry and deliberate native-window constraints. Native traffic-light buttons keep their system dimensions. The titlebar height is `max(40 × scale, 32)` logical pixels to leave room for them at small zoom levels.

## Icons

Use the native renderer in `native_icons`:

- Files and folders: `native_icons::file_icon(FileIcon, size)`, backed by `NSWorkspace.iconForFile:`. `FileIcon::classify` runs off the UI thread, reusing the `symlink_metadata` the caller already read, and keys the icon by what decides its look: regular files by lowercase extension (and the executable bit when there is none), plain folders as one kind. Symlinks, folders with an extension (packages such as `.app`), folders directly under `/`, `/Users`, `/Volumes`, or home, and folders containing a custom `Icon\r` file keep their own icon. Each kind is drawn once, from the first file seen, and shared by every row and tab. Files given a custom icon in Get Info show their type icon. File-list rows and ⌘K completions both use classified icons; a ⌘K row for the typed folder itself draws that folder's own icon. `FileIcon` holds its path as `Arc<Path>` so rows clone it cheaply on every redraw.
- Interface controls: `native_icons::symbol(name, size)`, backed by SF Symbols.
- Compact inactive tabs: an 8-point blue dot drawn with GPUI, using `.rounded_full()`.

| Element | Graphic size | Click area where applicable |
|---------|--------------|-----------------------------|
| File or folder row | 16 | — |
| Sidebar navigation symbol | 16 | Rows are currently inactive |
| Sidebar toggle (`square.split.2x1`, square-style frame) | 20 | 32 × 32 |
| Tab close symbol | 10 | 24 × 24 |

These values scale through `dp`. Keep the click area larger than its graphic.

Rasterize at `ceil(zoomed point size × display scale)`, rather than enlarging a default-resolution bitmap. Configure SF Symbols at the requested point size. Cache by icon kind, pixel dimensions, and point size; share cached images between tabs. GPUI keeps cached assets and their atlas tiles for the life of the app, so `native_icons` records every raster it hands over and `release_stale_rasters`, called at the start of each frame, releases them all from both the asset cache and the GPU atlas when the zoom or display scale changes or more than 1,024 are held; visible icons are redrawn on demand.

AppKit drawing requires a supported premultiplied RGBA bitmap. Convert the result to straight BGRA before passing it to GPUI. Do not restore the default-size TIFF conversion, which produced blurry icons, or use a non-premultiplied AppKit drawing context, which failed to render.

File and folder icons come only from macOS; the project bundles no icon assets.

## File-list density

Use the shared `ui::row` tokens for the main file list, sidebar navigation, and Recent rows:

| Token | Size | Purpose |
|-------|------|---------|
| `row::HEIGHT` | 28 | Fixed row height, with text and icon vertically centered |
| `row::ICON` | 16 | File and navigation graphics |
| `row::GAP` | 8 | Space between the graphic and filename |

Use `BODY` text for filenames and navigation. Do not add vertical padding on top of the fixed row height. Keep names on one line and truncate with an ellipsis when needed, so long filenames do not increase the row height. These values follow UI zoom. The titlebar toggle retains its larger icon and click area.

## Corner radii

Use the shared radius palette:

| Token | Radius | Current use |
|-------|--------|-------------|
| `radius::XS` | 4 | Storage bar segments in Settings |
| `radius::SM` | 6 | Sidebar toggle, close-button hover surface, and command rows |
| `radius::MD` | 8 | Tab faces |
| `radius::LG` | 12 | Command-menu surface |

```rust
.rounded(dp(radius::MD))
```

Do not introduce per-view radius literals. Reuse the appropriate role, and update the shared palette and this document together if a new role is needed. Use `.rounded_full()` for circles and pills. Leave the native window's outer corners to macOS.

## Titlebar, tabs, and sidebar

The titlebar overlays the content. In a regular window, the sidebar background extends behind the native controls, while sidebar rows begin below the titlebar. In fullscreen, sidebar rows start at the top with no reserved traffic-light strip, and the sidebar toggle moves to the start of the tab row. It follows the sidebar edge during collapse and expansion. macOS handles its own window-control reveal at the screen's top edge.

Traffic lights, the sidebar toggle, and tabs share a vertical centerline. Measure the native button group instead of reserving an arbitrary fixed-width gap. In a regular window, anchor the toggle to the sidebar’s right edge with a 4-point inset. As the sidebar collapses, let it move left until its hit area is 4 scaled points after the native button group; keep another 4-point gap after its hit area. The tab row adds no leading padding, so the gaps on either side of the toggle match when the sidebar is collapsed. In fullscreen, omit the traffic-light reservation and the native minimum titlebar height.

Tabs have individual rounded faces. When the sidebar is expanded, one continuous background matching the sidebar spans the toggle, its surrounding gaps, and the tab row. Paint this shared backdrop behind the controls rather than starting it after the toggle. That background fades with the sidebar animation and becomes transparent when the sidebar is collapsed. At 100% zoom, the sidebar is 220 points wide, navigation and file rows are 28 points high, sidebar sections are 16 points apart, and tab gaps are 4 points; these dimensions scale together.

Tab fitting uses these widths in unzoomed points:

| Tab width | Content |
|-----------|---------|
| 100 or more | Label and close button |
| 70–99 | Label; close button on the selected tab |
| Below 70 | Blue dot on inactive tabs; close button on the selected tab |

Tabs shrink to a 36-point minimum, then the row scrolls horizontally. Selecting a compact tab reveals its close button without closing the tab on that selection click.

The sidebar starts with Favorites: Home, Desktop, Documents, Downloads, Applications, Pictures, Music, and Movies. Favorites navigate the active tab and highlight its current location. The Recent and Locations sections follow. Each section heading toggles its rows, and its chevron reflects the expanded state. Locations lists the mounted volumes Finder shows, the startup disk first, each with its system icon and name (`system::volumes`; hidden system volumes such as Recovery are left out). External, removable, disk-image, and network volumes have an eject button that unmounts and ejects through NSWorkspace off the UI thread; tabs showing an ejected volume move to Desktop, and a volume still in use stays mounted with the alert sound. The list refreshes when /Volumes changes, through the same folder watcher as live listings, so mounts and ejects from anywhere appear without polling. Volumes are drop targets and highlight when current. Trash is pinned below the scrolling area, separated by a hairline, with a speaker button that toggles sounds (blue `speaker.wave.2` when on, a muted gray crossed-out `speaker.slash` when off) and a gear button at the bar's trailing edge that opens Settings: a row-height square with the small radius and a quiet hover, so Settings is discoverable without knowing ⌘, or ⌘K. Recent lists the last 10 files and folders the user explicitly opened (double-click, Enter, ⌘↓, ⌘K, or a Recent row), newest first, each with its system icon; clicking one opens it like a double-click without reordering the list, so nothing moves under the pointer, and an item that no longer exists is dropped when clicked or at launch. Opening an item anywhere else still moves it to the top. Sidebar, path-bar, and history navigation do not add items. Trash opens `~/.Trash` in the active tab; reading it needs Full Disk Access, and a denied read says so in the list. Dropping files on Trash moves them to the Trash, with the Trash sound and undo. While indexing is active, a status and thin blue progress bar sit immediately above Trash. They fade and expand in, then fade and collapse completely after completion. Trash stays pinned at the bottom; released space returns to the scrolling sidebar contents. Sidebar scrolling stays below the header safe area.

Animate opening and closing with `ui::Reveal`: 140ms ease-out-quint, reversing from the current animated position, requesting frames only while moving. The sidebar and titlebar search both use it. ⌘R and the sidebar button perform the same toggle.

## File columns

The file list shows **Name**, **Date Modified**, and **Size**. Columns are always visible for now. Name fills the remaining width, with Date Modified initially 188 points and Size 92 points. Column headings remain above the vertical scrolling list. If the window is too narrow, the table scrolls horizontally with its headings aligned to the rows.

Click a heading to sort; click it again to reverse. New date and size sorts start descending, while Name starts ascending. Sort dates by their timestamps and sizes by their byte counts. Keep missing values and folder sizes last in either direction. Preserve selected file identities when the row order changes. Each tab owns its order and column widths.

Drag the left boundary of Date Modified or Size to resize that column; the Name column absorbs the difference. Widths scale with zoom. Date Modified is constrained to 136–320 points, Size to 64–180 points, and the table leaves at least 160 points for Name.

**Live listings.** `watcher::FolderWatcher` keeps one FSEvents stream over exactly the folders non-search tabs show (deduplicated, by real path, so `/tmp` works) plus `/Volumes`, rebuilt only when that set changes. Only events for a watched folder itself count; changes deeper inside it are ignored, so watching Home ignores churn in ~/Library. Events collect in one set while the UI catches up, so a burst wakes it once. When another app changes a shown folder, the visible tab re-reads it in place (keeping scroll, selection, and sort), at most every 500ms per tab, with changes arriving sooner coalesced into one more re-read; background tabs are marked stale and re-read when shown. Content changes update sizes and dates too. If macOS drops events, every watched folder is re-read. A tab whose folder was moved or deleted goes to the nearest folder that still exists. Changes on network shares made by other computers are not reported by macOS and are not polled for; revisiting re-reads them.

Collect folder metadata on a background executor, sorted by name. `folder_cache::FolderCache` keeps the 64 most recently listed folders, up to 100,000 rows in total, shared by all tabs; larger listings are not cached. A cached folder shows immediately and is re-read in the background every visit, then applied in place, keeping scroll, selection, and sort, only if it changed. An uncached folder keeps the previous rows on screen, under the delayed loading bar, until its listing arrives, so switching never shows a blank frame. Fetch metadata for folder listings and search results off the UI thread, in parallel across cores, reading each path once with `symlink_metadata` and following only symlinks for their target's size and date. Entries store the name as a `SharedString`, the path, a lowercase sort key, the modification time, and the byte size; date and size labels are formatted at render time for visible rows only, so cached listings do not carry them. Format modification dates in local time, using Today or Yesterday for recent entries relative to the current day. Show decimal file sizes (1 KB = 1,000 bytes), right-aligned. Display **—** for folders or unavailable metadata; do not recursively total folder contents.

## File selection

Selection is presentation-only: it does not open, move, or modify files. Each tab owns its scroll position and selected rows. Click selects a row, ⌘-click toggles it, and Shift-click selects a range. Click blank space to clear, ⌘A to select all in the focused list, or Escape to clear. Up and Down move a single selection, starting at the first row (Down) or last row (Up) when nothing is selected; Shift extends from the anchor and shrinks back when reversed; Option-Up and Option-Down jump to the first and last rows. The moved row scrolls into view. Keyboard focus returns to the active file list after navigation, tab changes, closing search or the command menu, and at launch.

Dragging beyond a 4-point threshold shows a translucent blue marquee with a thin outline. Select intersecting rows in either drag direction; Shift adds to selection and ⌘ toggles against the selection at drag start. Keep the marquee clipped to the file viewport. Edge scrolling eases in over the last 28 points inside the viewport. At the boundary it reaches 500 logical points per second, then increases gradually with distance outside, capped at 800 after 160 points. Multiply speed by UI zoom and elapsed time so motion stays proportional and frame-rate independent. Respect scroll limits and stop on release. End a drag when switching tabs or opening the command menu.

Use GPUI `uniform_list` to construct and lay out only visible rows, plus its measurement row. Do not build one element per file on every redraw. Share folder listings between tabs through the folder cache, while keeping selection and scroll state per tab.

Store committed selection as sorted, disjoint index ranges. A contiguous million-file selection and Select All each need a single range. During marquee movement, retain the initial selection and one hit range; calculate membership only for rendered rows. Merge or toggle intervals on release instead of rebuilding a selected-file set every frame. Sparse selections cost space proportional to the number of disjoint ranges, not the total directory size.

Selected rows use a blue fill and white text. Adjacent selected rows form a continuous block: only the first row has rounded top corners and only the last row has rounded bottom corners, using the shared small radius. Determine neighbors by selection index, including rows outside the visible viewport. Unselected rows have a subtle hover surface. Preserve the compact row dimensions and align icons and text inside the selection surface.

## Opening and navigation

Single-click selects; double-click opens. Enter and ⌘↓ open the selection. Folders open in place in the current tab. Files and packages such as `.app` bundles, detected with `NSWorkspace.isFilePackageAtPath:`, open in their default application. When several items are opened, files open in their apps and the tab moves to the first folder. Opening a search result leaves the search.

Each tab keeps its own back and forward history in `history::History`, up to 5,000 locations in each direction; beyond that the oldest are dropped. History lives only in memory and is freed with its tab. Navigating to a new location, including from the sidebar or path bar, clears forward history; returning to the current location does not add an entry. ⌘[ goes back, ⌘] goes forward, and ⌘↑ opens the enclosing folder, always. ⌘← and ⌘→ follow the `arrow_keys` user setting, chosen in Settings (a two-option control: Back and forward / Folder path, with an `info.circle` beside the label whose tooltip explains both) or with two ⌘K commands whose checkbox marks the active one:

- **Back and forward**: the same as ⌘[ and ⌘].
- **Folder path** (default): ⌘← opens the parent; ⌘→ goes back down, one level at a time, into the folder the tab came up from. `history::History` remembers the deepest folder on the tab's current path (including the folder a tab started in); going anywhere off that path replaces it. The path bar shows the folders ⌘→ returns to after the current one, in dimmed text and clickable. With nowhere to go, ⌘→ plays the alert sound.

Renaming a folder updates every tab's history and remembered path. ⌘← and ⌘→ apply only while the file list has focus, so text fields keep them for moving to the start and end of the line.

Clicking the path bar outside its segments and buttons, or pressing ⇧⌘G (Finder's Go to Folder) or ⌘L, turns it into a text field holding the current path (`~` inside home), all selected so typing replaces it. The field has a blue outline on the list background. Once the user types, suggestions from `path_completion` (the same as ⌘K's: full-path labels, system icons, hidden items after a dot or with Show Hidden Files) show in a panel below it, drawn above the list. Up and Down choose, Tab fills in the highlighted (or first) suggestion and keeps editing, and Enter goes to the highlighted suggestion, else the typed path if it exists, else the first suggestion; folders open in the tab and files in their app, and nowhere to go plays the alert. Escape or moving focus away restores the segments. Segments keep navigating on click.

A 36-point path bar sits between the tab row and the file list on the same surface as the column header, with a hairline below it. It holds 28-point back and forward buttons, dimmed to 35% when there is no history, followed by the location as clickable segments separated by small chevrons. Paths inside the home folder start at the home folder; other paths start at **Macintosh HD**. The current folder uses the primary text color and medium weight and never shrinks; earlier segments are muted and truncate with an ellipsis when space runs out.

## File actions

Right-clicking a row selects it unless it is already part of the selection, then opens a menu at the pointer: Open (⌘↓), Open With ›, Quick Look (Space); Rename (R, single item only), Duplicate (⌘D), Copy (⌘C), Copy Path (⌥⌘C); Show in Finder; Move to Trash (⌘⌫); New Folder (⇧⌘N), and Paste (⌘V) when the clipboard holds files. Right-clicking blank space clears the selection and offers New Folder, Paste, Copy Path, and Show in Finder for the current folder.

Open With shows a submenu beside the menu (or to its left when there is no room) when its row is hovered or clicked, listing the apps macOS knows can open the first item, the default first and marked Default, each with its icon. Enter on Open With opens with the default app. Apps are found once per menu. The menu is 220 points wide with `row::HEIGHT` rows, the medium radius, a shadow and no border; shortcuts are muted `DETAIL` text. It stays inside the window, takes the keyboard while open (Up, Down, Enter, Escape), and closes on any click outside it. As in a native menu, each item's shortcut also works while the menu is open, running that item and closing the menu; shortcuts for items the menu does not offer do nothing. The shortcuts work without the menu while the file list has focus.

`file_list` owns selection and inline editing and reports what the user asked for as `file_list::Request`; the browser performs it through `file_ops`, which validates input and leaves the disk unchanged on failure. A failed action plays the system alert sound; there are no error notices.

- **Move to Trash** goes through `NSFileManager.trashItemAtURL`, so Finder's Put Back works. Rows disappear at once; the folder is re-read afterwards. On success it plays the Dock's “drag to trash” sound from macOS's system sounds, only when “Play user interface sound effects” is on in System Settings › Sound.
- **Rename** (R with exactly one item selected; otherwise the alert sound) edits the name inline in its row with a blue outline, selecting the name before the extension (the whole name for folders). Enter or clicking away commits; Escape cancels. Empty names, `/`, `.`, `..`, and names already taken are refused; a change of letter case alone is allowed. Tabs inside a renamed folder and recent items follow it.
- **New Folder** creates “untitled folder” (numbered when taken) in the current folder and starts renaming it. It is unavailable while showing search results.
- **Copy** (⌘C) puts the selection on the clipboard as files, in Finder's format, so it pastes into Finder too. **Paste** (⌘V) copies the clipboard's files, from this app or Finder, into the current folder; **⌥⌘V** moves them instead. Pasting into the folder they came from makes “name copy”. Both are unavailable while showing search results.
- **Duplicate** (⌘D) copies each selected item into its own folder as “name copy”.
- **Show in Finder** opens Finder with the selection, or the current folder, selected.
- **Copy Path** copies the selected paths, one per line, or the current folder's.
- **Quick Look** uses macOS's shared preview panel. GPUI's window class gains the panel-controller methods and a small data source serves up to 100 selected items. While the panel is open it previews the current selection, arrow keys and Space go back to the list, and Escape closes it.

**Drag and drop.** Dragging from a row starts a native macOS drag (`drag_out`) of the selection, with system icons as the drag image, so files can be dropped on Finder, the Desktop, or any app. A plain press on a row in a multiple selection keeps the whole selection so it can be dragged, and selects just that row on release; a marquee starts only on blank space. AppKit swallows the mouse release during a drag, so one is posted back when the drag ends; GPUI otherwise keeps synthesizing drag moves. Files dropped on the app, from its own list, another tab, or another app, arrive as GPUI file drops. While a drag from the list is in progress, the dragged rows show the sidebar's active-row gray (`0x303030`) instead of the selection blue and are not drop targets themselves, so blue marks only where the drop will land. Drop targets highlight in blue: folder rows (not packages such as `.app`), the list itself (its current folder; not while showing search results), sidebar favorites, path bar segments, and tabs. `file_ops::transfer` moves within a volume and copies across volumes, or always copies with Option held, through `NSFileManager`; taken names are numbered, copying into the same folder makes “name copy”, and dropping a folder on itself or where it already is does nothing. A drop that moves or copies anything plays the Dock's “poof” sound (`ui::Sound::Move`), under the same System Settings condition as the Trash sound; a drop that changes nothing is silent. Affected folders refresh afterwards; after a drag out, the source folders refresh a moment later in case another app moved them.

**Undo.** ⌘Z undoes and ⇧⌘Z redoes file changes made in this app, app-wide as in Finder: Move to Trash (put back), moves from drops or ⌥⌘V (moved back), renames, and anything created by a copy, paste, Duplicate, or New Folder (moved to the Trash, as Finder does). `undo::UndoHistory` keeps the last 100 changes for the session; each change is a list of steps that one ⌘Z reverts together, so a whole drop undoes at once. Undoing never replaces anything: a step whose original name has since been taken fails with the alert sound while the rest still run. One undo or redo runs at a time, off the UI thread; tabs inside a folder that moves back follow it. ⌘Z applies only while the file list has focus, so text fields keep their own. Changes made by other apps are not undoable here.

While renaming, the list's own shortcuts are off so Space types a space. After a change, tabs listing the affected folder refresh in place. Searches of indexed folders already show the change and refresh when the index sees it; an on-the-spot folder index is rebuilt.

## Titlebar search

A search button sits at the right end of the tab row. Clicking it or pressing ⌘F expands it into a 260-point field with a fading 32-point background; the magnifier stays at the field's leading edge, and the text fades in as the field grows. Tabs shrink to make room as it expands. Escape collapses it and returns focus to the file browser; clicking outside or moving focus away collapses it only when the field is empty. Typing runs a debounced fsearch query.

A scope pill at the field's trailing edge shows where search looks: the current folder's name (Macintosh HD at `/`), meaning that folder and everything below it, or All, meaning every indexed folder. Clicking it switches; the choice is the `search_scope` user setting and defaults to the current folder. The pill is 22 points high with the small radius and `DETAIL` text, truncating long folder names at 96 points.

Searching a folder inside the indexed roots uses the saved index limited to that subtree. Searching a folder outside them, or inside a pruned folder, first builds `search_index::FolderIndex`: a temporary in-memory fsearch index of that folder, crawled breadth-first in the background with the same pruning rules and through the folder's real path (so `/tmp` works). Results then rank and tolerate typos like everywhere else, and later keystrokes reuse it. It is dropped, or its crawl cancelled, once no tab is searching that folder, and never saved. A crawl stops quietly after 200,000 entries, nearest levels first, so searching from `/` stays bounded; never tell the user about this limit or any result count. While the folder index builds, the delayed loading bar shows. Escape clears the query, restores the tab’s directory listing, and collapses the field. ⌘F does nothing while the command menu is open.

## Saved state

`storage` owns everything the app keeps in `~/Library/Application Support/Filetree/` besides the index: `settings.json` (below) and `session.json`. Both are written through a temporary file and rename, take defaults for missing fields, and are moved to `<name>.invalid.json` rather than overwritten if unreadable.

The session restores the window frame (a maximized or fullscreen window returns to its saved frame), each tab's folder and the active tab, zoom, the sidebar's open state and section states, and recent items. It is saved when the window closes, at quit, and whenever recent items change. Tabs whose folders are gone are dropped, falling back to Desktop; a window frame that no longer overlaps a connected display by at least 100 points opens centered at the default size. Searches and back/forward history are not restored.

## User settings

`storage::UserSettings` is the single source of truth for preferences; the command menu and the settings panel both read the one copy held by the browser and toggle it through the same action. Settings persist as pretty-printed JSON at `~/Library/Application Support/Filetree/settings.json`, loaded at launch and rewritten through a temporary file and rename on every change. Missing fields take defaults, so older files load and new settings need no migration. An unreadable or invalid file is moved to `settings.invalid.json` instead of being overwritten.

Boolean settings use `ui::checkbox`, a 16-point box with the extra-small radius: blue with a white checkmark when on, a gray outline when off.

**Play sounds** (⌘K, Settings, and the sidebar's speaker button; stored as `mute_sounds`) defaults to on and silences the Trash and move sounds when off. The system alert for a failed action is macOS's own and is not affected.

**Show hidden files** (⌘⇧., also in ⌘K and Settings) defaults to off. Off hides entries whose names start with a dot in folder listings; browsing into a hidden folder still lists its contents. Search results also drop anything inside a hidden folder. ⌘K path completions show dotfiles when it is on, and otherwise only after a dot is typed. The folder cache keeps every entry and filtering happens on display, so toggling updates all tabs in place without re-reading, keeping scroll and selection. Running the command from ⌘K keeps the menu open so its checkbox shows the new state.

## Command menu

⌘K toggles a floating command menu near the top center of the window. Use the system font, shared sizing tokens, and the app's charcoal palette, with a shadow and blue-gray selection and no border. The panel is 560 points wide, constrained to the viewport with 16-point side margins. The search field is 52 points high with `INPUT` text; command rows are 40 points high with `BODY` text.

Opening the menu clears and focuses the search field. Filter live using case-insensitive words in any order; each word must match the command label or its search keywords. Show “No matching commands” when the result is empty.

Commands are **Settings** (also ⌘,), **Reset to Default Zoom** (also ⌘0) and **Clear File Index**; a command with a shortcut shows it at the row's trailing edge. Up/Down changes the selected result. Enter runs it and closes the menu; it does nothing when no result matches or text composition is active. Escape, another ⌘K, or a click outside dismisses the menu. Restore focus to the file browser when closing. The overlay blocks mouse and scroll interaction with the content behind it.

Below the commands, separated by an 8-point gap, the menu lists recent items as path rows: the newest five with nothing typed, otherwise every recent item whose path contains each typed word. They are captured when the menu opens, so typing never touches the disk, and behave like path completions: Enter opens, Tab fills the field.

A query starting with `/` or `~` switches the menu to path entry, handled by `path_completion`. Only the folder being typed into is listed, off the UI thread and once per folder, so it works anywhere on disk without the search index. Rows show the typed path itself first when it exists, then children whose names start with the typed text, then those containing it, up to eight, with the same per-type system icons as the file list. Every row shows its full path, using `~` inside the home folder, so a row reads the same whether or not its name is fully typed. Hidden items appear only after typing a dot, which matches by prefix only. Tab accepts the selected row into the field, adding a trailing slash to folders and keeping `~` if it was typed. Enter opens the selected row like a double-click: folders open in the current tab, files and packages in their default app. Show “No matching files” when nothing matches.

Search uses a single-line input adapted from the GPUI example, with text selection, clipboard shortcuts, native IME input, and horizontal scrolling. Keep the upstream notice and [Apache license](../licenses/GPUI-LICENSE-APACHE).

## Settings

⌘, or the **Settings** command toggles a 600-point panel sized to its contents, centered, and kept inside a 32-point window margin (it scrolls if the window is shorter), over a backdrop that blurs the window behind it (24-point Gaussian, from the vendored gpui's `paint_backdrop_blur`) and then dims it like the command menu's overlay, with the same surface, radius and `INPUT` heading row. Its contents scroll if the window is short. Escape, another ⌘,, a click outside, or the 28-point × button at the heading row's trailing edge (its 12-point glyph on the content's right edge) closes it, and focus returns to the file list. Opening it closes the command menu.

The narrow width keeps controls near their labels; the title aligns with the section headings, and the × stays at the panel's top-right corner. Settings are grouped under small semibold muted headings (**General**, **Storage**) on rounded groups one fill step above the panel (`#2a2a2a`, `radius::MD`), with 40-point rows separated by hairlines. The storage group holds **File index**: the saved index's size on disk, a 14-point bar split by indexed folder, and a legend of dot, label and size. The five largest folders get the validated dark categorical slots in order (blue, orange, aqua, yellow, magenta), and everything else, including the index's fixed overhead, is **Other** in a neutral gray chosen to stay separable from magenta under color-vision deficiency. Segments are 2 points apart with `radius::XS` corners. Each folder's subtree is one contiguous entry range in the index, so the split costs a few lookups per folder; its bytes are its share of entries, an estimate because names are shared, while the total is the exact file size. Folders in Home are labeled by name and others by full path, so `/Applications` stays distinct from `~/Applications`. The figures are measured when the panel opens and refresh while it stays open. Sizes use tabular numerals.

## Tabs, startup, and file indexing

⌘1–⌘8 switch to tabs 1–8 and ⌘9 to the last tab, as in browsers; a number past the last tab does nothing. Switching scrolls the tab into view and returns focus to its file list.

Start the first tab on Desktop. New tabs inherit the current folder with an empty search. Each tab owns its folder, query, list, sort, column widths, selection, and scroll position. Switching or closing tabs restores the active tab’s query without replacing another tab’s data. Before removing a tab, dismiss the command menu and focus the surviving browser root so keyboard shortcuts cannot remain routed to a detached list or search field. ⌘W closes the active tab, and closing the last tab closes the window. Favorites change only the active tab. File-row opening and directory double-click actions remain deferred.

Search uses the bundled fsearch name index and query parser, including typo tolerance and filters such as `ext:`. The chosen scope is Desktop, Documents, Downloads, Pictures, Music, Movies, and Applications under Home, plus `/Applications`. Do not crawl the rest of Home or `~/Library`, follow directory symlinks, cross mounted directories, or start the standalone daemon or content index.

Keep the app’s cache at `~/Library/Application Support/Filetree/common-index-v2.bin`, separate from the standalone fsearch cache; launch removes the older, larger `common-index.bin`. Reuse the saved snapshot on launch without rebuilding it. Persist the native FSEvents cursor with the snapshot; replay changes made while the app was closed, then watch for new changes. Relist affected folders and scan newly discovered subtrees only within the configured scope.

Index `node_modules`, `.git`, `.app` bundles, and a Cargo `target` beside a `Cargo.toml` as entries, but not their contents (`fsearch::walk::pruned`, enabled by the app through `walk::PRUNE`). The folders stay searchable and browsable; their contents are not searched, as in Finder and Spotlight. The initial build, live relisting, new-folder scans, and FSEvents handling all apply the rule, and events inside a pruned folder are ignored. FSEvents reports folders with a trailing slash; the rule accounts for it.

Watch only the indexed roots that exist at startup (`fsevents::watch_paths`), not the whole disk, so builds and other activity elsewhere cannot make fseventsd drop events. A root created later is picked up on the next launch.

If events are dropped or history is unavailable, recheck only the affected roots: check folders, not files, in parallel against their modification time since the last save (`Live::changed_dirs_within`), then relist the folders that changed. Adding, removing, or renaming an entry updates its folder's mtime; file sizes and dates are re-read whenever they are shown. After a search, fsearch keeps its scored name table and spare buffers (tens of MB after a broad query) for the next keystroke; a minute after the last search the app releases them with `NameCache::trim_if_idle`, as fsearch's own daemon does. fsearch records folders it was refused only for its daemon's skip list, so the app's rescans do not accumulate them. The saved index stays searchable throughout; searches share its read lock, and only the worker's own updates wait, after the recheck. Older snapshots without a saved cursor receive the same quiet recheck.

The index worker sleeps until FSEvents reports a change, a pending checkpoint falls due, or cancellation wakes it; it does not poll. The UI is told about progress and revision changes through a bounded channel that coalesces bursts, so an idle app does no periodic work. A search that finishes against an older index revision is re-run quietly once.

Run indexing on a utility-priority worker. Filesystem reads share the index’s read lock with searches; applying changes holds a brief write lock. Saving rebuilds the whole index, so compact and atomically checkpoint pending changes only once 50,000 are pending or after 12 hours, as fsearch does. The app uses fsearch's allocator so large transient blocks return to the system after a save. Keep the existing snapshot available during saving. A failed checkpoint retains the live overlay and retries later. The saved cursor makes unsaved changes replayable on the next launch. Build from scratch only when no usable snapshot exists or **Clear File Index** is chosen. Clearing cancels older generations and removes only the app-owned cache; old workers cannot republish stale data, and staging files are cleaned after cancellation. Dropping the last service handle stops its watcher.

Use the shared 140ms eased `Reveal` for both the status section’s height and opacity, starting hidden. Reversing a rebuild or completion mid-animation continues from the current amount. A first build or clear/rebuild shows the section, and errors remain visible. Routine cached startup, event replay, live updates, and checkpointing do not show indexing progress.

The status above Trash shows a crawl percentage based on completed and discovered directories, without a tilde prefix. Show only the status label and percentage above the bar. Keep the label flexible and the percentage in a fixed-width, right-aligned cell with tabular numerals and a fixed row height, so updates do not shift neighboring text. The denominator grows during discovery, so the estimate may decrease. Reserve the final portion for index building and saving; show 100% only after publishing the completed index. Never display a time-based invented percentage.

Debounce queries by 120ms and run lookup and result metadata reads off the UI thread. Per-tab request numbers reject stale responses and results for closed tabs. Retry pending searches when a newer index snapshot becomes available. Preserve relevance order unless the user has explicitly chosen a column sort. Results are capped at 1,000 without a notice. A new search keeps the previous results visible until the next ones arrive. Show loading as a 2-point indeterminate blue bar across the top of the list, only after a load has run for 150ms so quick results never flash it. Show empty and error states as muted text centered in the empty list; never as a status line above the rows.

Refresh existing searches quietly when the index changes: keep scroll offset, selection by file path, and column sorting. Do not show a loading bar for these background refreshes. Coalesce index revisions while a query is running, then apply the latest revision without repeatedly cancelling results. Defer replacement rows until an active marquee or column resize finishes. Folder listings refresh the same way on every visit.

## Permissions and packaging

`scripts/bundle.sh` builds `target/bundle/Filetree.app` from `packaging/Info.plist` with an ad-hoc signature and the generic identifier `local.filetree` (or `BUNDLE_ID`). It rewrites the builder's home folder to `~` in the source paths Rust embeds for error messages (`--remap-path-prefix`), so the app never contains a local username, and fails if one slips through. It builds in its own target folder so those flags do not force everyday builds to recompile. The app icon is `packaging/FileBrowser.icon`, an Icon Composer package with one full-bleed 1024-point layer (a white folder on cyan); the script compiles it with Xcode's `actool` into `Assets.car`, which macOS 26 and later draw in the system shape, and `FileBrowser.icns` for older systems, named by the plist's `CFBundleIconFile` and `CFBundleIconName`. Without Xcode the app builds with the generic icon. The plist carries the usage descriptions macOS shows when the app first opens Desktop, Documents, Downloads, external drives, or network drives. Run from Terminal, the app inherits Terminal's access and never prompts, so test prompts with the bundle; ad-hoc signatures change every build, so macOS asks again after each rebuild, and `tccutil reset All <bundle id>` resets them.

Official releases are built by the maintainer's private release script, which is not in the repository. It runs `bundle.sh` with the official identifier and the update settings below, then signs with a Developer ID certificate (hardened runtime, secure timestamp), notarizes, staples, and zips the app for a GitHub release tagged `v<version>`. No identity, team, or credentials live in the repository; keep the official identifier stable, since macOS grants folder access per identifier.

**Updates.** A build updates itself only when it was compiled with `FILETREE_UPDATE_REPOSITORY` (a GitHub "owner/name") and `FILETREE_RELEASE_TEAM` (the signing team), is a release build running from an app bundle, and that bundle is signed by that team, checked with codesign off the UI thread at launch. Builds from source have neither setting, so they never offer or contact official releases. An updating build checks the repository's latest GitHub release at launch and every three hours, silently. The command menu's Check for Updates (listed only in updating builds) checks at once: a spinner takes the Update button's place while it looks, then the button appears if there is a newer release, or a native alert says the app is up to date or that the check failed. A newer, non-draft, non-prerelease release with a `.zip` asset shows the titlebar's Update button. Clicking it downloads the zip with system `curl`; the accent fills the button from left to right behind “Updating” as it arrives, then “Installing” while it is verified: size and GitHub's SHA-256 digest must match, the extracted app must satisfy the running app's designated code requirement (same team), and its version must match the release. The new app replaces the old in one atomic swap beside it, and the app quits (saving the session) and reopens itself, with no separate restart step. Any failure leaves the running app untouched, plays the alert, and offers the update again. Setting `FILETREE_PREVIEW_UPDATE` in a debug build shows a pretend update whose download only counts up, to preview the button. Everything goes through macOS's own tools, so the app carries no HTTP or TLS library.

Indexing reads the protected folders in the background, so first-launch prompts never block the window. A folder macOS denies shows, in the file list, where to grant access instead of "Operation not permitted"; the index leaves denied folders out.

## Failure containment

The index worker and the UI share the index's locks. Those locks recover their data if a thread panics while holding one, the worker catches its own panics and reports an index error, and each search catches panics as a failed query, so an index failure never takes down the app.

## File-size handling

The directory view lists names and paths regardless of file byte size; it has no file-size cutoff. The application does not read file contents to draw these rows. File icons are requested from macOS, and their raster dimensions are based on the displayed icon size, not the size of the file on disk.

## Verification

After changing shared sizing or rendering, run `cargo fmt`, `cargo check`, `cargo clippy --all-targets -- -D warnings`, and the relevant tests from the project directory.

Launch or inspect the running application only when explicitly requested.
