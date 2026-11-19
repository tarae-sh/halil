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
halil                 # the open tasks, what's due first
halil start 2         # to Doing
halil done 2
halil edit 1 next monday !med   # a new date and priority, the title kept
halil sub 1 transfer it friday  # a sub item under task 1
halil rm 1
halil board
```

`add` and `edit` read three things out of the words, and the rest is the title (`edit` keeps the old title when
no words are left):

- **A due date.** A day (`today`, `tomorrow`, `friday` or `fri`, `next friday`, `in 3 days`, `in a week`,
  `nov 20`, `2026-11-20`), a time (`9am`, `9:30pm`, `14:30`, `noon`, `eod`), or both (`in 2 hours`). A time on
  its own means the next time it comes. `at`, `by`, `on` or `due` before a date goes with it.
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
| `tab`, `←` `→` | go to another column |
| `a` | add a task, with the same words as `halil add` |
| `A` | add a sub item under the picked card (or the picked sub item's card) |
| `e` | change it, its title filled in, with the same words as `halil edit` |
| `d` | move it to Done, its open sub items with it; on a sub item, done or open again |
| `x`, `Delete` | remove it, a card with its sub items, after asking |
| `y`, `enter` | after `x`: remove; any other key keeps it |
| `y` | copy its title to the clipboard |
| `esc` | clear the marks, or quit when there are none |
| `q` | quit |

With marks, `h`, `l`, `s`, `t`, `d`, `e`, `x` and `y` act on all the marked at once, and `esc` clears them. `e`
then takes a date, `+tags`, `-tags` or a `!priority`, not a title; `y` copies each marked title, a line each, in
the board's order.

The picked row shows whole: when its title is cut or its tags or due date left out for room, it's shown on one
line over the columns to its right, as far as the board's edge. Only a row longer than that wraps, there (in Done,
under itself).

`y` copies with the platform's own command when there is one (`pbcopy` on macOS, `wl-copy` or `xclip` on Linux,
`clip.exe` under WSL), and else sends the title to the terminal's clipboard with its OSC 52 sequence; the row above
the footer says which, and whether the command failed. Not every terminal takes OSC 52, and some pass it on only
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
whole on each save (written beside it, then renamed over it). A sub item has `"parent": <id>`; one whose parent
is gone shows as a task of its own. Each task has a `"stamp"`, the time it was made, which never changes: ids are
reused after a remove, so the stamp tells a task from one that took its id. A line written before 0.3.0 gets one
when it's next saved.

## License

MIT or Apache-2.0, at your choice.
