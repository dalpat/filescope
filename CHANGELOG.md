# Changelog

## [0.6.0] - 2026-10-21

Indexed search, a preview pane, and a terminal inside each tab.

### Added
- **GNOME Overview search** with ranked results, file-type icons, readable
  locations, and a working “more” action. **Ctrl+F** searches the current folder
  and its indexed descendants; load more results without the Overview limit.
  A bounded fallback remains available when the index cannot be used.
- **Preview pane** with image/text previews and file properties, including
  properties alongside image previews.
- **Terminal pane** per tab, toggled with Ctrl+Backtick. Hiding it retains the
  shell and its working directory; closing the tab terminates the session.
- **Missing thumbnails** generated through installed desktop thumbnailers,
  with validated cache entries, cancellation, and process timeouts.

### Changed
- **New Folder** creates and selects the folder immediately, with inline name
  editing and numeric suffixes for existing names. Rename and creation can be
  undone separately; creation undo preserves files added afterward.
- **Zoom** resizes existing grid/list cells without resetting selection or
  scrolling. Names grow with icons, and icons reload at the requested resolution.
- **Shared theme icons** refresh visible consumers, including empty states,
  when the theme changes.
- **Status bar** uses concise item and selection information.

### Fixed
- Closed tabs release retained view callbacks and terminal resources, including
  shells still starting or ignoring the normal shutdown signal.
- Shell exit status stays visible, and an unset Ptyxis profile uses fallback colors.
- Empty or invalid inline names report an error without adding an undo action.
- Search supports symlinked scopes, keeps real paths for duplicate names, and
  disables folder sorting while results are ordered by relevance.

### Installation
- The terminal pane requires the **VTE GTK4 runtime** (`libvte-2.91-gtk4-0`
  on Debian/Ubuntu). Source builds also need `libvte-2.91-gtk4-dev`.
- GNOME search registration is installed system-wide and may request
  administrator authentication; the app itself remains installed per user.

## [0.5.0] - 2026-10-13

A home page for the computer, and a folder of photos no longer decodes every
original just to show an icon.

### Added
- **This PC** — quick access, drives with free space, and recent files. Back
  from a folder opened there returns to This PC, and the sidebar highlights it.
- **Rubber-band selection** — drag empty space to select a rectangle of files.
- **Execute permission** in Properties, for a file that can be a program.

### Changed
- **Zoom** (Ctrl+wheel, or the header buttons) resizes the icon and the name
  together, including This PC and Trash. Trash uses the same grid and list as a
  folder.
- **Double-click** opens the default app. The chooser appears only when none is
  set.
- **Thumbnails** use the small picture already in the thumbnail cache. The
  original file is left alone, so a photo folder stays idle.

## [0.4.0] - 2026-10-13

Other apps can open a folder here, Space uses Sushi when it is installed, and
the right-click menu behaves like Files.

### Added
- **Default file manager** — turn it on from the menu. A second launch opens
  that folder in a new tab. "Show in folder" from other apps then opens
  filescope. Turning it off hands folders back to Files.
- **Sushi preview** — Space opens GNOME Sushi when it is installed, and the
  built-in preview when it is not.
- **Reorder bookmarks** — drag a bookmark to a new place. The remove button
  stays hidden until you hover the row.

### Changed
- **Hidden files** are faded, and they sort after normal files and folders.
- **Right-click** selects the item under the pointer. The menu lists only what
  applies to a file, a folder, or empty space, and a short menu no longer
  scrolls.

## [0.3.0] - 2026-07-16

Trust and polish: nothing you do is a one-way door any more — undo it, or fish it
back out of the Trash — plus archives, in-place renaming, and a filescope that
remembers how you left it.

### Fixed
- **Space and Backspace couldn't be typed while renaming** (and would have hit
  search and the path bar too). Space previewed and Backspace navigated up
  instead of reaching the text field: they were window accelerators, which GTK
  dispatches *before* the focused widget. Space / Backspace / Delete / Enter are
  no longer global — they act on the file view only when it has focus, so entries
  get them first. A test now rejects any bare (unmodified, non-function) key in
  the accelerator table.

### Added
- **Archives** — right-click → **Extract Here** unpacks `.zip`, `.tar`(`.gz`/
  `.bz2`/`.xz`/`.zst`), `.tgz`, `.7z` and `.rar` into a new folder named after the
  archive (never littering the current one), and **Compress…** packs the
  selection into a `.zip`, `.tar.gz` or `.tar.xz`. Both run in the background with
  a progress row and Cancel, and say so plainly if the needed tool
  (`unzip`/`7z`/`unrar`…) isn't installed.
- **Inline rename** — `F2` now edits the name on the item itself (in both grid
  and list) instead of opening a dialog. The stem is pre-selected so typing
  replaces the name but keeps the extension; folders and dotfiles select the
  whole name. Enter commits, Escape cancels, clicking away cancels. Falls back to
  the dialog if the row isn't on screen to host the editor.
- **Trash view** — a Trash entry in the sidebar listing everything you've
  deleted, with where it came from and when. **Restore** puts an item back at its
  original path (or beside it, if something has since taken the name — it never
  clobbers), and **Empty Trash** clears it for good. Implemented against the
  freedesktop trash spec, including percent-decoded original paths.
- **Preferences persist** — view mode, zoom, sort column/direction, show-hidden,
  and the window size/maximised state are saved to
  `~/.config/filescope/settings` and restored on launch. Unknown or malformed
  entries fall back to defaults rather than losing the file.
- **Undo / redo** (`Ctrl+Z` / `Ctrl+Shift+Z`, also in the right-click menu) for
  moves, copies, renames, new folders, and trashing (restored out of the Trash).
  The background engine reports the exact paths it acted on, so undo puts things
  back where they came from rather than guessing. Permanent deletes and entries
  you chose to *Replace* are deliberately not recorded — they can't be restored.

## [0.2.0] - 2026-07-14

Big update focused on making file operations robust and drag-and-drop feel
native.

### Added
- **Background file operations** — copy, move, and delete now run on a worker
  thread with a live **progress bar** and **Cancel**, so the window never
  freezes on a big transfer. Chunked copies keep a 4 GB file's bar moving and
  stay cancellable mid-file. Name clashes raise a **conflict dialog** — Replace /
  Skip / Keep Both, with *apply to all*.
- **Real system clipboard** — Copy / Cut / Paste use the desktop clipboard
  (`x-special/gnome-copied-files` + `text/uri-list`), so you can copy in
  filescope and paste in Files/Nautilus and vice-versa.
- **Nautilus-style drag & drop** — drag a selection onto a **folder to drop it
  inside** (the folder highlights as you hover), onto empty space for the current
  folder, or into another tab or app. **Moves within the same filesystem, copies
  across** — hold **Ctrl** to force copy or **Shift** to force move.
- **Open Terminal Here** (`Ctrl+.`, plus the right-click and primary menus) —
  opens the current folder in your terminal.
- **Empty-folder placeholder** — a clear "This Folder Is Empty" state instead of
  a blank pane.
- Screenshots (grid, list, and the This PC drive overview) in the README.

### Fixed
- **Open Terminal** now opens a new window even for single-instance /
  D-Bus-activated terminals (ptyxis, GNOME Console, gnome-terminal), which
  previously just raised the existing window and ignored the folder.

## [0.1.0] - 2026-07-02

Initial release. Tabbed browsing; grid and list views with zoom and image
thumbnails; a places / bookmarks / devices sidebar; a Windows-Explorer-style
**This PC** drive overview with capacity bars and **mount / unmount** (NTFS via
`ntfs-3g`); **Space** to preview (Quick Look); copy / cut / paste, rename, new
folder, trash, delete, properties, open and open-with; hidden-files toggle;
sortable columns; and native GNOME theming (icon theme + light/dark + accent).
