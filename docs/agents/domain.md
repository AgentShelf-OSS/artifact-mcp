# Domain docs

This repository currently uses a single domain context. These rules explain how engineering skills should consume its domain documentation.

## Before exploring, read these

- Read `CONTEXT.md` at the repository root.
- If `CONTEXT-MAP.md` exists, use it to find the `CONTEXT.md` files relevant to the work.
- Read ADRs under `docs/adr/` that touch the work. In a future multi-context layout, also check `src/<context>/docs/adr/`.

If a file does not exist, proceed silently. Do not flag its absence or propose creating it before the work requires it. The domain-modeling workflow creates or updates domain documents when terms or decisions need to be recorded.

## File structure

The current single-context layout is:

```text
/
├── CONTEXT.md
├── docs/adr/
└── src/
```

A future multi-context layout uses `CONTEXT-MAP.md` at the root:

```text
/
├── CONTEXT-MAP.md
├── docs/adr/
└── src/
    ├── ordering/
    │   ├── CONTEXT.md
    │   └── docs/adr/
    └── billing/
        ├── CONTEXT.md
        └── docs/adr/
```

## Use the glossary vocabulary

Use terms as `CONTEXT.md` defines them in issue titles, proposals, hypotheses, and test names. Do not replace defined terms with synonyms.

If a needed concept is absent, reconsider whether the repository already uses another term. If the concept represents a real gap, record it through the domain-modeling workflow.

## Flag ADR conflicts

Report any conflict with an accepted ADR instead of silently overriding it. State which ADR conflicts with the proposal and why reopening that decision may be justified.
