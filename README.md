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

You need Rust 1.85 or newer.

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

A **sub item** is a task under another task, one level deep. It takes the words of `add` and an id of its own,
so `done`, `start`, `edit` and `rm` work on it as on any task. The list shows a task's open sub items under it,
and how many are done after it (`2/4`). `halil done` on a task finishes its open sub items too, and `halil rm`
removes them. A task never finishes by itself when its last sub item does.


`add` and `edit` read three things out of the words, and the rest is the title (`edit` keeps the old title when
no words are left):

- **A due date.** A day (`today`, `tomorrow`, `friday` or `fri`, `next friday`, `in 3 days`, `in a week`,
  `nov 20`, `2026-11-20`), a time (`9am`, `9:30pm`, `14:30`, `noon`, `eod`), or both (`in 2 hours`). A time on
  its own means the next time it comes. `at`, `by`, `on` or `due` before a date goes with it.
- **Tags**, as `+home`. `edit` takes one off with `-home`, when the task has it (else `-home` is a word of
  the title).
- **A priority**, as `!high`, `!med` or `!low`.

The list shows due dates in words from now (`today 6pm`, `tomorrow`, `fri`, `in 3w`, `2d late`), red when late.
`NO_COLOR` turns colour off.

### The board

| Key | |
|---|---|
| `j` `k` | pick a card or a sub item, row by row |
| `space` | mark it, or unmark it, and step down: `h`, `l` and `d` then act on every marked card |
| `h` `l` | move it to the column left or right; a sub item moves with its card only |
| `tab`, `←` `→` | go to another column |
| `a` | add a task, with the same words as `halil add` |
| `A` | add a sub item under the picked card (or the picked sub item's card) |
| `e` | change it, with the same words as `halil edit`; with marks, a date, `+tags`, `-tags` or a `!priority` change them all |
| `d` | move it to Done, with its open sub items; on a sub item, mark it done or open again |
| `esc` | clear the marks, or quit when there are none |
| `q` | quit |

Sub items show under their card, `◦` open and `●` done, and the card counts them (`2/4`). In Done they're folded
into the count.

Every change is saved at once. A `halil add` from another terminal while the board is open shows up on it.

## Where tasks are kept

`$XDG_DATA_HOME/halil/tasks.jsonl`, or `~/.local/share/halil/tasks.jsonl`: one JSON object a line, rewritten
whole on each save (written beside it, then renamed over it). A sub item has `"parent": <id>`; one whose parent
is gone shows as a task of its own.

## License

MIT or Apache-2.0, at your choice.
