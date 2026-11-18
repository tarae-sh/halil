# Notes

## For 0.3

- Ids are reused after a remove (a new task takes the smallest free one), so on the board a mark or an open input can follow an id to a new task; the mend is not reusing ids, which touches the file's format.
- A save that crashes leaves its `tasks.jsonl.<pid>.tmp` behind, and nothing clears it.
