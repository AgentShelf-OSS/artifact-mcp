# 25. Tuned mimalloc as the global allocator

Date: 2026-11-15

## Status

Accepted

## Context

The release binary is a static `x86_64-unknown-linux-musl` build. Until this change it used
musl's default `malloc`. Issue #164 records a benchmark from 2026-11-09:

- Both servers ran on copies of the same production-sized dataset (343 artifacts).
- Each server was pinned to 2 CPUs, with 8 concurrent clients. The figures are medians of 3 rounds.
- Response bodies were byte-identical.

| Build | MCP CPU per call | HTML CPU per request | RSS idle | RSS high-water |
| --- | --- | --- | --- | --- |
| musl malloc (v1.32.0) | baseline: `list_artifacts` about 4× and `read_artifact` about 2× the Node v1.25.0 cost | baseline | 18 MiB | 50 MiB |
| mimalloc 0.1.52, defaults | about -50% | -10 to -25% | 156 MiB | 607 MiB |
| mimalloc 0.1.52, tuned (below) | about -45% | about unchanged | about 31 MiB | 80–107 MiB |

The cost comes from musl's allocator on allocation-heavy paths, mainly building MCP JSON
responses. A control build without the allocator change matched the published release, so the
build environment did not cause the difference.

mimalloc's default configuration trades memory for speed. Its first arena is committed eagerly,
and freed memory goes back to the OS only after a 1 s delay. For a small, long-running server, the
607 MiB high-water mark is worse than the Node implementation that ADR-0024 retired.

## Decision

The server binary uses `mimalloc` (crate 0.1.52, which builds mimalloc v3) as its
`#[global_allocator]`. The declaration is in `src/main.rs` only, so the library, the test binaries
and companion crates such as `ops/baby-log-voice` keep the system allocator.

The binary runs with two mimalloc options:

- `MIMALLOC_ARENA_EAGER_COMMIT=0` commits arena memory on demand, so a purge can decommit it.
- `MIMALLOC_PURGE_DELAY=0` returns freed memory to the OS immediately.

### How the options take effect

mimalloc reads its options from the environment when the allocator starts, before `main`. Two
facts rule out setting them from Rust code:

- The Rust runtime allocates before `main`, so the first arena already exists. mimalloc's
  `options.c` says that setting these options programmatically is too late.
- mimalloc v3 has a compile-time default macro for eager commit (`MI_DEFAULT_ARENA_EAGER_COMMIT`)
  but not for the purge delay, which is fixed at 1000 ms in the options table. `libmimalloc-sys`
  also passes no user C defines, so the macro could only be set through a global `CFLAGS`. That
  would affect every C dependency and still leave the purge delay unset.

So the first thing `main` does is call `startup::allocator::reexec_with_tuning`.

- If any tuning variable is missing, the process replaces itself, through `exec`, with the same
  executable and arguments, adding the missing variables and the marker
  `ARTIFACT_MCP_ALLOCATOR_TUNED=1`.
- The marker prevents a second pass.
- A value the operator set is kept, so the operator can still override either option.
- If the re-exec fails, the process logs the reason on stderr and continues with mimalloc's
  defaults.
- `std::os::unix::process::CommandExt::exec` is safe Rust, so the crate keeps
  `#![forbid(unsafe_code)]`.

This works the same under Docker, systemd and a manual start. It does not depend on an operator
remembering to set environment variables.

## Alternatives considered

- **Keep musl malloc.** This keeps the lowest memory use, but MCP calls stay 2–4× more costly.
- **mimalloc with its defaults.** This gives the best CPU, but uses 5–6× more memory at idle and at
  peak. That is not acceptable for a small server.
- **Environment variables in the Dockerfile and the systemd unit only.** This would work, but the
  production unit lives outside this repository, and a missing variable silently brings back the
  607 MiB high-water mark.
- **`mi_option_set` at the start of `main`.** This needs `unsafe` FFI, and it is too late for the
  first arena.
- **jemalloc.** This was not measured. It has a larger build footprint on musl, and mimalloc
  already meets the goal.

## Consequences

- MCP tool calls cost about half the CPU they did. Memory stays close to the musl build: about
  +13 MiB idle and +30–57 MiB at peak.
- Every start costs one extra `exec` of the binary, including the `healthcheck` subcommand.
- The process ID does not change across the re-exec, so a supervisor still sees the same process.
- mimalloc's C source is compiled into the binary. The Dockerfile's musl `build-base` toolchain
  already provides the C compiler.
- A future mimalloc upgrade must recheck the option names in its `options.c`, and should rerun the
  #164 benchmark harness.
