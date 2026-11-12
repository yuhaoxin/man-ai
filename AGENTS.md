## Agent skills

### Output language

This is a public project. Everything the program prints to stdout or stderr MUST be in English: `--help` and usage text, usage errors, configuration warnings, diagnostics, header and tail lines, progress lines, and the five failure reasons. Language display names are printed in their English form. Documentation and code comments stay in Chinese per the global rules; tests assert the English strings exactly.

### Issue tracker

Issues and specs live as GitHub issues in `yuhaoxin/man-ai`, managed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Five default triage labels, each named after its role: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
