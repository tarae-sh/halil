# halil

A small task manager for the terminal. You write tasks in plain words, the list puts what's due first, and a
board shows them as Todo, Doing and Done.

![halil board: Todo, Doing and Done columns, each card its title, its tags and its due date in words](https://raw.githubusercontent.com/tarae-sh/halil/main/docs/board.png)

halil was built from an empty folder in one sitting with Claude in [tarae](https://github.com/tarae-sh/tarae).
Halfway, two Claudes worked side by side, each in its own checkout of this repository, and every change waited for
a yes before it was kept. Watch it in eleven minutes:
[Building a task manager with Claude, two at once](https://youtu.be/_GRlIMMMTmk). `start` and `edit` were written
after the sitting.

## Install

You need Rust 1.89 or newer.

```sh
cargo install halil
```

Or the latest from this repository: `cargo install --git https://github.com/tarae-sh/halil`.

## Use

```sh
halil add "pay rent friday 9am +home !high"
halil add call mom today 6pm +family
halil add plan the friday party //wed +home   # // marks the date
halil                 # the open tasks, what's due first
halil start 2         # to Doing
halil done 2
halil edit 1 next monday !med   # a new date and priority, the title kept
halil edit 1 //                 # the date taken off
halil sub 1 transfer it friday  # a sub item under task 1
halil rm 1
halil add water plants //every fri   # repeats
halil list +home //by fri       # only what matches
halil board
halil cal              # the board's calendar
```

`add` and `edit` read three things out of the words, and the rest is the title (`edit` keeps the old title when
no words are left, and the old date when none is given):

- **A due date.** A day (`today`, `tomorrow`, `friday` or `fri`, `next friday`, `in 3 days`, `in a week`,
  `nov 20`, `20th of nov`, `2026-11-20`), a time (`9am`, `9:30pm`, `14:30`, `noon`, `eod`), or both (`in 2
  hours`). A time on its own means the next time it comes. `at`, `by`, `on` or `due` before a date goes with it.

  A month and a day are the next one, today included; with a year they're that day, gone or not: `dec 31 2026`,
  `december 31st, 2026`, `31 dec 2026`, `31st of december 2026`, `2026 dec 31`, `2026/12/31`. `12/31/2026` and
  `31.12.2026` could be the day or the month first, and are refused, saying to give the year first or the month's
  name.

  Or marked with `//`, as `+` marks a tag: `//fri 6pm`, `//nov 27`. The words after it, up to the next `+tag`,
  `!priority`, `-tag` or the end, must read as one date, or the task is refused (`` `//fridya` isn't a date``).
  With `//`, the other words are all title, date words too: `plan the friday party //wed` is due Wednesday.
  Without it, date words are read wherever they are, as ever.

  `//` with nothing after it takes the date off, in `edit` only: `halil edit 3 //`, or `halil edit 3 plan the
  friday party //` with the title kept whole. `add` and `sub` refuse it (`` `//` needs a date``).

  Inside the `//` mark, after the date, how it repeats (see below): `//fri 6pm every week`.
- **A repeat**, inside the `//` mark only: `every day`, `every week`, `every month`, `every year`, `every 3 days`
  (weeks, months, years), `every fri` (weekly, on fridays), or `daily`, `weekly`, `monthly`, `yearly`. Anything
  else after `every` is refused, in the date's own words (`` `//every fortnite` isn't a date``).
- **Tags**, as `+home`. `edit` takes one off with `-home`, when the task has it: `halil edit 3 -home +work`. When
  it hasn't, `-home` among other words is a word of the title, and `-home` alone is refused (`3 has no tag home`).
  A word starting with `-` is a word, first or not; only `-h` alone is the help, so a tag named `h` comes off with
  `halil edit 3 -- -h`. (0.4.2 and earlier read a first `-home` as `-h`, printed the help, and changed nothing.)
- **A priority**, as `!high`, `!med` or `!low`.

The list shows due dates in words from now (`today 6pm`, `tomorrow`, `fri`, `in 3w`, `2d late`), red when late.
A due date is a day and maybe a time, with no zone: late means its day is gone on the clock where halil runs now,
so after a flight east a task due today can be late a few hours sooner than it would have been at home.
`NO_COLOR` turns colour off.

### Repeating

`halil add pay rent //dec 1 every month` is due Dec 1 and repeats monthly; `//fri 6pm every week` keeps the time
on each one. With no date, the first is the step's next from today, today included: `//every fri` is the coming
Friday, `//every day` today. A month or year step keeps its day: `//jan 31 every month` is Feb 28, then Mar 31.

Done, it stays done, with the day it was done, and no longer repeats; a new open task comes, the same title,
tags, priority and repeat, due the step's first date after the day it was done (done early, the one after its
date; done three weeks late, the coming one, not three late ones), its sub items all open again, their dates
moved as its own. The note says when: `Done. Next: fri dec 4.` Opening the done one again (`halil start`, or `h`
`s` `t` on the board) makes it a plain task, and says where its repeat went (`Reopened. Its repeat is on #12.`).

`halil edit 3 //sat` moves the date and keeps the repeat, and says so (`Still every week — -every stops it.`);
`halil edit 3 //sat every 2 weeks` changes both; `halil edit 3 -every` stops it, and `//` alone, the date taken
off, too. On the board, `e` fills in the repeat after the date (`pay rent //2026-12-01 every month`); taking those
words out of the line leaves the repeat as it is, as taking the date's words out leaves the date, and the row
above the footer says so. On a line whose repeat is a weekday (`every fri`), the calendar above it shows the day
the line reads as, and a move writes the picked day's weekday too (`//2026-12-05 every sat`). `halil sub` with a
repeat is refused: a sub item repeats with its card.

A repeating card shows its repeat after its date, faint (`fri weekly`), where its title still fits whole; where
the title is cut, `*` after the date (`fri*`), and the picked row, drawn whole, spells it. The calendar shows only
real tasks: a repeat's coming dates aren't drawn.

### Finding

One filter, written with `add`'s marks, for `halil list`, the board and its calendar: words (in the title, every
one, case ignored), `+tag` (all of them), `!priority`, and `//fri` (due that day), `//by fri` (due by then, late
ones too; `//by sun` is this week) or `//late`. A task with no date matches none of the three. A card shows when
it or one of its sub items matches, drawn as ever. `halil list +home milk` keeps each task's own number, so
`halil done 7` works from it; nothing found says `Nothing matches +home milk.` `halil board +home` and `halil cal
+home` open with it on.

### Sub items

A sub item is a task under another task, one level deep. `halil sub 3 write the tests friday` adds one under
task 3, with the words of `add`. It has an id of its own, so `start`, `done`, `edit` and `rm` work on it as on any
task.

The list shows a task's open sub items under it, indented, and how many are done after the task (`2/4`). An open
sub item of a done task is listed as a task of its own.

`halil done 3` finishes its open sub items too, and says how many. `halil rm 3` removes them with it. A task never
finishes by itself when its last sub item does.

### The board

| Key | |
|---|---|
| `j` `k`, `↓` `↑` | pick a card or a sub item, row by row |
| `g` `G` | the first or last row of the column |
| `space` | mark it, or unmark it, and step down |
| `h` `l` | move it to the column left or right; a sub item moves with its card only |
| `s` | move it to Doing; a sub item moves with its card only |
| `t` | move it to Todo; a sub item moves with its card only |
| `tab`, `←` `→` | go to another column, to the row on the same line, or its last row when it's shorter |
| `a` | add a task, with the same words as `halil add` |
| `A` | add a sub item under the picked card (or the picked sub item's card) |
| `e` | change it, its title and date filled in (`pay rent //2026-11-27 6pm`), with the same words as `halil edit` |
| `d` | move it to Done, its open sub items with it; on a sub item, done or open again |
| `x`, `Delete` | remove it, a card with its sub items, after asking |
| `y`, `enter` | after `x`: remove; any other key keeps it |
| `y` | copy its title to the clipboard |
| `z` | fold the card's sub items away, or unfold them; on a sub item, its card's |
| `c` | the calendar (below), and back |
| `/` | the filter (below): typed where `a` types, the columns narrowing as you type |
| `esc` | clear the marks, else the filter, or quit when there's neither |
| `q` | quit |

`e` fills in the title, then the date after `//`, its day named and its time as you'd type it (`pay rent
//2026-11-27 6pm`), so it reads back as the same day whenever you save, past midnight too. The cursor starts after
the title, so typing goes on with the title. `enter` with the line untouched changes nothing, and a date word in
the title stays title, as with any `//`. Change the
date in place, or clear the words after the `//`, keeping it, to take the date off, as `halil edit 3 //` does;
taking the `//` out too leaves the date as it is, as `halil edit` with no date does. A task with no date shows its
title alone, or `plan the friday party //` when a word of the title would read as a date.

With marks, `h`, `l`, `s`, `t`, `d`, `e`, `x` and `y` act on all the marked at once, and `esc` clears them. `e`
then takes a date, `//` alone to take their dates off, `+tags`, `-tags` or a `!priority`, not a title; `y` copies
each marked title, a line each, in the board's order.

The picked row shows whole: when its title is cut or its tags or due date left out for room, it's drawn over the
columns to its right, on its own ground, covering just the cells it needs (its text and one more). The rest of the
board is drawn as ever, and a neighbour's row is cut where the picked row ends; a double-width letter there, such
as Hangul, is covered whole, never cut in half. Only a row longer than the room to the board's edge wraps (in
Done, under itself), and its lines are drawn over the rows under it, which stay where they are.

`z` folds a card: its sub items aren't drawn, `j` and `k` step past them, and the card's count shows it (`▸ 2/4`).
`z` again, or `A` to add one, opens it. A folded card never hides what's due: the count turns red when an open sub
item in it is late, and one due before the card's own date (or when the card has none) gives the card that date,
which it sorts by. Folding clears the marks on its sub items, and says how many. The other keys act on the card,
as ever; `x` still names its sub items. In Done, sub items are folded into the count already. The board keeps
which cards are folded across runs, in a file of its own (see below); `halil`'s list shows sub items as ever.

Where you type after `a`, `A` or `e`, the second `/` of a `//` opens a calendar above the line, and so does the
cursor moved into a `//`'s words from outside them (`ctrl-e` after `e`, or `→`). Typing stays typing, and the
calendar follows the date the words after `//` read as (on `e`, the task's own while they read as none). `←`
`→` move it a day, `↑` `↓` a week, `PageUp` `PageDown` a month, and each move writes the day after the `//`,
named as `e` fills it in (`2026-11-27`, `2026-11-27 6pm`), so it's saved as the day the calendar showed: a day
already gone, or saved past midnight, too. A time you typed is kept: a time is typed, not picked (`//fri 6pm`). While it's open, the arrows are the calendar's; `enter` saves, `esc` closes
it and leaves the words, and a second `esc` leaves typing. Another `//`, or the cursor moved out of the words and
back in, opens it again. On a screen under 10
rows, there's no calendar, and the row above the footer says to type the date.

`y` copies with the platform's own command when there is one (`pbcopy` on macOS, `wl-copy` or `xclip` on Linux;
under WSL `win32yank.exe` when it's on `PATH`, else `wl-copy` under WSLg, else `powershell.exe` told to read the
title as UTF-8, so Hangul comes through whole), and else sends the title to the terminal's clipboard with its OSC 52
sequence; the row above the footer says which, and whether the command failed. Under WSL it never uses `clip.exe`,
which reads in the console's code page and garbles Hangul. Not every terminal takes OSC 52, and some pass it on only
when told to: zellij passes it on, and tarae's own terminal pane drops it for now.

Sub items show under their card, `◦` open and `●` done, and the card counts them (`2/4`). In Done they're folded
into the count. A card that goes to Done takes its open sub items with it, and one moved back out of Done
reopens none.

`x` asks in the footer first (`Remove 3 "pay rent" and its 2 sub items?`), and a removal can't be undone. A card's
sub items go with it, as with `halil rm`; a picked sub item goes alone.

The footer shows the keys that act on what's picked: not `t` on a card in Todo, `s` in Doing or `d` in Done, and
none of `h l`, `t` and `s` on a sub item. When the window is narrow, `tab` gives way first, then `y`, `q`, `j k`,
then `space`, then `s` or `t` (`h l` says the other column too), `z`, `A`, `x`, `/` and `c`; `h l`, `d`, `a` and
`e` never do, and on a sub item `d` says `d done/open`. With a filter kept, its line and `esc clear` never go, and
`/ filter` isn't shown: the line starts with `/`. Where you type, the hint at the right gives way to what you
type, `esc to cancel` first, so the end of the line stays in sight. Room left over goes back to the keys
that gave way, the last to go first, any that doesn't fit skipped. With marks, the footer says how many are marked
and the keys for them. After `a`, `A` or `e`, it's where you type. The row above it says why a key did nothing, or
what else a change did.

Every change is saved at once, under the tasks file's lock (see below), so the board and `halil` commands in other
terminals never write over each other's changes. A `halil add` from another terminal while the board is open shows
up on it, and a task removed there while it's marked or being changed here is said to be gone, never mistaken for
one that took its number since.

`/` opens the filter (see Finding) where `a` types; the columns narrow as you type, and a half-typed `//` just
doesn't narrow yet. `enter` keeps it and gives the keys back, every key acting on what's shown; `esc` while typing
drops what you typed. A kept filter stays in sight where you type, `/ +home milk · 3 of 12` (cards shown, of all),
and the keys give way before it does; `esc` on the board clears it, and `/` again changes it. The columns count
what's shown; a filter that shows nothing says `Nothing matches`. The pick stays on its card while it's shown, else
goes to the first shown in its column, else the first shown; clearing keeps it. A task you change or add that no
longer matches stays, faint, until the filter changes, and the row above the footer says so (`No longer matches
+home.`). There's no calendar in the filter line: `by` and `late` aren't days. The filter isn't kept across
runs; `c` keeps it into the calendar and back, where every count is of what it shows.

#### With two-set (Dubeolsik) Hangul

The board reads two-set (Dubeolsik) Hangul as the keys it sits on: `ㅓ` is `j`, `ㅗ` is `h`, and a whole syllable
is its letters' keys in order, so `어` is `d` then `j`, `ㅊ` is `c` and `ㅆ` is `T`. The first time it does, the row above
the footer says so.
Where you type after `a`, `A` or `e`, Hangul is text. Two limits: with two-set (Dubeolsik) Hangul, Shift gives the
same letter for most keys, so `A` and `G` can't arrive (`End` does `G`'s work, and `Home` `g`'s; `A` has no other
key); and the input method may hold a letter until the next key. The arrows, `space`, `tab`, `enter`, `esc` and
`Delete` come through whatever input is on.

### The calendar

`c` on the board, or `halil cal`, opens a calendar of the tasks, and `c` again goes back: a month, each day saying how many tasks are due
that day and still open, and how many were done on it (`due 3 · done 12`). A count of 0 is left blank. It opens
on today, in blue; the picked day is on the picked row's ground. A due count is red once its day is gone.

| Key | |
|---|---|
| `←` `→` | a day back or on |
| `↑` `↓` | a week back or on (in a list, pick a task, as `k` `j`) |
| `PageUp` `PageDown` | a month back or on (31 Jan to 28 Feb) |
| `g` `G`, `Home` `End` | the month's first or last day (in a list, its first or last row) |
| `T` | back to today |
| `j` | from the grid, the picked day's first task; in a list, the next |
| `k` | in a list, the one before; from the first, back to the grid |
| `tab` | the grid, the picked day's list, the date-not-known row, round; a list with nothing in it is passed by |
| `a` | a new task, the picked day filled in after `//`, the cursor before it |
| `c` | back to the board |
| `esc` | clear the marks; else back to the board, or quit when it was opened with `halil cal` |

Under the grid, the picked day's tasks: `Due` (due that day and still open), `Done` (done that day) and `Added`
(added that day), each as the board draws a card, a sub item with its card's title after it where there's room. A
task in two of them, added and due today, is in each. The board's keys act on the task picked there as on the
board: `space`, `h` `l` `s` `t`, `d`, `e`, `x`, `y` and `A`, and with marks, on all the marked. `d` is done: on a
task done already it keeps it, and the day it was done. A move opens it again (`h` `l`, `s` or `t`, which the
footer shows where a done task is picked); a done sub item opens again with `d` on the board. A task a key takes
off the picked day (a new date, done or opened again) stays listed, faint, until the pick moves, and the row above
the footer says where it went (`6 is due Fri 13 Nov now.`).

The footer always has `c board`, the way back: on the grid right after the moves and `tab`, and in a list it's
the last key to give way but `d`.

Where a cell is narrow, its counts take two lines, `due 3` and `done 12`, and where even that doesn't fit (at 46
columns, always), `3` and `●12`, the line above the grid saying `due n · ● done n`. Every cell of the month says
it the same way. A number is never cut: one too wide
for its cell shows as `9999+`. A month takes 20 rows when its counts take a line, 26 when they take two; with fewer,
the calendar shows the picked day's week. It needs 46 columns and 10 rows.

One row under the grid, `Date not known: done 14 · added 3`, holds what's on no day: tasks done before 0.5.0,
which kept no done time, and tasks added before 0.3.0, which kept no time they were added. `tab` picks it, and its
list is `Done`, then `Added`, the board's keys acting on them as on any day's.

The day a task was done or added is the day it was where it happened, on the clock there: done at 8am in Seoul
on the 25th, it's on the 25th read in New York too, where it was the 24th; travel or a clock change never moves
it. A task added before 0.5.0 has its added day read from its stamp, in the zone halil runs in now.

## Where tasks are kept

`$XDG_DATA_HOME/halil/tasks.jsonl`, or `~/.local/share/halil/tasks.jsonl`: one JSON object a line, rewritten
whole on each save (written beside it, then renamed over it). Every change, from a command or the board, reads,
changes and writes it under an advisory lock on `tasks.jsonl.lock` beside it, so halil run several times at once
loses nothing: each sees the others' changes. (0.4.2 and earlier didn't lock, and the last to write won: 40 adds at
once kept a few.) The lock is the system's, let go of when a halil ends, however it ends; one that waits more than
5 seconds for another says `tasks.jsonl is in use by another halil — nothing changed` and exits with an error.
The board keeps the cards folded with `z` beside it, in `board.json`, by id and stamp, written under the same lock;
a missing or unreadable `board.json` means none are folded. A sub item has `"parent": <id>`; one whose parent
is gone shows as a task of its own. Each task has a `"stamp"`, the time it was added, which never changes: ids are
reused after a remove, so the stamp tells a task from one that took its id. A line written before 0.3.0 gets one
when it's next saved.

From 0.5.0, a task added has `"added"`, and a task done has `"done"`, each the time as the clock read where it
happened, with its offset and its zone's name when the system has one:
`"done":"2026-11-25T18:04:00+09:00[Asia/Seoul]"`. `"done"` is set when a task goes to Done and taken off when it
leaves; done again while done, it stays. A line where either doesn't read as such a time is refused, naming the
line. A repeating task has `"every"`, its repeat in words, a month or year one with the day it keeps (`"week"`,
`"2 weeks"`, `"month on the 31st"`), and a done one that repeated has `"next"`, the stamp of the task its repeat
went on to. halil 0.4.2 and earlier read a 0.5.0 file, but a change made with one of them writes the file without
`"done"`, `"added"`, `"every"` and `"next"`: those tasks show as done, date not known, their added day comes
from when they were stamped, and they no longer repeat. Don't run an older halil on a 0.5.0 file.

## License

MIT or Apache-2.0, at your choice.
