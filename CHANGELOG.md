# Changelog

## 0.1.5

- Copies of other computers' index: what a connected computer opened to you is kept
  here and refreshed when it changes. Search everywhere uses these copies, so results
  from other computers appear instantly and even when they are switched off ("as of"
  the copy date). Closing access or removing a computer deletes its copy.
  Both computers need 0.1.5 or later; with older versions search asks over the network.

## 0.1.4

- Search everywhere: a switch under the results ("search everywhere" / "only in this
  folder") searches all disks, folders and S3 of this computer and the connected
  computers in one query. A "Where" column shows the source of each result; results
  open, copy and delete wherever they are. The choice is remembered.
- Dialogs, including the update prompt, are now visible on every tab.

## 0.1.3

- Duplicate search right in the search field:
  - `dup:size` answers instantly from the index, on all sources: copies of whole
    folders (same total size and file count) and files of the same size. Marked
    "same size" because contents are not compared.
  - `dup:` checks contents in the open folder and below, `dup:all` on all disks and
    S3. The biggest possible savings are checked first; groups appear as "matches by
    samples" and then become "exact copy" once compared in full.
  - Files from 1 MB by default; add `size:>0` for smaller ones. Filters like
    `ext:mp4` work too.
  - S3 objects are compared by their ETag without downloading; a local file of the
    same size is checked by MD5. Buckets are re-listed once to collect ETags.
  - Found files and folders are regular rows: select, delete, copy as usual; the
    delete prompt shows full paths.
- The window no longer freezes while sources, folders or search results load:
  heavy work runs in the background.

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
