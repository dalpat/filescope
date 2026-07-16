# Changelog

## [Unreleased]

### Added
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
