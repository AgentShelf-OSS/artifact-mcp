# Authoring readable artifacts

Listen reads visible document content in DOM order, including content before and after embedded
`main` and `article` elements. It never operates a tool, expands panels, or follows arbitrary links.
The existing supported ebook adapter still owns chapter order and continuation.

## Optional hints

- `data-artifact-reader-region`: when visible marked regions exist, read their contents in document order. Include the relevant heading inside the region. Nested regions are read once. The value `view` marks an interactive reading view without hiding the surrounding document. The value `exclude` excludes a subtree. Use `document` or an empty value for an explicit document root.
- `data-artifact-readable="false"`: exclude a subtree, including animated counters or decorations.
- `data-artifact-reader-block`: keep visible text inside a card/statistic together. Put label, value, unit, and qualifier in their intended spoken order. Native list items and definition lists are grouped automatically.
- `data-artifact-reader-summary="..."`: replace a visual or complex block with authored plain-text narration. The whole element is highlighted. This is not an instruction to a model.

```html
<main data-artifact-reader-region>
  <h1>Quarterly report</h1>
  <div data-artifact-reader-block>
    <span>Availability</span><strong>99.9 percent</strong><span>this month</span>
  </div>
  <figure data-artifact-reader-summary="Revenue increased in each quarter.">
    <svg aria-hidden="true"><!-- chart geometry --></svg>
    <figcaption>Quarterly revenue</figcaption>
  </figure>
  <span data-artifact-readable="false">Refreshing in 10 seconds</span>
</main>
```

## Reading scopes and details

The expanded Read selector offers Document / chapter, Current view, This section, selected text,
and reading from the current position. Click inside a tool before choosing Current view.
The reader uses the nearest visible `data-artifact-reader-region="view"`, `role="tabpanel"`, or
`role="application"` container; otherwise it chooses the first visible view or document root.
It reads only the visible state. The author or user operates tabs and filters, never the reader.

Click a table, row, code block, or visual with `data-artifact-reader-detail="..."` to reveal
Read details in the expanded player. This is a bounded reading action: it stops at the end of
that target. An authored detail attribute contains plain text, not HTML or a selector.
Give persistent views and detail targets unique HTML `id` values for reliable saved positions.

## Figures and tables

Figures, images, canvas, SVG, and elements with `role="img"` are atomic narration targets.
Description precedence is an explicit reader summary, visible `aria-describedby` references,
direct caption, then `aria-label`, image `alt`, or SVG `desc`. A chosen caption or referenced
explanation is not repeated within the same reading root. Decorative images with empty alt,
presentation roles, aria-hidden graphics, and unlabelled nested SVG icons are silent.
An undescribed standalone visual gets a brief unavailable-description notice. Listen does not
interpret chart geometry or verify that an authored summary accurately describes its data.

Simple tables with a complete first header row, at most 12 following rows, at most six columns,
and no merged cells read each row as column label/value pairs. Other tables read their caption
or a brief notice. Read details explicitly reads larger tables. Ambiguous headers get an announced
fallback to visible row order. Clicking a row and choosing Read details reads only that row.
Code blocks use a brief notice during continuous reading and literal text in detail mode.
Selection remains an alternative for any visible passage.

Authored summaries and generated table rows use whole-block highlighting. Matching prose and
explicitly selected text retain word highlighting. Clicking a figure, card, or row chooses the
corresponding semantic block for Read from here.

## Pronunciation hints

Use `data-artifact-pronounce` on a short inline term to provide its spoken form without changing
the displayed text. For example:

```html
<p>Ask <span data-artifact-pronounce="Shiv awn">Siobhan</span> about
  <span data-artifact-pronounce="sequel">SQL</span>.</p>
```

While the replacement is spoken, word highlighting covers the original term. Sentence replay
uses the same audio and mapping. Selecting only part of a term reads that selection literally;
selecting the complete term uses its pronunciation. Hints do not apply to code or synthetic
figure/table narration. Write the desired spoken form directly into an authored summary instead.
Changing a hint invalidates the affected reading snapshot and saved position.

Each original term and replacement is limited to 200 UTF-16 code units, with at most 128 hints
per block. Use plain text, not SSML, phoneme markup, or instructions to a model. Nested or
ambiguous hints may be ignored. Spellings are suggestions to Pocket, not guaranteed phonetics.

## Playback diagnostics

`window.artifactReaderDiagnostics()` in the viewer's browser console returns the latest 50
streaming measurements for that page. These include time to first scheduled audio, gaps between
chunks, late audio frames, and received audio duration. Scheduling times estimate playback;
they do not measure sound leaving the speakers. Pauses and manual navigation can affect gaps.
Measurements contain no narration text and are neither persisted nor sent to a telemetry service.
Reloading the page clears them.

## Dynamic content and limits

Hidden or aria-hidden content, navigation, form controls, editable content, tab controls, timers,
and logs are excluded. An explicit reader block on a `role="log"` element opts its visible text
back in. Closed details expose only their summary. Authors should mark unrelated counters with
`data-artifact-readable="false"`; arbitrary counters cannot reliably be inferred from styling.
Changes within the chosen view, section, or detail invalidate its reading snapshot. Updates outside
that scope and excluded timer updates do not. Hiding or removing the chosen view stops playback.
Saved positions distinguish reading scopes and validate their content fingerprint before restoring
an ordinal. Legacy saved positions retain the old fingerprint check and require a fresh start if
content no longer matches.

Rich merged-header associations, automated author-hint diagnostics, and new explicit bundle
sequences remain outside this v1. No runtime model or server-side parser was added.
