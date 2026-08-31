# Prefer a private state machine over a wide helper

When chunking state includes the output list, id counter, heading path, pending text, “new content” marker and profile, passing them independently creates a wide helper and makes invariants easy to split across callers. A private `TextChunker`-style state machine gives callers a narrow vocabulary (`heading`, `line`, `finish`) while keeping overlap carry and final-flush rules together. Clippy's `too_many_arguments` warning was the useful symptom; state ownership was the actual design issue.
