# Notes

## Later

- Undo, for every change: the board's `x` removes for good, and so does every other key's change.
- A save that crashes leaves its `tasks.jsonl.<pid>.tmp` behind, and nothing clears it.

## Done in 0.5.0

- Repeating (the design session's shape): `every …` inside the `//` mark; `task::roll_repeats` after every change,
  under the lock, makes the next one from a done repeating task (one, however many halil are at it), the step's
  first date after the day it was done; a month or year step keeps its aimed day. The done one keeps `"next"`, the
  successor's stamp, for the reopen note. Taking `every …` out of `e`'s line leaves the step, as the date's rule
  leaves the date (0.4.1): `-every` stops it.
- Finding: one filter (`filter.rs`), one reader, for `halil list`, `/` on the board and the calendar.
- The lock's wait is said after 5 s, not hung; `board.json` is written under it too.

- Several `halil` at once lost tasks: each read, changed and wrote `tasks.jsonl`, and the last writer won (40 adds
  at once kept 1 to 4 with 0.4.2). Every change is now `store::update`: an advisory lock on `tasks.jsonl.lock`, held
  from the read to the write, by commands and the board alike. `std::fs::File::lock`, so Rust 1.89 is the least.
  Reads alone (the list, the board's first load and its reloads) take no lock: the save's rename makes a read see
  the old file or the new. Not locked: the board's `board.json` (folds), written by one board at a time.
- Days with a year: `dec 31 2026`, `december 31st, 2026`, `31 dec 2026`, `31st of december 2026`, `2026 dec 31`,
  `2026/12/31`; `20th of nov` and a comma after the day without one. `12/31/2026` and `31.12.2026` are refused in
  words: either order could be meant.
- "태그 edit 불가": `halil edit 3 -home` printed the help and changed nothing, since clap took `-home` for `-h`. The
  words of `add`, `edit` and `sub` now take a first word starting with `-`; `-h` alone is still the help (`--`
  before it for a tag named h). `e` on one card took `+tag` and `-tag` already; now pinned.

- The calendar (`c`, `halil cal`). A task's done and added times are kept as the clock read where they happened,
  with the offset and the zone's name (`"done":"2026-11-25T18:04:00+09:00[Asia/Seoul]"`), and placed by the day
  written, never converted: one rule for both, held by one property over zones and clock changes. Every change of
  status goes through `task::set_status`: into Done sets the time, Done again keeps it, out of Done clears it.
  Done before 0.5.0 and added before 0.3.0 (a stamp below 2³² is a hash, no time) are "date not known", in one row
  under the grid; no date is made up for them. Late, on the calendar as in the list, is judged by today where halil
  runs now: a due date has no zone.
- `d` is done, in both views; in the calendar it never opens a done task again, since that clears the done time it
  exists to show: a move (`h` `l`, `s`, `t`) does. On the board, `d` on a done sub item still opens it, the one way
  a sub item opens there. The board's footer shows `c calendar` at 80, and the calendar's always shows `c board`.

## Done in 0.4.2

- Under WSL, the board's `y` no longer uses `clip.exe`, which read the title in the console's code page and pasted Hangul as garbage (`?댑슸`): `win32yank.exe` when it's there, else `wl-copy` under WSLg, else `powershell.exe` reading UTF-8, else OSC 52.

## Done in 0.3

- Ids are reused after a remove, so on the board a mark or an open input could follow an id to a new task. Mended
  by a stamp each task gets when it's made (`"stamp"`, milliseconds since 1970): the board holds id and stamp, and
  a change to a task whose stamp no longer matches is refused as gone. Ids are still reused.
