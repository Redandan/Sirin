Read and display the Sirin task log.

The task log is a JSONL file located at one of these paths (check both):
- `%LOCALAPPDATA%/Sirin/tracking/task.jsonl` → on Windows: `/c/Users/Redan/AppData/Local/Sirin/tracking/task.jsonl`
- `data/tracking/task.jsonl` (fallback)

Steps:
1. Read the last 30 lines of the task log file
2. Parse each JSON line and display a summary table with columns: timestamp, event, status, persona, message_preview
3. Group by status: PENDING / FOLLOWING / FOLLOWUP_NEEDED / DONE
4. Show counts per status
5. Highlight any entries older than 24 hours that are still PENDING

If the file doesn't exist, tell the user the app hasn't recorded any tasks yet (listener may not have started).
