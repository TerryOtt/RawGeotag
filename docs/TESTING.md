# Testing standard

*Three standing orders share one diff: [`REVIEWING.md`](REVIEWING.md) for the code,
this file for the tests, [`WRITING.md`](WRITING.md) for the prose.*

## The standing order

> **Reach for every branch, and prove every test can fail.**

Two wordings get proposed for the first half, and both are wrong in opposite
directions:

- **"As many branches as possible"** invites tests written to move a number. This
  project has already produced one. A `collect_paths` test asserted that the result
  was sorted, on filenames the filesystem already returned in order — it passed, it
  looked like coverage, and deleting the sort it existed to guard changed nothing.
- **"As many as feasible"** is too soft the other way. It makes skipping a branch a
  question of effort, and the branches hardest to reach are precisely the ones
  nothing else covers.

So the order is two-sided. Reach for everything, and then hold each test to whether
it could fail if the code were wrong. **A branch left uncovered is acceptable; an
uncovered branch nobody noticed is not, and a test that cannot fail is worse than no
test, because it reads as coverage.**

Five rules follow from that.

1. **Coverage is the search strategy, not the goal.** Use it to find branches
   nothing exercises. Do not use it as a score.
2. **Mutation-check anything the compiler cannot see.** See *The bar* below.
3. **Prioritise branches no fixture can reach.** They are the dangerous ones,
   because everything stays green. See *Branches no fixture can reach*.
4. **Record what you leave uncovered, and why.** A known gap is a decision; an
   unknown one is a defect waiting. There is a list at the end of this file.
5. **Do not test what the compiler already guarantees.** Exhaustive `match` arms,
   type-level invariants and `#[non_exhaustive]` gaps need no test. Prefer making a
   mistake impossible over testing that it did not happen.

## The bar: a test that cannot fail is not a test

**Write the test, then break the thing it guards and confirm it fails — ideally that
it, and only it, fails. Revert immediately.**

A green test proves the code passes today. It does not prove the test would notice
if the code stopped being right, and the tests worth most here are the ones whose
subject is a silent behaviour change rather than a crash: a change that compiles,
passes everything else, and quietly alters which photos get tagged.

Cross-module agreements are where this pays, because nothing else checks them — a
CLI flag against the constant it mirrors, a `read_strategy` against the format it
describes, a summary total against the categories it sums.

**If a mutation produces no failure, the test is decorative. Fix it then, while you
still know what it was meant to catch.**

One worked example, because the failure mode is subtle. The `collect_paths` sort
test above used plainly alphabetical filenames, so `WalkDir` already yielded them in
sorted order. It now uses two names differing only in case: NTFS enumerates
case-insensitively while `PathBuf`'s `Ord` is byte-wise, so only an explicit sort can
produce the asserted order. **A test whose subject is an ordering has to be built on
inputs the underlying source does not already order for you**, or it measures the
filesystem rather than the code.

## Branches no fixture can reach

The fixtures are real camera files, which makes them the strongest evidence
available — and gives them a blind spot worth stating plainly:

> **A fixture suite cannot cover a branch that no supported input reaches.** The
> code is defensive about a case the current formats do not produce, so nothing on
> disk exercises it, and the whole suite stays green whether the branch works or not.

The case that established this: `exif_offset` handles both shapes nom-exif can
return, `Aware` and `Naive`. Every CR3 and NEF comes back `Naive` — JPEG is what
yields `Aware`, and this tool does not read JPEG. A version of the function that
ignored its `datetime` argument entirely passed **the entire suite as it then stood
and all three fixture aggregates**, measured rather than assumed. The branch was
load-bearing for the "EXIF wins" rule and held by nothing at all.

A deliberate sweep afterwards found four more, each confirmed by mutating it and
watching everything pass:

| Branch | Why no fixture reaches it | Now held by |
|---|---|---|
| the `CreateDate` / `OffsetTimeDigitized` fallback pair | every fixture file carries `DateTimeOriginal`, so the first tag always wins | `capture_tags_are_the_spec_pairs_in_priority_order` |
| `MediaSource` failing to open or parse | every fixture file is a valid raw | `a_file_that_is_not_a_raw_reports_an_error_naming_it`, `an_empty_file_is_an_error_rather_than_a_panic` |
| dropping GPX points with no `<time>` | the fixture tracks are fully timestamped | `points_without_a_timestamp_are_dropped` |
| the summary's skipped total and its breakdown | the harness compares sidecars, never the printed summary | `every_skip_category_is_both_counted_and_named` |

The last was also fixed structurally rather than only tested: the total and the
reason list were a five-term sum and five separate `if` blocks, so a new outcome had
to be remembered in two places. Both now derive from one array in `skip_breakdown`.
**Prefer that where it is available** — rule 5.

**Go looking for these deliberately.** They do not announce themselves, and the
signal that would normally prompt a test — something failing — never arrives.

## What to run

```
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1
```

**`--all-targets` is not optional** — without it clippy does not lint test code at
all, and there is more of that here than there is implementation. The release build
comes before the harness, which
runs `target\release\rawgeotag.exe` and throws if it is missing.

`cargo test` runs two kinds of test in one command: the unit suites inside each
module, and `tests/cli.rs`, which drives the built binary over synthetic input —
exit codes, refusals and early returns, the branches `run()` keeps to itself.
Anything that needs a real raw belongs to the fixture harness below, not there.

### The fixtures — every format, every time

`verify-fixtures.ps1` — invoked as in *What to run* above, since cmd cannot run a
`.ps1` directly — covers all three fixtures and exits non-zero on any failure.
[`FIXTURES.md`](FIXTURES.md) has what each one holds and why.

**Never run just one.** `read_strategy` sends CR3 through `Streaming` and NEF
through `WholeFile` — different code in `raw.rs` — so one passing says nothing about
the other. The three also differ by timezone case (`+00:00`, `+01:00`, none), and a
dropped EXIF offset would pass the first and third while misplacing the second by
~50 km, still tagging, with no warning.

**Every new format needs a fixture of its own.** NEF failed in a way no unit test
and no crate documentation would have revealed.

### Determinism under parallelism

The main regression risk the concurrency design introduces: a worker printing
directly, or a tally that depends on completion order.

**Re-run after any change to the phase structure, the outcome enums, the reporting
order, or the GPX load path.** Two comparisons, not one — across `--jobs` values,
**and against the previous run's artifacts**. The second matters because a reorder
that is consistent at every `-j` passes the first and fails only the second. The
three fixture aggregates *are* previous artifacts, so the harness covers that half
for free.

| When | After | Result |
|---|---|---|
| 2026-07-31 | first full verification | 3,883 CR3s at `-j 1/2/16`: identical console output, 1,489-line warning list, SHA-256 manifest over 2,394 sidecars |
| 2026-07-31 | gate selection extracted into a function | also reproduced the *pre-extraction* output byte for byte — the stronger result |
| 2026-08-02 | phase structure, both outcome enums and reporting order reshaped | `cr3-offset-nonzero` + `nef-no-offset` at `-j 1/2/16`: identical aggregates *and* identical `--verbose` stdout+stderr |
| 2026-08-02 | end of day, after `skip_breakdown` and the `exif_offset` extraction | same two sets, and the report hashes came back **identical to the pre-refactor run that morning** — a day of restructuring moved no output byte |
| 2026-08-02 | reporting reshaped: `print_ignored`, and column widths moved out of the `describe_*` helpers | `cr3-offset-nonzero` + `nef-no-offset` at `-j 1/2/16`: identical at every job count, aggregates unchanged. The `cr3-offset-nonzero` *report* hash moved and `nef-no-offset`'s did not, which is the fix landing exactly where expected — `nef-no-offset` runs all-UTC and prints no `Timezone` line to reformat |
| 2026-08-02 | GPX arguments may be directories (`collect_tracks`) | `cr3-offset-nonzero` + `nef-no-offset` at `-j 1/2/16`, tracks resolved from a *directory* argument: identical aggregates and reports. The aggregates match the single-named-file runs too, since a photo outside a track is skipped and extra tracks change nothing |
| 2026-08-02 | fixtures trimmed to two files per set | `cr3-offset-nonzero` + `nef-no-offset` at `-j 1/2/16` identical; the aggregates moved, as a smaller set must, but no code changed and the sets' distinguishing properties are untouched |
| 2026-08-02 | walk-error reporting hoisted ahead of the early returns | all three fixtures at `-j 1/2/16`: identical aggregates, matching the recorded values, and identical `--verbose` output |

That middle run is why the "diff against previous artifacts" rule exists. Extracting
a function that owns a `sort` is exactly what this check polices — and a careless
`sed` during that work also removed an unrelated sort in `collect_paths`, which
nothing else would have caught before release.

### Behaviour checks

Most are now unit tests, and two more are automated than this list used to admit —
`verify-fixtures.ps1` covers the gate *and*, since 2026-08-02, both dry-run
guarantees. What is left to confirm by hand after anything that touches the write
path: existing sidecars skipped with a warning; `--force` overwrites; a deliberately
wrong `--utc-offset` against a CR3 that has `OffsetTimeOriginal` warns *and still
uses the EXIF value*.

**The dry-run rehearsal check is a worked example of rule 2**, and worth reading
before writing a check of your own. It compares the sidecars' **last write time**,
not their contents, because a `--dry-run --force` that wrongly wrote would re-render
the same photo from the same track and lay down byte-identical files. The first
version compared aggregates, passed the mutation it existed to catch, and read as
coverage while guarding nothing.

### ExifTool as an independent oracle

Installed, and a different implementation — useful precisely because it shares no
code with this one.

- `exiftool -DateTimeOriginal -OffsetTimeOriginal <file>.cr3` should match what the
  tool extracted.
- `exiftool <file>.xmp` should read the GPS back; compare against the track for that
  timestamp. `-validate` should be OK.
- It calls the XMP `exif:GPSTimeStamp` property **`GPSDateTime`** — asking for
  `GPSTimeStamp` returns nothing, which is a naming difference, not a bug.
- **Test-time only.** No ExifTool call exists anywhere in shipped code, and none
  ever will — binding constraint 1.

### Scaling

Not a pass/fail check; see CLAUDE.md's *Measured behavior*. **Whether more threads
are faster depends on whether the read is real** — on a *warm* re-run local storage
peaks at `-j 2` and a slowdown at `-j 8` is the expected result, while a first
touch of the same files behaves like SMB and `-j 20` wins by several fold. (This
paragraph said "on local storage" unqualified until 2026-08-02, which is the same
warm-only reading that produced the old default of 2.) If you re-measure, evict
the page cache and delete existing sidecars first, or you are timing RAM and cheap
overwrites.

## The mutation log

Mutations already tried and what caught each. **Append to it; do not count it** — a
hand-maintained total is exactly the number that goes stale, and did.

| Mutation | Caught by |
|---|---|
| `choose_offset`'s `(None, None)` arm defaults to UTC instead of gating | `no_exif_zone_and_no_cli_offset_gates_the_run` |
| gap comparison `>` loosened to `>=` | `a_gap_exactly_at_the_time_limit_is_still_bridged` |
| `default_value_t = GapLimits::DEFAULT_GAP_SECONDS` replaced by a bare literal | `the_cli_gap_default_matches_the_shipped_limit` |
| `gate()` no longer sorts the zoneless paths it returns | `the_gate_reports_every_zoneless_file_in_sorted_order` |
| `parse_offset` reverts to stripping colons wherever they appear | `colons_are_only_accepted_between_the_hours_and_the_minutes` |
| the directory walk filters on a hardcoded format rather than the run's | `a_run_for_one_format_does_not_collect_another` — retired 2026-08-02 with the extension argument, whose removal made the mutation inexpressible; `each_format_alone_behaves_as_the_old_extension_argument_did` holds the per-format ground now |
| `tally_writes` collects per-file positions regardless of `--verbose` | `per_file_positions_are_collected_only_when_verbose` |
| `!settings.force` loses its `!`, so an ordinary run overwrites sidecars | `an_existing_sidecar_is_skipped_and_left_untouched_without_force` |
| `settings.dry_run` inverted, so `--dry-run` writes and a real run does not | `dry_run_reports_a_tag_but_creates_no_file` |
| `force` and `dry_run` swapped in `WriteSettings::from_args` | `write_settings_carry_the_flags_they_were_given` |
| `collect_paths` loses its `sort_unstable` | `collect_paths_finds_matching_files_recursively_and_sorts_them` |
| `exif_offset` ignores the `Aware` shape | `a_merged_aware_timestamp_supplies_its_own_offset`, `an_aware_timestamp_wins_over_a_paired_offset_tag` |
| `exif_offset` drops the paired-tag fallback | `a_naive_timestamp_falls_back_to_the_paired_offset_tag` |
| the `CreateDate` fallback removed from the tag table, or the two reordered | `capture_tags_are_the_spec_pairs_in_priority_order` |
| a non-raw file resolves silently instead of erroring | `a_file_that_is_not_a_raw_reports_an_error_naming_it` |
| untimed GPX points kept at the epoch | `points_without_a_timestamp_are_dropped` |
| the summary drops a category from its skipped total | `every_skip_category_is_both_counted_and_named` |
| `describe_offsets` calls any mix "two clocks in one run" | `more_than_two_clocks_are_reported_as_several` |
| the timezone line's file count loses its `plural()` | `two_non_utc_clocks_are_reported_as_a_mix` |
| `.gpx` dropped from `ignorable_extension`'s exclusions | `our_own_sidecars_and_tracks_are_not_counted_as_ignored` |
| the empty-tree return goes back to swallowing walk errors and exiting clean | `an_unreadable_subdirectory_fails_the_run_rather_than_exiting_clean` |
| `failed > 0` dropped from the exit-code decision, so a run with errored files exits clean | `a_file_that_is_not_a_raw_is_named_counted_and_fails_the_run` — **the whole unit suite passes this mutation**; the process test is the only thing holding it |
| `settings.dry_run` inverted, checked again now the harness has a say | `dry_run_reports_a_tag_but_creates_no_file`, and `verify-fixtures.ps1`'s dry-run check on all three fixtures |
| `if settings.dry_run` weakened to `&& !settings.force`, so the documented rehearsal writes | `dry_run_wins_over_force_on_an_existing_sidecar`, and the harness rehearsal check — **but only after it stopped comparing hashes**; see *Behaviour checks* |

## Known gaps

Recorded deliberately, per rule 4.

| Gap | Why it is left |
|---|---|
| `nom_exif::Error::ExifNotFound` → `Capture::NoCaptureTime` | Reaching it needs a file nom-exif recognises as media but that carries no EXIF. Empty and non-media files fail earlier at `MediaSource::open` — measured — which the two error tests do cover. Synthesising a valid-but-EXIF-less media file costs more than the branch is worth: it changes a per-file diagnostic, not which photos get tagged. |
| `run()`'s happy path and the gate, end to end | They need real camera files by design; `verify-fixtures.ps1` exercises both as a process. This row used to cover all of `run()` — until the 2026-08-02 walk-error bug landed in exactly that blind spot. The orchestration branches reachable *without* raws — the early returns, the refusals, the exit codes — are no longer a gap: `tests/cli.rs` drives the built binary over synthetic input via `CARGO_BIN_EXE_`, Cargo's own mechanism, with no dependency added. |
| `raw::capture_time`'s parse path | Needs real camera files by design. That is what the fixtures are, and why every new format needs one. |
