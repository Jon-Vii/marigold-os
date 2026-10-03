# Issue tracker: Local Markdown

Issues and PRDs for this repo live as markdown files in `.scratch/`.

## Conventions

- One feature per directory: `.scratch/<feature-slug>/`
- A feature PRD, when one exists, is `.scratch/<feature-slug>/PRD.md`. So far, larger plans and PRDs have lived in `docs/plans/<date>-<slug>.md` instead (for example, `docs/plans/2026-09-12-reader-performance.md` accompanies `.scratch/reader-performance/`)
- Implementation issues are `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01`
- Triage state is recorded as a `Status:` line near the top of each issue file, under its title (see `triage-labels.md` for the role strings). Resolved issues use `Status: completed` and name the implementing commit(s) just below it
- Comments and conversation history append to the bottom of the file under a `## Comments` heading

## When a skill says "publish to the issue tracker"

Create a new file under `.scratch/<feature-slug>/` (creating the directory if needed).

## When a skill says "fetch the relevant ticket"

Read the file at the referenced path. The user will normally pass the path or the issue number directly.
