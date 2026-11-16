# Compatibility

Alforria targets OpenCode **1.18.31**, commit
`88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67`. The provenance of the frozen
reference material is recorded in [fixtures/PINNED.md](../fixtures/PINNED.md).

| Surface | Target and verification |
| --- | --- |
| v1/v2 API documents, routes, event type inventory | Compared against the live pinned source in CI (P0). |
| Wire DTOs and provider exchanges | Frozen schema fixtures and recorded provider replay tests run in ordinary Rust CI. |
| Structured output | Compared against both runtimes in CI (P8), and against a committed TS capture offline. |
| Other agent scenarios | Strict live tests are available explicitly. The complete matrix currently reports differences in repeated summary notifications, child-session permission policy, and the revert driver; full behavioral parity is not claimed. |
| Configuration | The pinned configuration contract plus alforria extensions. Names such as `opencode.json`, `.opencode/`, and `OPENCODE_*` remain unchanged. Newer upstream configurations have no general compatibility guarantee. |
| Storage | Alforria owns its database schema and application directories. Databases migrated by newer OpenCode v2 are explicitly refused; applications should not share a database. |

The default global directories are `~/.config/alforria`,
`~/.local/share/alforria`, and the platform equivalents. An existing legacy
`opencode` directory is used only until its alforria counterpart exists.
`alforria migrate` moves an install over; `alforria migrate --fresh` provides
an independent start when the legacy database is incompatible.

Alforria adds a local SQLite index for turn-specific assistant lookups on both
new and existing databases. It leaves the pinned table definitions and upstream
migration journal unchanged. Background summaries read only the requested turn
and its snapshot step parts, including when the turn predates compaction.

Undo snapshots own their objects independently of the source repository. Git
references retain new snapshot trees, and cleanup packs objects without
expiring recovery points. Older alternate-based stores are dissociated before
new captures. Already-pruned legacy objects cannot be reconstructed; recovery
reports an error without changing working files when required contents are
unavailable.

Recovery stages required contents before replacing any working file. Failed
replacements roll back the operation. If rollback itself fails, the error
identifies retained checkpoints containing the original files and target-path
manifests. Recovery refuses obstructing directories and symlinked parent paths
rather than removing unrelated work. This favors recoverability over an
automatic age-based disk limit.

File edits authorize both the requested name and its resolved target. Patch
moves reject symlink endpoints and distinct names for the same file, including
hard links. Ordinary writes replace only the authorized directory entry, so a
hard-linked name cannot overwrite a separately protected file. Tools recheck
resolved paths after permission waits before applying changes.

Linux CI runs the ordinary Rust and web suites. macOS and Windows CI run
focused recovery, permission, storage, history, and real PTY tests. Release
verification additionally runs the wider suite on all three systems, with
legacy POSIX-specific core tests still excluded on Windows. A Windows test
job is required to establish Windows behavior; a passing macOS run alone is
not that evidence.

Live parity comparisons are marked ignored by default, rather than being
reported as passed without execution. See the
[parity capture instructions](../crates/alforria/tests/parity/golden/README.md)
for exact commands. The manual backend benchmarks run with:

```sh
cargo test -p alforria-core benchmark_ --lib -- --ignored --nocapture
```

These measure constant active context with increasing archived history, a
scripted processor step including its completed background summary, and
bounded-result/no-match searches over a 32 MiB repository. The step benchmark
uses file-backed SQLite and a no-op snapshot service. These local timings do
not establish an end-to-end speed comparison with OpenCode.
