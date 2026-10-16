---
name: field-guide
description: Use before opening, planning or changing code in this repo. Prints one system's plain-English entry (what it does, how it works, key files, jobs, switches, rules, checks) from the Field Guide, or names the system that owns a file.
---

`docs/field-guide/guide.json` maps every system in this repo. Never Read it whole (it
is large); print one entry:

- `node docs/field-guide/guide.mjs`: every system, one line each
- `node docs/field-guide/guide.mjs <id>`: one system in full
- `node docs/field-guide/guide.mjs <path>`: the system that owns a file, and its entry
- `node docs/field-guide/guide.mjs --search <words>`: everything that mentions them (entries, flows, overview, glossary)

`node docs/field-guide/guide.mjs check` keeps every file on the map, not the prose.
When you change how a system works, re-read its entry and fix the sentences
that moved, in the same change.
