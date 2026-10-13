# Changelog

## 0.1.2

- Double-click the path above the file list to get it as plain text, already selected:
  copy it, edit it, or paste another path and press Enter.
- Ctrl+C in the empty search field copies the path of the open folder.
- macOS: the app is signed locally, so macOS no longer reports it as damaged. It is
  still not signed by Apple, so the first launch has to be allowed once in
  System Settings, Privacy and Security.

## 0.1.1

- Tasks are now a table: what, from, to, progress, speed, time left, state.
- An action on several files or folders shows one row per object; each row can be
  paused or cancelled on its own, and the group row cancels them all.
- The tasks panel has a visible handle: drag its top edge to resize it.
- Moving files within one disk of another computer is now instant: it is a rename on
  that computer instead of sending every byte through this one. Both computers need
  0.1.1 or later.
- Fixed a test that failed on macOS.

## 0.1.0

First public beta.
