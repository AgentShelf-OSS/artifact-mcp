# 14. Listen voice roles declared by artifacts

Date: 2026-10-14

## Status

Accepted

## Context

Listen narrates every block with the single voice selected in the player. Long-form artifacts,
especially ereaders, often have more than one speaker: a frame narrator, nested narrators,
letters written by other characters, and quoted dialogue. Readers asked for an audiobook-style
narrator in which each character can have a distinct voice that the listener chooses.

The artifact runs in a sandboxed iframe without network access. The viewer shell owns voice
selection, synthesis requests, and saved preferences. The injected reader bridge owns text
extraction and highlighting inside the artifact.

## Decision

An artifact may declare a **cast** of voice roles and tag readable content with a role. The
viewer shell lets the listener assign a voice to each role. Synthesis remains one request per
segment with a single voice; no new server route or model capability is needed.

- **Cast declaration.** A `<script type="application/json" id="artifact-reader-cast">` element
  contains `{"version":1,"default":"<role id>","roles":[{"id","label","description"?,"suggest"?}]}`.
  The bridge validates and sanitises it before sending it to the shell.
- **Role tagging.** `data-artifact-reader-voice="<role id>"` on any element assigns its readable
  text to that role. The nearest ancestor wins. An inline tagged element inside a block splits the
  block into consecutive segments, each with its own role and highlight mapping. Unknown or
  untagged text uses the cast's default role.
- **Player.** When a cast is present, the expanded player shows a Voices section with one voice
  picker per role, a per-role preview, and a "Character voices" switch. Defaults come from the
  artifact's `suggest` list when those voices are available, otherwise from a deterministic
  assignment of distinct available voices. The default role follows the main voice picker.
  Choices persist per artifact in the existing reader preferences.
- **Ereader headings.** The ereader adapter honours `data-artifact-reader-summary` on the chapter
  heading and reads visible `[data-artifact-reader-intro]` elements inside the heading after it,
  so artifacts can provide spoken chapter openings.

The artifact only declares roles and suggests voices. It never selects a voice directly, sends
model instructions, or learns which voices the listener chose.

## Consequences

- Narration can switch voices mid-paragraph without changing the speech API.
- Splitting blocks changes reading ordinals for tagged content, so saved positions taken before
  an artifact adds roles fail their fingerprint check and restart cleanly.
- Role switching adds one synthesis request per segment. Prefetch continues to request only the
  next segment.
- Voice-engine features such as Qwen reading style apply only when the segment's voice supports
  them.
