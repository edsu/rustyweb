# AGENTS.md

Guidance for AI agents (and humans) working on rustyweb. Keep this file current as
our conventions evolve.

rustyweb is a small, fast, **single-binary** web-archive server: it indexes WACZ
files into a Tantivy full-text index and replays them in the browser via
ReplayWeb.page / wabac.js. See [README.md](README.md) for user-facing docs and
[DESIGN.md](DESIGN.md) for the architecture (a **living document** — update it as
the design changes). [PRIMER.md](PRIMER.md) teaches Rust *through this codebase*.

## Working with the maintainer

- The maintainer (Ed) is newer to Rust: **teach the concepts, don't just hand over
  code.** Explain the "why," the trade-offs, and the Rust idioms in play. Prefer a
  recommendation with reasoning over an exhaustive menu.
- **Measure before optimizing.** When performance or behavior is in question,
  instrument and get real numbers rather than speculating (e.g. the per-phase
  `-v` timings in the indexer). A surprising result usually means a wrong mental
  model — chase it.

## Build & verify

Before committing, run the same checks CI enforces — the tree should be clean:

```sh
cargo fmt --all --check                                # rustfmt (formatting)
cargo clippy --workspace --all-targets -- -D warnings  # lints; warnings are errors
cargo build --workspace --locked
cargo test --workspace --locked   # unit (in-module #[cfg(test)]) + integration (tests/)
```

**CI:** GitHub Actions (`.github/workflows/ci.yml`) runs these four checks on every
push to `main` and every pull request, and the README shows a build badge. The
codebase is rustfmt-formatted and warning-clean — keep it that way so CI (and the
badge) stay green. If you add a dependency, commit the updated `Cargo.lock` (CI
builds `--locked`).

Fixtures for tests live in `crates/rustyweb-lib/tests/fixtures/`. Prefer adding a
focused test alongside the code it covers.

## Git & PR workflow

- **Commit or push only when asked.** If on `main`, branch first.
- Keep commits focused and logically scoped; write a clear imperative subject and a
  body explaining the *why*. One behavior change per commit where practical.
- End commit messages with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- End PR bodies with:
  `🤖 Generated with [Claude Code](https://claude.com/claude-code)`
- **Don't commit scratch/data files**: loose `*.wacz`, URL lists, ad-hoc scripts
  (e.g. `scripts/browsertrix-wacz-urls.py` stays uncommitted). `/target`,
  `/index`, `/archive`, `/a` are gitignored. Stage files explicitly rather than
  `git add -A` so scratch doesn't sneak in.
- Keep the PR description in sync with the work as it evolves.

## Issue tracking (beads)

We track work with **beads** (`br`, at `~/.local/bin/br`; SQLite-backed, exported
to `.beads/issues.jsonl` which *is* committed). Work is organized into epics with
numbered child tasks (e.g. `rustyweb-streaming-index-r0n.7`). Close tasks as their
work lands, and file follow-ups rather than expanding a PR's scope.

**Bundle tracker changes into ONE PR.** `br` auto-flushes the *entire* DB to
`.beads/issues.jsonl` on every write, so whichever branch commits the JSONL sweeps
in *all* pending tracker changes — not just "this branch's" issue. Splitting beads
changes across parallel PRs therefore doesn't work: the first to merge carries
everything, and the rest conflict on the single JSONL file (and are usually already
redundant). Do all the `br` operations for a batch, then make a single JSONL
commit/PR. The DB is the source of truth if things get out of sync (`br sync
--status`; `br sync --flush-only` exports DB → JSONL).

## Code review checklist

When reviewing a change (ours or a PR), look for:

- **Complexity that can be simplified.** Prefer the simplest code that does the job.
- **Complex-but-necessary code that lacks documentation.** If it has to be subtle,
  it needs comments explaining *why* (not what).
- **Redundant / duplicated code** that could be unified.
- **Test sufficiency.** Do unit *and* integration tests adequately cover the new
  code and its edge cases (error paths, fallbacks, boundaries)?
- **Docs that need updating** — `README.md`, `DESIGN.md`, `AGENTS.md`, `PRIMER.md`,
  CLI `--help`, or code comments.

Also worth checking:

- **Correctness & edge cases**: error handling, empty/boundary inputs, fallbacks.
- **Scope**: no accidental scope creep, and no scratch/data files committed.
- **Build hygiene**: `build`/`test`/`clippy` clean, no new warnings.
- **Spec adherence**: archive code should follow the WACZ/WARC/CDX specifics noted
  in DESIGN (e.g. Stored WARCs, multi-member gzip CDX, `.cdxj` variants).

## Conventions & ethos

- **Single binary, pure Rust.** Favor dependencies that compile into one
  self-contained binary (no runtime services, no C toolchain surprises). rustyweb
  aims to serve both small/local/private use *and* institutional scale from the
  same binary.
- **Keep the library UI- and dependency-free.** `rustyweb-lib` reports data and
  facts; `rustyweb-bin` owns user-facing concerns (CLI via `clap`, progress via
  `indicatif`, stdout gagging via `gag`). New UI deps belong in the binary. See the
  `IndexProgress` trait for the pattern (library emits counts; binary renders).
- **Comments explain *why*.** Match the surrounding code's style, naming, and
  comment density. Reference `file:line` when pointing at code.
- **Docs are living.** README is user-facing; DESIGN is the architecture of record;
  update both when behavior or design changes, in the same change.
- **Vendored ReplayWeb.page assets need manual refresh.** `static/replay/ui.js`
  and `sw.js` are committed, pinned copies of the `replaywebpage` npm release (see
  `scripts/fetch-replay.sh`). They are *not* a Cargo dependency, so Dependabot
  won't bump them - upgrade deliberately: bump `VERSION` in the script, re-run it,
  rebuild, re-test replay in a browser (`cargo test -p rustyweb-lib --test
  browser`), then commit the refreshed assets. Do this periodically to pick up
  wabac.js fixes (details in DESIGN → *ReplayWebPage Assets*).
