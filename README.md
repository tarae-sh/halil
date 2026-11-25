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

You need Rust 1.88 or newer.

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
halil board
```

`add` and `edit` read three things out of the words, and the rest is the title (`edit` keeps the old title when
no words are left, and the old date when none is given):

- **A due date.** A day (`today`, `tomorrow`, `friday` or `fri`, `next friday`, `in 3 days`, `in a week`,
  `nov 20`, `2026-11-20`), a time (`9am`, `9:30pm`, `14:30`, `noon`, `eod`), or both (`in 2 hours`). A time on
  its own means the next time it comes. `at`, `by`, `on` or `due` before a date goes with it.

  Or marked with `//`, as `+` marks a tag: `//fri 6pm`, `//nov 27`. The words after it, up to the next `+tag`,
  `!priority`, `-tag` or the end, must read as one date, or the task is refused (`` `//fridya` isn't a date``).
  With `//`, the other words are all title, date words too: `plan the friday party //wed` is due Wednesday.
  Without it, date words are read wherever they are, as ever.

  `//` with nothing after it takes the date off, in `edit` only: `halil edit 3 //`, or `halil edit 3 plan the
  friday party //` with the title kept whole. `add` and `sub` refuse it (`` `//` needs a date``).
- **Tags**, as `+home`. `edit` takes one off with `-home`, when the task has it. When it hasn't, `-home` among
  other words is a word of the title, and `-home` alone is refused (`3 has no tag home`).
- **A priority**, as `!high`, `!med` or `!low`.

The list shows due dates in words from now (`today 6pm`, `tomorrow`, `fri`, `in 3w`, `2d late`), red when late.
`NO_COLOR` turns colour off.

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
| `esc` | clear the marks, or quit when there are none |
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
then `t`, `x` and `s`; `d` goes last, and on a sub item it says `d done/open`. Room left over goes back to the keys
that gave way, the last to go first, any that doesn't fit skipped. With marks, the footer says how many are marked
and the keys for them. After `a`, `A` or `e`, it's where you type. The row above it says why a key did nothing, or
what else a change did.

Every change is saved at once. A `halil add` from another terminal while the board is open shows up on it, and a
task removed there while it's marked or being changed here is said to be gone, never mistaken for one that took
its number since.

#### With two-set (Dubeolsik) Hangul

The board reads two-set (Dubeolsik) Hangul as the keys it sits on: `ㅓ` is `j`, `ㅗ` is `h`, and a whole syllable
is its letters' keys in order, so `어` is `d` then `j`. The first time it does, the row above the footer says so.
Where you type after `a`, `A` or `e`, Hangul is text. Two limits: with two-set (Dubeolsik) Hangul, Shift gives the
same letter for most keys, so `A` and `G` can't arrive (`End` does `G`'s work, and `Home` `g`'s; `A` has no other
key); and the input method may hold a letter until the next key. The arrows, `space`, `tab`, `enter`, `esc` and
`Delete` come through whatever input is on.

## Where tasks are kept

`$XDG_DATA_HOME/halil/tasks.jsonl`, or `~/.local/share/halil/tasks.jsonl`: one JSON object a line, rewritten
whole on each save (written beside it, then renamed over it). The board keeps the cards folded with `z` beside
it, in `board.json`, by id and stamp; a missing or unreadable `board.json` means none are folded. A sub item has `"parent": <id>`; one whose parent
is gone shows as a task of its own. Each task has a `"stamp"`, the time it was made, which never changes: ids are
reused after a remove, so the stamp tells a task from one that took its id. A line written before 0.3.0 gets one
when it's next saved.

## License

MIT or Apache-2.0, at your choice.
