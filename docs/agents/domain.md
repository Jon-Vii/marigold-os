# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`docs/ARCHITECTURE.md`** for the current firmware shape, invariants, and domain vocabulary.
- **`docs/IMPLEMENTATION_PLAN.md`** for current phase status and verification commands.
- **`docs/CONTEXT.md`** if it exists.
- **`docs/adr/`** if it exists. Read ADRs that touch the area you're about to work in.

If any of these files don't exist, proceed silently. Don't flag their absence; don't suggest creating them upfront. Producer skills create them lazily when terms or decisions actually get resolved.

## File structure

This is a single-context repo:

```text
/
├── README.md
├── AGENTS.md
├── docs/
│   ├── ARCHITECTURE.md
│   ├── IMPLEMENTATION_PLAN.md
│   ├── CONTEXT.md   # optional, created when domain terms need pinning down
│   └── adr/         # optional, created when decisions need recording
└── <workspace crates>
```

## Use the repo vocabulary

When output names a domain concept, use the terms from `docs/ARCHITECTURE.md` and `docs/IMPLEMENTATION_PLAN.md`: reader state, display command, framebuffer, board I/O task, storage command, catalog snapshot, section cache, and so on. Don't drift to generic names when the repo already has precise ones.

If the concept you need isn't in the docs yet, that's a signal: either you're inventing language the project doesn't use, or there's a real gap worth resolving in conversation.

## Flag ADR conflicts

If output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (example decision) — but worth reopening because..._
