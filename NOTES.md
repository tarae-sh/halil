# Notes

## Later

- Undo, for every change: the board's `x` removes for good, and so does every other key's change.
- A save that crashes leaves its `tasks.jsonl.<pid>.tmp` behind, and nothing clears it.

## Done in 0.3

- Ids are reused after a remove, so on the board a mark or an open input could follow an id to a new task. Mended
  by a stamp each task gets when it's made (`"stamp"`, milliseconds since 1970): the board holds id and stamp, and
  a change to a task whose stamp no longer matches is refused as gone. Ids are still reused.
