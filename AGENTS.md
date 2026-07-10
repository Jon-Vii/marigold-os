# CalendulaOS Agent Notes

## Agent skills

### Issue tracker

Issues and PRDs are tracked as local markdown under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

This repo uses the default mattpocock/skills triage vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repo: read the domain and architecture docs in `docs/`, plus `docs/adr/` if present. See `docs/agents/domain.md`.

### Cutting a release

Releases are tag-triggered and CI-built; `tools/prepare-release.sh <version>`
first syncs the crate version and the site's version/size labels so the
descriptor stamp and page don't lie. Never pre-create the GitHub release — the
workflow creates it, and Pages can't deploy without a populated release. See
`docs/agents/release.md`.

### Bench workflow

Development bench runs use `tools/bench/bench.py` and structured `bench:` serial
telemetry. See `docs/agents/bench.md`.

### Visual & Layout Changes Verification

Visual, layout, rendering, and typography changes are verified locally against the
emulator's golden frames, on both the X4 and the X3. Host-side cargo commands need
an explicit `--target`, and the reading-page goldens run in no CI job. See
`docs/agents/visual-verification.md`.
