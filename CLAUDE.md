# RawGeotag — working notes for Claude

A Rust CLI that geotags camera raw files from one or more GPX tracks, writing XMP sidecars.

**Read [`docs/PLAN.md`](docs/PLAN.md) before proposing or writing anything.** The
design is settled there — CLI shape, crates, module layout, concurrency model. Do not
re-litigate decisions it already records; if you think one is wrong, say so explicitly
rather than quietly diverging.

**"Deep dive review" always means all four:** code, unit tests, code comments (in the
code *and* in the tests), and docs (`docs/` *and* this file). Terry has had to ask for
these separately and should not have to; changing any one is reason to check the other
three. [`docs/REVIEWING.md`](docs/REVIEWING.md) has the table of what stales what, and
why "the tests pass" is not a review of the tests.

**Read [`docs/REVIEWING.md`](docs/REVIEWING.md) before anything lands on `main`.**
Standing order: a branch can be as ugly as it needs to be, `main` has no broken
windows. There is no PR to hide behind here — the workflow commits straight to
`main`, so self-review at commit time *is* the gate, at the same bar. It carries a
table of the specific shapes a real review pass found in this codebase.

**Read [`docs/TESTING.md`](docs/TESTING.md) before adding, changing or removing a
test.** It is the standing order: reach for every branch, and prove every test can
fail.

**Read [`docs/WRITING.md`](docs/WRITING.md) before writing documentation or a
comment** — including edits to this file. Standing order: every document leads with
what its reader came for, and for the README that reader is the 98% case who wants
what it does and then a command that works.

## Project mantra

**"Geotags off by more than 5 m from actual are worse than no geotags."** A missing
tag is visibly missing; a wrong one looks authoritative and silently corrupts the
photo's provenance. So **a tag is a nice-to-have, earned only where the track
genuinely supports one** — never clamp, extrapolate, or bridge a hole to raise the
tagged count.

Say it that way round when it comes up. "Accuracy before coverage" is the same rule
stated as an abstraction, and it does not land — the concrete pair is *no tag* versus
*wrong tag*, and the answer is always no tag.

## Binding constraints

1. **Pure Rust only.** No ExifTool, no C-library bindings (`rexiv2`/gexiv2,
   `libexif`, `xmp_toolkit`, `libopenraw`). This is non-negotiable and applies to
   every dependency added, forever. ExifTool is installed on this machine and is
   useful as an independent oracle *when verifying output by hand* — it must never
   appear in shipped code.

2. **Optimize for wall-clock time.** The workload is embarrassingly parallel and is
   I/O-bound, not CPU-bound. **Match the thread count to the storage, not to core
   count** — "keep all cores busy" is actively wrong here, and `--jobs` defaults to
   2 for that reason (see *Measured behavior*). Note that *what* the storage limit
   is depends on the format: a `Streaming` format like CR3 is latency-bound and
   gains ~12x from threads over SMB, while a `WholeFile` format like NEF is
   bandwidth-bound and gains only ~2x. Do not introduce shared mutable state on the
   hot path, and never share a `MediaParser` across threads behind a mutex — use
   rayon's `map_init` for per-worker parsers.

3. **Readable and maintainable over clever.** Strive not to violate the principle
   of least surprise for an experienced Rust developer reviewing this codebase.
   Prefer the obvious mechanism to the clever one. Notably: format extensibility is
   an enum plus a data table, *not* a plugin registry or runtime module discovery —
   the plan explains why.

4. **Raw files are never modified.** Output is sidecars only.

Constraints 1-4 above bind the **code**. The two below bind **you, Claude, as an
operator of it** — they are not product requirements, and the distinction is the
whole point of them. Terry keeps every capability the tool has, including the
destructive ones. What is being removed is *your* ability to do irreversible harm,
not his. If data gets destroyed, the only person who should be able to have caused
it is him.

5. **On `Q:\`, you may read anything and create a new `.xmp`. Nothing else, ever.**
   You must never remove or overwrite a file there, for any reason — no exception,
   no "just this once", no flag that unlocks it. If a task appears to require one,
   the task is wrong: stop and ask. So **you never point `--force` at `Q:\`**,
   because overwriting an existing sidecar is exactly what it does.

   The atomic write is unaffected and stays: `tempfile` creates a temp file it owns
   and renames it into place, and cleaning up *its own* temp on failure is not what
   this prohibits. The rule is about files that were already there.

   **`--dry-run --force` does not unlock it either — asked and settled 2026-08-02.**
   The argument is sound on its face: a dry run provably writes nothing, so the
   combination cannot overwrite anything on `Q:\`. It is refused anyway, and *because*
   it is sound. The line is worth having precisely because it is unconditional, and a
   rule with one well-reasoned exception is a rule that gets a second one argued from
   the same "but this case is provably safe" shape. Rehearse a forced run on a staged
   copy under `N:\`, which costs nothing and needs no exception. (Terry keeps the
   combination, and it is documented in the README as the right way to preview
   `--force` — this constrains the operator, not the tool.)

6. **You never delete or modify a Lightroom-created sidecar, anywhere, ever.**
   Unlike constraint 5 this follows the *file*, not the drive — a copy staged on
   `N:\` is still a Lightroom sidecar, because a "fixed" copy invites being copied
   back over the original.

   **How to tell whose it is:** `exiftool -XMPToolkit <file>.xmp`. Lightroom's say
   `Adobe XMP Core ...`; ours say `rawgeotag <version>`, written as `x:xmptk` by
   `xmp::render`. That one field is the whole test; run it before anything that
   could write.

   Why it is absolute: those files carry develop settings, keywords, ratings and
   crop data that **exist nowhere else** — not derived from the raw, not
   regenerable. A missing geotag is recoverable; years of edits are not.

   Consequence to accept rather than engineer around: **a photo that already has a
   Lightroom sidecar does not get geotagged by you.** Say so and move on.

**Do not build these rules into the tool.** No drive-letter check, no `x:xmptk`
guard on `--force`, no confirmation prompt — `--force` stays exactly as destructive
as it is today. Terry runs it deliberately to do things you are not permitted to do,
and a safety rail added for your benefit would take that from him. The default
(skip existing with a warning) is load-bearing and should not change; that is a
different thing from adding a rail on top of it. If you find yourself proposing a
guard so you can be trusted with `--force`, the answer is that you are not supposed
to be trusted with it.

### The NAS guard — 5 and 6 are enforced, not merely written down

A **`PreToolUse` hook on `Bash|PowerShell`** refuses destructive commands aimed at
the shares. It exists because the protection used to depend on which tool you
reached for: PowerShell's built-in system-path guard blocked `Remove-Item` on `N:\`
while `rm -rf` through Bash sailed straight past.

| Target | Decision |
|---|---|
| `Q:\` | **deny** — hard block, no prompt |
| `N:\` | **ask** — Terry approves each one |
| anywhere else | untouched |

- **Script** `~/.claude/hooks/nas-guard.py`, **wired in** `~/.claude/settings.json`.
- **Both are user-global, not in this repo.** The guard follows the drives into
  every project, and it is *not* version-controlled here — a fresh machine, or
  anyone else's clone, has no guard at all. **Constraints 5 and 6 bind you whether
  or not the hook is running.** It is a backstop against a slip, never the reason
  the rules hold.
- **Deliberately unguarded: `cp` / `Copy-Item`.** Staging a working set from `Q:\`
  to `N:\` is the documented workflow and destroys nothing; guarding it would make
  the fast path prompt constantly for no gain.
- **A harness rule, which is exactly why it is permitted.** It lives in Claude
  Code's config, not in `rawgeotag` — the distinction the paragraph above draws.
  Constraining *the operator* is the point; constraining *the tool* would take
  capability away from Terry.

**Known limitation, deliberately not fixed.** It matches the whole command string,
so it cannot tell which path a verb applies to, and it cannot tell a command from
data that merely looks like one. Two consequences you will actually hit:

- A compound command is judged as a whole. Reading from the archive in the same
  line as an unrelated delete elsewhere is refused. Run them separately.
- **Writing *about* the guard trips the guard.** A heredoc is part of the command
  string, so a commit message describing a deletion on these drives is itself
  refused — which is how this very entry got blocked on its first commit. Put the
  message in a file and use `git commit -F <file>`; the `Write` tool is not matched
  by the hook.

Do not "improve" either of these by pairing verbs to arguments or by stripping
heredoc bodies before matching. Shell quoting, pipelines and splatting make the
first guesswork, and the second opens a real hole — `<<EOF ... EOF | sh` is a
command wearing data's clothing. **A guard that is subtly wrong is worse than one
that is bluntly right, and friction against Claude is not a reason to weaken it.**

To confirm it is still live, probe with a path that does not exist, so the command
is a no-op even if the hook is inert: `rm -f "/q/__probe__"` must be refused, and so
must `Remove-Item "Q:\__probe__"`. Both spellings matter — testing only one is how
the inconsistency went unnoticed in the first place.

## Execution shape: two phases with a gate between them

**The phase boundary is not where the progress bars suggest.** They read `reading
capture times` and `writing sidecars`, which invites the conclusion that phase one
geotags and phase two writes. It does not. Interpolation, XMP rendering, and the
write are all **fused in one worker task per photo**; the only step hoisted into its
own pass is the EXIF capture-time read.

| Step | Where | Parallel? | Does |
|---|---|---|---|
| Phase A | `main`, `par_iter().map_init(MediaParser::new, ..)` → `extract` | yes | EXIF capture time **only** |
| Gate | `main`, `gate()` consumes the extractions, looking for `ExtractionKind::NeedsOffset` | no | abort the run, or let it proceed |
| Phase B | `main`, `into_par_iter()` → `write_one` → `write_sidecar` | yes | `track.lookup` interpolate → `xmp::render` → `xmp::write_atomic` |

**Why phase A exists at all — the gate.** A capture time with no timezone and no
`--utc-offset` could misplace a photo by a whole day of travel. The tool refuses the
entire run in that case, which is only expressible if *every* capture time is known
before *any* sidecar is written. That requirement, and nothing else, forces the
barrier.

**Why it cannot be fused away.** The gate needs the capture time, and obtaining the
capture time *is* the expensive operation — parsing a raw file, which for a
`WholeFile` format means reading all ~22 MB of it. There is no cheap pre-scan that
validates timezones without doing the costly work, so the barrier cannot be moved
earlier or made cheaper. The expensive pass and the gate are inherently the same
pass.

**What the barrier costs.** The lost overlap between reading and writing, which is
bounded by whichever phase is shorter — so it is ~10% over SMB (Malta: 3.0 s of reads
against ~0.3 s of writes) and up to ~20% locally (Rockies: ~0.3 s of reads against
~1.2 s of writes). A fused single-pass design could recover part of that, and less
than it sounds, since sidecar writes do not parallelize on NTFS anyway (see *Measured
behavior* below). Not a good trade against the guarantee.

**Rejected, do not re-propose:** when `--utc-offset` *is* supplied no file can reach
`NeedsOffset`, so the gate is provably vacuous and the two phases could legally fuse
into one pass. This buys a few percent on one code path in exchange for two
structurally different execution models to reason about and test. Constraint 3 wins.

**Workers never print.** Each returns its diagnostics inside its outcome value
(`Extraction`, `Written`); `main` sorts by path and prints after the phase completes.
This is what makes output byte-identical at any `--jobs`. Do not add an `eprintln!`
inside a worker.

## Settled decisions worth not rediscovering

- Sidecar naming: `IMG_1234.CR3` → `IMG_1234.xmp` (Adobe convention, extension replaced).
- Timezone: EXIF `OffsetTimeOriginal` wins over `--utc-offset`; warn on conflict.
  Neither one present means the run is refused — that rule lives in `choose_offset`
  in `raw.rs` and is unit-tested branch by branch, including the refusal.
- **Formats are an enum plus a data table.** Each format declares which extensions
  select it, which capture tags to try, and — added when NEF arrived —
  `read_strategy`, whether the parser gets a streaming handle or the whole file.
  That third one is what a second format actually cost; see the NEF section for why
  it is not optional.
- **Numbers ≥ 1,000 carry US thousands separators — in program output *and* in
  prose here.** Any new user-facing number goes through `thousands()` in `main.rs`
  (or `count()`, its `usize` wrapper); do not `println!` a bare count. Summary
  columns are width 7 to fit the separators, so widen rather than drop them. Four
  things stay unseparated, deliberately: Rust numeric literals (which use `_`),
  text quoted verbatim from another tool so it stays greppable — the nom-exif
  `Incomplete(Size(169858))` error is the live example — and years, model numbers,
  UTC offsets and coordinate encodings, none of which are quantities.

  **Hand-written on purpose, and that is settled — do not reopen it.** The question
  that keeps coming back is "surely Rust has Python's `f"{n:,}"`". **It does not.**
  `std::fmt`'s spec has fill, align, width, precision and sign, and nothing for digit
  grouping; separators are a localization question and std does no localization. So
  `format!("{:>7}", 3883)` can only produce `   3883`, and there is no more Rustonic
  spelling waiting to be found. The alternatives were weighed and declined: the
  `thousands` and `num-format` crates are a dependency for twelve unit-tested lines
  with no locale surface, and a `Display` newtype — normally the right instinct under
  constraint 3 — **silently ignores `{:>7}`** unless it routes through `f.pad()`,
  which takes a `&str` and so allocates the very `String` it was meant to avoid.
  Every call site here is a width-7 summary column, so that trade is a straight loss.
  `count(n)` returning a `String` is the boring version that cannot get the padding
  wrong. If this comes up again, cite this paragraph and move on.
- Photos outside the track are skipped and reported — no clamping, no
  extrapolation, no tolerance window. With several GPX files that means outside the
  *union* of them; a photo landing in the seam between two files is a gap, not an
  outside-track case.
- **Gap rule (reverses the plan's original "no `--max-gap`" decision).**
  Interpolate only when the bracketing points are within **60 s AND 100 m**
  (`--max-gap`, `--max-distance`) *and* share a `<trkseg>`. Both limits are load-
  bearing: endpoint separation does not bound the error, since a subject can leave
  and return between two nearby fixes, so a 140 s / 8 m hole is still untrustworthy.
  Do not "simplify" this to a single condition.
- Existing sidecars are skipped with a warning; `--force` overwrites. No merging.
- **Multiple GPX files** are accepted (`<GPX>...`) and merged into one index, for a
  day split across several tracks. Two rules make that safe, and neither is
  optional:
  - **Segment numbering continues across files**, so a seam between two files is a
    segment break and is never interpolated across. Restarting the counter per file
    would make the last point of one and the first of the next look contiguous.
  - **Overlapping time ranges are a hard error**, checked while the track is built
    and therefore before any sidecar is written. Two tracks covering one instant can
    disagree, and the index keeps one point per timestamp — so the winner would be
    decided by argument order. A geotag decided by argument order is the mantra's
    exact failure mode. Inclusive bound: sharing one second is an overlap. The remedy
    is separate passes, which work because photos outside a track are skipped.

### Track lookup complexity — asked and answered, do not re-litigate

**The index is one flat `Vec<TrackPoint>`, sorted and deduplicated at load, searched
with `binary_search_by_key`. There is no list-of-lists and nothing linear on the hot
path.** Every GPX file is flattened into that single array — which is exactly what
the segment renumbering above exists to make safe.

The recurring idea is to record each track's min/max time so photos can be dismissed
without searching. **It is already implicit**: the array is sorted, so a photo outside
every track lands at `Err(i) if i == 0 || i == len`, and the binary search *is* the
bounds check. The proposal would turn `O(log N)` into `O(1)`. The prize:

| | |
|---|---|
| binary search over 76k points | ~17 comparisons, ~100-200 ns |
| all 3,883 lookups in a full run | **under 1 ms** |
| that run's wall clock | ~1,500 ms, dominated by file I/O |

**Lookup is ~0.05% of runtime.** Making it free saves under a millisecond.

The headroom is not close either, because `log2` growth means doubling the cost
requires *squaring* N. 76k points to 5.8 **billion** doubles the comparisons. A decade
of continuous 1 Hz logging is ~300M points — 28 steps — and at ~48 bytes per point
that is ~14 GB of RAM, so **memory breaks several orders of magnitude before the
algorithm does**.

**The one genuinely superlinear thing is `ensure_no_overlap`, at `O(F²)` in the number
of GPX *files*** — every pair compared. Seven files is 21 comparisons.

Sorting by start time and checking only adjacent pairs would make it `O(F log F)`, and
**that alternative is correct** — if any two intervals overlap then after sorting some
*adjacent* pair must, since `I[i+1].start <= I[j].start <= I[i].end`. Verified against
containment, nesting and touching-by-one-second; it agrees with the pairwise form on
all of them. It is simply **not worth doing**: `F` is bounded by what a human types on
a command line, so 21 comparisons never becomes a cost, and the pairwise form is
correct without needing that sortedness argument at all.

*(An earlier version of this note claimed the adjacency check would miss full
containment. That was wrong — it catches it. What it does not survive is dropping the
sort: `overlap_is_checked_across_every_pair_not_just_neighbours` uses spans whose
overlapping members are **not** adjacent in argument order, so a check over
input-order neighbours alone misses them. That is the mistake the test guards, and the
reason to leave this function alone is cost, not correctness.)*

**The general test, worth applying before the next such question: who controls N?**
Here `N_points` is bounded by how long someone logs GPS in a day and `N_files` by what
fits in a shell command. Neither can grow without the user deciding to make it grow,
which is what separates this from the hyperscale case where the instinct is right.

## Status

Implemented. Builds clean, the unit test suite passes, `cargo clippy --all-targets
-- -D warnings` is clean. **`--all-targets` is the form that matters** — without it
clippy does not lint test code at all, and there is more of that here than there is
implementation. (No count here on purpose —
it went stale three times; `cargo test` is the authoritative answer.) Toolchain on
this machine: Rust 1.97.1 MSVC, with the VS Build Tools C++ workload installed.

**Byte-level verification covers five shoots on two bodies** — four Canon EOS R5
(CR3) and one Nikon D3300 (NEF) — written up below. That is a narrower claim than
where the tool has been *used*: *Field patterns from real shoots* draws on several
more (NZ, Cabo, St. Lucia), which informed the gap-limit advice but were not
diffed against hand-computed positions. The NEF run is under *NEF, and why
`read_strategy` exists*; the four CR3 shoots follow.

*Malta 2025-09-17* (`Q:\Lightroom\Images\2025\2025-09-17`, 1,024 files, with
`2025-09-17 - Malta Car Tour.gpx`): 1,002 resolve and tag, ~3.0 s over SMB. The 22 skips
are **three distinct holes, not one** — 10 across a segment break (460 s / 594 m), 9 in
a 140 s / 8 m hole, 3 in a 775 s / 27 m hole. The 140 s / 8 m cluster is exactly what
the two-limit rule exists for: 8 m clears the distance limit easily and only the time
limit rejects it.

*Canadian Rockies 2022-09-27* (3,883 files, 188 GB, local NVMe, with `2022-09-27- Peyto
Lake, Bow Lake, Yoho.gpx`): 2,394 tag, 1,489 skip, 772 of those across `<trkseg>` breaks.
This body's clock was on **`+01:00`**, so unlike the 2025 trips it actually exercises
the EXIF offset conversion instead of a no-op. Spot-checked against the raw GPX on an
exact-hit photo: longitude and altitude identical to the track point.

*Jackson WY 2025-11-24* (726 files, SMB, `-j 16`): **726 of 726 tagged, nothing
skipped** — the cleanest run so far. The shoot (17:40–19:43 Z) sat wholly inside the
track (17:11–21:57 Z) with no dropout large enough to trip the gap rule. Position
matched the raw track point exactly.

*Malta/Sorrento multi-track, 2025-09* — the first real exercise of **multiple GPX
files**. All seven tracks in `Q:\Photo GPX Tracks\2025\2025-09 - Malta, Sorrento` were
passed to each of the four photo folders that exist (`09-17/18/19/21`), writing 1,967
new sidecars: 95/95, 297/297, 1,575/1,689, and 09-17 already done. Passing every track to
every folder is the safe idiom here — the tracks are disjoint in time, so each photo
matches whichever one covers it and **nothing depends on pairing a filename to a folder
name**. The 114 skips on 09-21 were 108 across a 2,122 s / 844 m segment break and 6 in
a 100 s / 189 m hole. Six sidecars spot-checked against the raw GPX agreed to **under
0.11 m**, all exact-timestamp hits (the logger samples at 1 Hz, so most photos land on
a recorded point and are never interpolated at all).

Across all five, interpolation agrees to within the coordinate encoding's resolution:
`xmp.rs` writes ten-thousandths of a minute, and 0.0001 minute of latitude is ~0.19 m,
so that is the floor on any agreement these checks can demonstrate.

## Verifying a change — run every format, every time

**`.\scripts\verify-fixtures.ps1`.** One command, all three fixtures (2 CR3 Malta,
2 CR3 Rockies, 2 NEF Sedona), seconds, non-zero exit on any failure.
[`docs/FIXTURES.md`](docs/FIXTURES.md) owns the rest — what each fixture exercises,
the expected aggregates, `-CheckSources`, and the rebuild recipe. **The expected
hashes are deliberately not repeated here**, so a legitimate packet change has two
places to update rather than three.

**Never run just one.** CR3 goes through `Streaming` and NEF through `WholeFile` —
different code in `raw.rs` — so one passing says nothing about the other, and the
three differ by timezone case besides. A bug that dropped the EXIF offset entirely
would pass Malta and Sedona and misplace a Rockies photo by **49.9 km**, still
tagging, with no error or warning.

**This holds even when you are asked for one format.** "Run the NEF verification"
means *verify* — run all three and say what was covered. The request is for
confidence that the tool works; a single-format pass does not supply it, and the
other two cost seconds. Narrowing scope here has been raised twice.

**And do not name one fixture as though it were the check.** Terry accepts the CLI's
tab-enter prompt suggestions, which are drawn from recent conversation — so writing
"the Sedona NEF verification" produces exactly that suggestion, he accepts it, and
the narrow framing re-enters the transcript. Call it **the full verification** or
`verify-fixtures.ps1`; name a fixture only when the subject really is that fixture.

**He has asked to be teased when he asks for one — do it.** His instruction: past-
Terry told you to make fun of today-Terry for falling into a brain trap he left
himself a note about. So run all three and land a light jab at the recursion, then
move on. **One line, not a bit**, and do not soften it into nothing: this was
requested rather than tolerated, and the joke is what makes the reminder stick.

### Two checks the fixture harness does not cover

**Determinism, after any change to the phase structure, the outcome enums, the
reporting order, or the GPX load path** (which is parallel too). Same input at
`--jobs 1`, `2` and `16` must produce byte-identical sidecars *and* identical warning
lists. Compare against the previous run's artifacts as well as across job counts — a
reorder that is consistent at every `-j` passes the cross-job check and fails only
the historical one. The fixture aggregates cover that half for free.

**Mutation-checking, when you add a test for an invariant the compiler cannot see.**
Break the thing it guards, confirm that test fails, revert. Green proves the code is
right today; it does not prove the test would notice if the code stopped being right.

**[`docs/TESTING.md`](docs/TESTING.md) is the standing order here** — the bar, the
doctrine on branches no fixture can reach (there are several, and they stay green
whether they work or not), the running mutation log, and the gaps left open on
purpose. Read it before adding or removing a test.

**Using ExifTool as an oracle:** it reads our sidecars back correctly and `-validate`
is OK. Note it calls the XMP `exif:GPSTimeStamp` property **`GPSDateTime`** — asking
for `GPSTimeStamp` on a sidecar returns nothing, which is a naming difference, not a
bug.

## The XMP we emit — measured against Lightroom's own, settled as of 15.4.1

> **Mantra: the latest Lightroom XMP format is our gold standard.** Lightroom is the
> consumer of everything this tool writes, so whatever current Lightroom emits is the
> definition of correct. We are not chasing the XMP spec, which is loose enough that
> conforming to it proves very little; we are chasing whatever Lightroom will read
> without complaint today.

**Closed on 2026-08-01 against Lightroom Classic 15.4.1 — closed against a *version*,
not for all time.** The only thing that reopens it is evidence that current Lightroom
emits or expects something different; on a major upgrade, re-run the exercise below
and follow whatever it does now.

**Watch both directions.** That Lightroom still *reads* ours is the requirement, but
it is a lagging signal — it fails only once we are already broken. What Lightroom
*emits* is the leading one, and following a material change in it is how we avoid
ever letting the gap widen into something that stops being accepted.
[`docs/LIGHTROOM-XMP.md`](docs/LIGHTROOM-XMP.md) has both checks, under ten minutes
together, on major Lightroom versions rather than dot releases. It also records that
Adobe's only automation surface is the Lua plugin SDK, so **"automate it with an API
instead" is not an alternative to the plugin already declined there — it is the same
proposal.** Not spec-purity, not tidiness, not more precision,
not a nicer-looking document, not a new crate that renders XMP — **a proposal resting
on any of those is answered by this sentence.** The evidence here is a same-photo,
same-track diff rather than a reading of the spec, so repeating it against 15.4.1 will
only produce these numbers again.

And even then only for a difference *in kind*. **Lightroom merely not writing a property
we write is not a change to follow.** It already omits `GPSMapDatum`, `GPSTimeStamp` and
`GPSAltitudeRef`, and has since 2019; all three are valid `exif:` properties that
ExifTool emits and Lightroom ingests without complaint. Deleting ours to match would
cost information and buy nothing. The mantra is about *compatibility, not mimicry* — the
goal is zero heartburn for Lightroom, not a byte-identical forgery. **Additive-and-valid
is not a difference worth closing; different-in-kind is.**

**The recipe is versioned: [`docs/LIGHTROOM-XMP.md`](docs/LIGHTROOM-XMP.md).** It has
the staging script, the Lightroom steps, the timezone trap per format, the questions to
ask, and the recorded answers for 15.4.1 and the two earlier eras. It is in the repo for
the same reason the fixture harness is: findings are worth nothing once the method that
produced them is gone — and the tree that produced these is already gone, since Terry
removed `N:\lr-xmp-compare\` on 2026-08-01. Rebuild from the recipe when there is a new
Lightroom to test. Lightroom's sidecars in such a tree are **Lightroom-created**, so
constraint 6 binds on them; `N:\` being disposable does not exempt them, and Claude does
not clear that tree — Terry does.

**The workflow this tool serves is: geotag first, import into Lightroom second.** That
ordering is not just convenience — it is the only window in which no Lightroom sidecar
exists yet, so constraint 6 never binds and no photo has to be declined because
Lightroom got there first. Terry runs **Lightroom Classic 15.4.1**.

The recurring question is whether our packet should look more like Lightroom's.
**Answer: it already does where it counts, and the remaining differences are safe.**
Compared against real LR sidecars on `Q:\` — `2019\2019-01-19\DSC_0001.xmp`
(`Adobe XMP Core 5.6-c140`) and `2023\2023-05-06\DSC_0218.xmp` plus
`2023\2023-09-10\3X8A0001.xmp` (`7.0-c000`, written by LR Classic 13.4):

| | Lightroom | rawgeotag |
|---|---|---|
| Packet wrapper | **none** — starts `<x:xmpmeta`, ends `</x:xmpmeta>\n`, no BOM | `<?xpacket …?>` both ends |
| GPS namespace | `xmlns:exif="http://ns.adobe.com/exif/1.0/"` | identical |
| Coordinates | `exif:GPSLatitude="32,53.9148203526N"` | same `DDD,MM.mmmk` form, 4 decimals not ~10 |
| `GPSVersionID` | `2.2.0.0` | identical |
| Altitude | `exif:GPSAltitude="32700/10000"`, **no** `GPSAltitudeRef` | `"123456/1000"` **plus** the ref |
| `GPSMapDatum`, `GPSTimeStamp` | absent | present |
| Serialization | attribute-form, one `rdf:Description` | identical |

**Lightroom's GPS flavor has not moved since at least 2019.** Namespaces were added
across those eras (`exifEX`, `crd`, `xmpDM`) and `x:xmptk` changed, but how GPS is
expressed did not. **Re-confirmed at 15.4.1** — 9 CR3 and 5 NEF geotagged in Lightroom
from the same tracks rawgeotag used, then diffed: every row above still holds, down to
`x:xmptk` being byte-identical to 13.4's. The one addition is
`photoshop:SidecarForExtension`, which it now writes for both formats. **So the
encoding our packet rests on is stable across 5.6-c140 → 7.0-c000 → 15.4.1, and
nothing needs changing.**

### Why Lightroom's fix differs from ours by half a metre — sub-second capture times

The same-photo, same-track diff agreed to **0.02-0.12 m on CR3 and 0.33-0.53 m on NEF**,
altitude to 0.245 m at worst and usually exactly. All of it is inside the mantra's 5 m
by an order of magnitude, but the NEF gap is bigger than our 0.19 m encoding floor
explains, and the cause is worth recording so nobody hunts it as a bug:

**Both cameras record `SubSecTimeOriginal`, Lightroom uses it, and we truncate to whole
seconds.** LR writes `exif:DateTimeOriginal="2025-09-18T06:52:03.43Z"`; we interpolate
at `:03`. The hypothesis predicts the data quantitatively — Malta's CR3s are `.43` on a
near-stationary camera and differ by ~0.02 m, while Sedona's NEFs are `.50`/`.80`/`.60`
on a walking photographer and differ by 0.33-0.53 m, implying 0.6-0.9 m/s. That is a
person walking slowly, which is what those frames are.

**Not worth adopting.** The whole effect is sub-metre against a 5 m rule, and buying it
would mean threading fractional seconds through capture-time extraction and lookup, and
re-deriving all three fixture hashes. Recorded as an explanation, not a TODO.

### Why the remaining differences stay

**Do not restyle the packet to match.** Every difference is additive or cosmetic and
none is malformed: the three properties we write and LR does not are all in Adobe's own
`exif:` namespace and are what ExifTool emits, and the `<?xpacket?>` wrapper is
likewise ExifTool's default, optional per spec, and read by LR constantly. Dropping it
to match LR byte-for-byte would make the output *less* conventional for every other
tool in order to erase a difference that is not a problem.

**Why adding LR-native fields buys nothing: our sidecar only has to survive one read.**
On import LR takes the GPS into the catalog, and the first time it writes metadata back
it replaces our file wholesale with its own — `crs:`, `xmpMM:History`,
`photoshop:EmbeddedXMPDigest`. Fields added to look more native do not outlive the
import, and each one is a new chance to be wrong.

**`x:xmptk="rawgeotag <version>"` stays honest, and that is not the risky part.** That
field's job is naming the writer. It is also the whole of constraint 6's test, so
forging it would disarm the one check that protects Lightroom sidecars.

**Two changes were floated before the 15.4.1 diff and both are now closed by it** —
recorded because they are the two a future session is most likely to re-invent:

- **More coordinate precision.** 4 decimal minutes quantizes to ~0.19 m, and going to 7
  looked like it would sharpen field spot-checks. The diff killed it: the sub-second
  truncation above moves the fix **2-3x further** than the encoding does, so spending
  three fixture hashes to refine the *smaller* of two errors buys a sharper number that
  is still dominated by the one left in place.
- **`photoshop:SidecarForExtension`.** Its only real use is disambiguating
  `IMG_1234.xmp` when `IMG_1234.CR3` and `IMG_1234.JPG` share a folder. 15.4.1 writes it
  itself, for both CR3 and NEF, on the first save — so we would be adding a field to be
  overwritten by an identical one minutes later.

## The CR3 timezone trap — do not regress this

nom-exif returns CR3 `DateTimeOriginal` as **`Naive`** and exposes
`OffsetTimeOriginal` as a **separate `Text` entry** (`"+00:00"` on the 2025 Malta
files, `"+01:00"` on the 2022 Rockies ones — see *Whose clock is it* below before
assuming what that variation means). It never merges them.
It *does* merge them for JPEG. So `ExifDateTime::aware()` is always `None` on CR3,
and any code that trusts `.aware()` alone will gate every single Canon raw as
"no timezone" — which is exactly what happened on the first real-data run.

`format.rs` therefore pairs each capture tag with its offset tag (`DateTimeOriginal`
with `OffsetTimeOriginal`, `CreateDate` with `OffsetTimeDigitized`) and `raw.rs`
prefers `.aware()` when present, falling back to the paired tag. **Test any new
format against a real file of that format**; JPEG stand-ins do not exercise this path.

NEF has since proved that rule twice over: it also returns `Naive`, it carries *no*
offset tag at all on a D3300, and it does not even parse through the source CR3 uses.
None of that was visible from the crate's documentation — only from real files.

Beware also that ExifTool reports CR3 `CreateDate` in *local machine time* from the
BMFF container, which differs from the EXIF `DateTimeOriginal`. Compare against
`DateTimeOriginal`, not `CreateDate`, when sanity-checking by hand.

### Whose clock is it — expect UTC, verify anyway

**Terry sets every camera to UTC deliberately. A non-zero offset is a slip, not a
decision.** The 2022 Rockies `+01:00` is London on BST — in his words, "sometimes I
do shit like set it to London time with daylight savings on, which screws things up
just enough to be annoying."

Two things follow, and they pull in opposite directions, which is why both are here.
**Operationally:** expect `+00:00`, but never rely on it — a wrong offset is exactly
the silent whole-shoot displacement the mantra exists to prevent, and it is worth
*mentioning* a non-zero offset to him rather than quietly honouring it, because it
may be a mistake he would want to know about. **The tool now does that mentioning
itself**: `describe_offsets` puts a `Timezone` line in the summary whenever a run
resolved through more than one zone, or through a single one that is not UTC. An
all-UTC run stays silent, so the line only appears when there is something to say.

**That is unconditional on purpose, and it is not noise to be gated behind a flag.**
Terry runs UTC on everything down to his wristwatch, so on his cameras a non-zero
offset is *always* a mistake and always worth surfacing. With a customer base of one
there is nothing to configure. If a second user ever objects, the shape is a
`--warn-non-utc` defaulting to on — **but do not build it speculatively**, and do not
quietly narrow the behaviour to "only warn on a mix" because a single-body Rockies
run prints a line every time. Printing that line every time is the feature. **For the code:** do not narrow
anything to match this habit. Other photographers legitimately shoot on local time,
so the rule in `choose_offset` — EXIF wins, `--utc-offset` fills in, neither refuses
the run — is right for both and should stay as it is.

## NEF, and why `read_strategy` exists

The plan assumed NEF would come free because NEF is TIFF-based and nom-exif reads
TIFF. Tested against **150 Nikon D3300 files** from three shoots, the answer was
more specific than yes or no:

| | result |
|---|---|
| `MediaSource::open` — the streaming source | **0 of 150 parse** |
| `MediaSource::from_memory` — whole file | **150 of 150**, matching ExifTool exactly |
| Files carrying `OffsetTimeOriginal` | **0 of 150** |

The streaming failure is `malformed ifd entry: parse ifd entry header failed:
Incomplete(Size(169858))` — quoted verbatim so it stays greppable, hence no
separator: that path reports a need-more-bytes condition as
malformed data rather than asking for more bytes, so the buffer never grows. In
memory mode every byte is already present, so it never arises. **Do not "simplify"
`read_strategy` away by using `from_memory` everywhere** — it would make every CR3
run read 30 MB per photo instead of a header, for no gain.

Three consequences, all load-bearing:

1. **`WholeFile` formats are bandwidth-bound, not latency-bound**, which changes
   what `-j` buys. Cold SMB, `--dry-run`, a different uncached folder per job count
   so nothing is served from cache: **129 MB/s at `-j 1`, 159 at `-j 2`, 256 at
   `-j 8`** (3.7 GB / 27 GB / 103 GB respectively — throughput is the comparable
   figure, not elapsed time). So threads still help, but roughly **2x**, against the
   **12x** CR3 gets from the same knob: there is far less latency to hide when each
   file is 22 MB of payload. Worth raising for a network NEF import, worth much less
   than the CR3 numbers below would lead you to expect.
2. **`--utc-offset` is mandatory for a D3300.** It writes no `OffsetTimeOriginal`,
   so every file reaches the `NeedsOffset` gate and the run aborts having written
   nothing. That is the gate working, not a bug. ExifTool does show a Nikon
   MakerNote `TimeZone` tag — it is a maker note, not the EXIF tag, and `format.rs`
   deliberately cannot pair against it.
3. **This camera's clock was on UTC.** Sedona 2019-01-19: naive EXIF 20:52 with
   `--utc-offset +0000` lands inside a track running 20:48:50-21:40:34 Z, and the
   resulting coordinates are in Sedona at 1,323 m. Do not generalize it — read the
   span and compare, exactly as with the R5 bodies.

**Verified end to end** (Sedona 2019-01-19, 30 NEFs against that day's track): 30
of 30 tagged; the run refuses everything without `--utc-offset`; an interpolated
position recomputed by hand from the raw GPX agreed to **under 5 cm** (31 s / 1.2 m
bracketing gap); ExifTool `-validate` OK; byte-identical output at `-j 1, 2, 8, 16`.

## Storage on this machine: `Q:\` and `N:\` are not interchangeable

Two NAS shares with opposite roles. Getting this wrong is either slow or damaging.

| | `Q:\` | `N:\` |
|---|---|---|
| Array | **HDD RAID6** — seek-bound, slow | **NVMe RAID10** — fast |
| Holds | `Q:\Lightroom\Images\<year>\<date>` and `Q:\Photo GPX Tracks\<year>` | nothing that matters |
| Rule | **read anything; create new `.xmp` only. Never delete, never overwrite** — binding constraint 5 | disposable, clobber freely — *except* `rawgeotag-bench`, see below |
| Capacity | 11 TB | 3.0 TB |

Free space is deliberately not recorded here — it changes on every run, no decision
in this file turns on it, and a stale figure is worse than none. Ask the filesystem
if it matters: `Get-PSDrive Q, N`.

**Stage a working set on `N:\` before running anything that writes.** That is not
optional — it is the only way to do trial runs, benchmark sweeps or `--force`
without touching the archive. Never point those at a folder under `Q:\Lightroom`.

The line to hold: a **real geotagging pass may create new sidecars on `Q:\`**, which
is the tool's whole purpose and the one write constraint 5 allows. Everything else —
trial runs, benchmark sweeps, `--force`, anything re-runnable — happens on `N:\` or
in a temp directory. If a photo on `Q:\` already has a sidecar, it stays as it is;
the answer is never to overwrite it.

### Staging for *speed* is a much narrower case than it looks

**The rule: staging pays only when the format reads whole files AND you will read
them more than once.** Both halves, or it loses. **NEF** satisfies both, so the
`-j` sweep amortised: `WholeFile` means the copy costs exactly one run's worth of
I/O, and the sweep re-read the set five times. The 103 GB NEF sweep against `Q:\`
took ~7 minutes of wall clock, which is what staging avoids.

**CR3 fails both halves, and it is not close.** Do not stage it for performance:

| | |
|---|---|
| staging moves | **188 GB** |
| the run actually reads | **~1.1 GB** — 169x less |
| stage 188 GB off `Q:\` | **13-25 min** |
| the run itself | **~13 s** at `-j 20` — so staging costs 58-115x the run |

The "~1.1 GB" is not a guess. 3,883 CR3s resolve in ~0.3 s locally, and 188 GB in
0.3 s would need **627 GB/s**; the fastest NVMe does ~7. nom-exif therefore touches
a slice of each file, not the file. Even if reads off `N:\` were *instantaneous*,
staging loses by two orders of magnitude — and repeated passes do not rescue it,
since five runs is ~65 s against a 13-minute copy.

**Beware the plausible-sounding argument for staging CR3, because it is half
right.** CR3 is latency-bound, and `N:\` genuinely has far better latency of the
kind that matters: both shares sit on the same NAS behind the same 5 Gbps link, so
SMB round-trip time is identical, but the arrays are not — HDD RAID6 seeks in
~5-10 ms against NVMe's ~0.1 ms, and a CR3 read is mostly seeks. The measured 25
files/s cold at `-j 1` is 40 ms per file, far more than LAN round trips explain, so
platter movement really is in there. Reads off `N:\` would be faster. **It still
loses**, because the copy has to move 169x the bytes the run will ever look at.
Partial-read plus single-pass beats any per-read speedup.

There is also a correctness reason not to: **the archive is where the photos live**,
so timing CR3 against NVMe measures a configuration that never occurs.

**Caching GPX tracks locally is worth nothing — measured, not assumed.** Reading a
4 MB track's span costs **174 ms from `Q:\` and 184 ms from local `C:\`**: identical
within noise, because the cost is `xml-rs` *parsing*, not I/O. This is the most
appealing wrong idea in this area, since tracks are small and re-read constantly.
The only thing that speeds track reading up is parsing fewer of them, or in
parallel — which `Track::load` already does.

**Keep the two rationales separate.** Staging CR3 for *safety* — because the work
writes, forces, or deletes — is right and required. Staging CR3 for *speed* is
wrong. Conflating them is how someone talks themselves into a 13-minute copy for a
performance benefit that does not exist.

**Benchmarking caveats.**

- **Do not overwrite the recorded throughput figures with `N:\` numbers.** Everything
  in *Measured behavior* below — CR3 25→296 files/s, NEF 129/159/256 MB/s — describes
  **HDD RAID6 over SMB**, which is the situation a user with a spinning NAS actually
  has. `N:\` results are a separate row, not a correction.
- **Stage distinct file sets per measurement anyway**, even though the check below
  found no caching: it costs nothing and the assumption could change with file size.
- **Do not verify file counts over SMB with `Get-ChildItem <dir>\*.ext`.** That form
  served a *stale directory enumeration* and reported 3 sidecars where 30 existed —
  `Get-Content` on one of the "missing" files read it fine at that very moment. It
  looks exactly like catastrophic data loss and is not. Use
  `Get-ChildItem -LiteralPath <dir> -Filter *.ext -Force`, and confirm with
  `Test-Path` before believing any count that suggests files vanished.

### The standing NEF fixture on `N:\` — reuse it, do not delete it

`N:\rawgeotag-bench\{j1,j2,j4,j8}` — **four disjoint sets of 200 Nikon D3300 NEFs,
~4.3 GB each, ~18 GB total.** Kept deliberately so NEF work never has to re-copy off
the slow array. Copied from `Q:\Lightroom\Images\2021\2021-05-19` (4,813 files), name-
sorted, slices 0-199 / 200-399 / 400-599 / 600-799. The directory names record which
job count each set was used for, so a repeat sweep can reuse the same pairing.

**It is a read benchmark, not a tagging fixture** — the opposite job from the `C:\`
fixture above, so do not reach for the wrong one. No GPX track exists for 2021-05-19
— the earliest 2021 track is 08-06 — so every photo in it reports *outside track*,
which is correct and is not a bug to chase. That is fine for timing the read phase,
which is the whole point.

| | `C:\...\RawGeotag-fixtures` | `N:\rawgeotag-bench` |
|---|---|---|
| For | correctness — byte-identical output | performance — read throughput |
| Size | 222 MB, 6 raws, 3 sets | ~18 GB, 800 files, 4 sets |
| Formats | CR3 **and** NEF | NEF only |
| Has a covering track | yes, all three sets | **no** |
| Right storage | local NVMe, fast and fixed | the NAS, which is what a real import reads |

**For any end-to-end verification, run `.\scripts\verify-fixtures.ps1`** — all
formats, recorded hashes, not this one.

Being D3300, every file needs `--utc-offset`; without it the run stops at the gate.
`--dry-run --utc-offset +0000` is the benchmark invocation.

**NEF read sweep, `--dry-run`. The two rows were measured differently — read the
note before comparing them to each other:**

| `-j` | 1 | 2 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| `N:\` NVMe RAID10 | 375 MB/s | 433 | 486 | **511** | 517 |
| `Q:\` HDD RAID6 | 129 MB/s | 159 | — | 256 | — |

- **`N:\` row:** 200 files (~4.3 GB) per run, from the four staged sets above. Note
  there are **four sets and five columns** — `-j 16` necessarily re-used one, so
  "a set per job count" is not true of this row as a whole. The methodology
  paragraph below is what covers that: three independent `-j 1` runs and a 1.6%
  spread between oldest- and newest-staged sets show re-reading did not put these
  files in cache, so the re-use does not flatter the `-j 16` figure.
- **`Q:\` row:** **not** the 200-file sets — a different uncached folder per job
  count, of **3.7 GB / 27 GB / 103 GB** respectively. Throughput is therefore the
  only comparable figure across it; elapsed time is not.

**The interesting part is that the faster array scales *worse*.** `N:\` is ~2.9x
`Q:\` single-threaded, but gains only **1.4x** from threads where `Q:\` gains ~2x —
because at 517 MB/s it is running at 83% of the **5 Gbps link** (~625 MB/s ceiling)
and there is nothing left to win. The bottleneck has moved off the disk and onto the
wire. Practical upshot: **`-j 4` already gets 94% of the best result on `N:\`**;
pushing higher buys ~6%.

*Methodology, since these numbers are only worth what the method is:* the 625 MB/s
line rate doubles as a cache detector — any result above it came from RAM. None did.
Three independent `-j 1` runs gave **362 / 368 / 375 MB/s**, and the oldest- and
newest-staged sets differed by 1.6%, so neither staging recency nor re-reading put
these files in cache. 4.3 GB reads over SMB are evidently not retained despite 63.7 GB
of RAM. That control is what makes the sweep trustworthy; re-run it before believing
a future sweep.

**Harness note.** Drive access is gated by Claude Code's directory permissions, not
just the OS. A fresh session or a subagent fork may be limited to the repo directory
and see both shares blocked, while `df -h` still lists them (`/q`, `/n`) because it
targets no path. `/add-dir N:\` grants it.

## Measured behavior worth not rediscovering

> **Canonical record.** `README.md`'s *Performance* section is a user-facing summary
> of these numbers and `docs/PLAN.md` deliberately carries none, so correcting a figure
> here means correcting README too — **and carrying the caveat across, not just the
> digit.** A stale caveat is what drifted last time, not a stale number.

**`--jobs` defaults to 2, deliberately, and that is not a typo.** The optimum depends
on the storage, and the two cases point in opposite directions:

| | read phase | best `-j` | why |
|---|---|---|---|
| **Local NVMe** | ~0.3 s / 3,883 CR3s — nearly free | **2** | run is write-bound; NTFS serializes creates *within one directory*, so threads contend — but see the sidecar-writes section: a recursive run spanning many folders lifts this |
| **SMB / network** | dominates the run | **16-20** | latency-bound; threads keep requests in flight |

**Everything in this section was measured on CR3 and holds for `Streaming` formats
only.** A `WholeFile` format moves the bottleneck from latency to bandwidth and the
advice changes with it — see the NEF section above before tuning `-j` for one.

Local NVMe, full workflow on 3,883 files creating 2,394 sidecars — `-j 2` measured
~1,470 ms warm and ~1,723 ms cold, against ~1,790-1,850 ms at `-j 20` and ~1,640-1,920 ms
at `-j 1`. `-j 2` won at min, median and max, warm *and* cold, so it is not noise.

Cold SMB read throughput, **CR3**: **25 files/s at `-j 1`, 107 at `-j 4`, 296 at
`-j 20`** — nearly **12x** from parallelism. Projected onto a 3,883-file day that is
~155 s single-threaded versus ~13 s. This is why the rayon design stays even though
the local case gains nothing from it: dropping threads would optimize the case that
is already under two seconds and wreck the case that takes minutes. (NEF over the
same link scales ~2x, not 12x — the files/s figures here do not transfer between
formats, because a CR3 "file read" is a few hundred KB and a NEF one is 22 MB.)

**Warm benchmarks lie here.** An earlier sweep with a cache warm-up showed reading
parallelizing only ~3x and plateauing near 4 threads; that measured RAM, not storage.
Always evict or use untouched data before quoting read-scaling numbers.

**GPX parsing is a serial-feeling cost that turned out to be worth parallelizing.**
Seven tracks of one trip (15.4 MB, 75,728 points) cost the better part of a second —
comparable to an entire local 3,883-file run — and all of it lands before a single
photo is touched. `Track::load` now parses the files with `par_iter`: **658 ms at `-j 1`,
390 at the `-j 2` default, 269 at `-j 4`, 215 at `-j 8`.** It scales with *file
count*, not total bytes — the floor is the largest single file, ~170 ms for a 4 MB
track. The slowness is `xml-rs`, which the `gpx` crate uses internally; quick-xml
would be far faster, but forking `gpx` to get it is not worth it.

This *softens* the `--jobs` reasoning above without overturning it. Threads now help
a phase the local-NVMe case previously had no use for them in, so a local run with
many tracks is a case where a higher `-j` is defensible. But the 175 ms between
`-j 2` and `-j 8` is smaller than the write contention a high `-j` costs on NTFS, so
the default stays 2. Everything after the parse runs in argument order — segment
ids, which of several bad files is reported, the overlap message — so none of this
is visible in output at any `-j`.

### Sidecar writes: the bottleneck is the directory, not the atomic write

> **STOP — a note from past-Terry to future-Terry, in his words:** reopening this is
> navel-gazing, bikeshedding, masturbating. **Stop trying to get cute.** He asked for
> it to be passed along in exactly those terms, so consider it passed along.
>
> The entire prize here is **~375 ms** on an operation that takes about four seconds
> at its worst. It has been analysed, then measured, then measured again. The design
> below is correct and the reasons are recorded. **There is nothing left to win.** If
> a future session finds itself proposing a faster write path, the answer is no —
> spend the effort on something that is not already fast enough.
>
> Read the rest of this section to understand *why* the current design is right, not
> as a starting point for improving it.

**Do not drop the atomic write to make writes parallelize. It is not what stops them
parallelizing.** Measured directly on local `C:\` NTFS — 2,000 sidecar-sized files,
min of 3 trials, `tempfile`+rename against a plain `File::create`+write:

| | `-j 1` | `-j 2` | `-j 4` | `-j 8` | `-j 16` | scaling |
|---|---|---|---|---|---|---|
| **atomic**, one directory | 2,878/s | 2,965/s | 2,543/s | 2,427/s | 2,506/s | **0.87x** |
| **direct**, one directory | 5,531/s | 5,104/s | 4,101/s | 4,250/s | 4,237/s | **0.77x** |
| **atomic**, 16 directories | 2,775/s | 3,868/s | 4,680/s | 5,123/s | 5,114/s | **1.84x** |

Three things fall out, and the second is the one that gets guessed wrong:

1. **NTFS takes an exclusive lock on a directory's B-tree index for any entry
   create, rename or delete.** One directory, one writer, no matter the thread count.
2. **A one-stage write does not help.** It scales *worse* (0.77x) — creation alone is
   what serializes, so removing the rename halves the work on a phase that stays
   single-file-at-a-time. It is 1.92x faster single-threaded, and that is the entire
   prize: on a real 2,394-sidecar single-folder run, ~810 ms against ~430 ms.
   **~375 ms is the whole price of atomicity**, and it buys protection against a
   Ctrl-C leaving a truncated sidecar that `skip existing` would then skip *forever*.
3. **Spreading across directories recovers nearly all of it.** Atomic writes over 16
   directories at `-j 8` hit 5,123/s — matching direct writes into one directory.

**So `-j` advice depends on the shape of the run, not just the storage.** A
single-folder import is write-serial and wants the default 2. A **recursive run over
many date folders parallelizes its writes** — and gets it free, because
`collect_paths` sorts, so rayon's chunked split hands each worker a different
directory. If a big multi-folder import ever feels slow, raise `-j` before
suspecting the atomic write.

Note also that *creating* a new sidecar costs ~2.3x *overwriting* an existing one, so
a `--force` re-run is not a valid benchmark of a fresh import; delete the `.xmp`
files first — **on a staged copy under `N:\`, never on `Q:\` and never a Lightroom
sidecar** (constraints 5 and 6; that is the whole reason staging exists).

**What the atomic write does and does not guarantee.** `persist()` does not fsync, so
it is complete protection against *process* death — Ctrl-C, panic, kill, where the
file is either fully written or never linked into place — and only partial protection
against *power loss*, since NTFS journals the metadata but not your data. Closing that
would need an fsync per file plus a directory fsync, costing far more than the rename
ever does. The current design is deliberately at that point: cheap cover for the
failure that actually happens, none for the one that mostly does not.

One deviation from the plan, deliberately: a file with **no EXIF at all** returns
`nom_exif::Error::ExifNotFound`, and `raw.rs` maps that to `Capture::NoCaptureTime`
rather than letting it become a hard error. Otherwise two indistinguishable
situations — no EXIF, versus EXIF without a date tag — would produce different exit
codes.

## Field patterns from real shoots

**Which gap limit binds depends on how the camera was moving.** Knowing which knob to
reach for saves a wasted pass:

| Platform | Binding limit | Evidence |
|---|---|---|
| Ship under way | **distance** | NZ cruise 2025-03-14: 541 photos rejected on distance alone, **zero** on time. Typical hole `83 s / 599 m` — the vessel covers ~600 m between fixes seconds apart. |
| Boat stationary or drifting | **time** | Cabo sunset cruise 2025-01-22: largest cluster `154 s / 74 m` — a 2½ minute hole in which the boat moved 74 m. |
| Walking or driving on land | neither | Valletta, Jackson, St. Lucia all reached full or near-full coverage on the defaults. |

So at sea raise `--max-distance`, at anchor raise `--max-gap`, on land change nothing.
`--max-distance 2000` took the NZ sailing pass from 74% to 88%; `--max-gap 200`
recovered 174 Cabo frames. Both were hand-checked against the raw track and agreed to
centimetres — but **be clear what that proves**. It proves the interpolation
*arithmetic* is right. It cannot prove the vessel held course between two fixes,
because there are no intermediate observations to check against. Only relax a limit
when the *other* limit shows the subject was barely moving, and say so when reporting.

**Multi-track workflow that works.**

- **Pass every disjoint track to every folder.** Nothing then depends on matching a GPX
  filename to a folder name — which is the one failure mode the tool cannot detect,
  since a wrongly-paired track just reports everything as outside-track.
  **A `<GPX>` argument may now be the trip's track *directory*** rather than seven
  enumerated paths, which is what this idiom was always asking for. Not recursive, so
  a trip folder stays one trip. `--verbose` lists what it resolved to before touching
  a photo — worth reading, given the filenames-lie entry below.
- **When one track overlaps others** — a multi-day cruise log spanning port days — the
  overlap check makes a single run impossible, by design. Run the *specific* tracks
  first, then the broad one **without `--force`**, so the more authoritative fix wins
  and the broad track only fills what is left.
- **GPX filenames lie.** Seen in the wild: a file named `2025-09-21` holding 09-24
  data, a `2015` typo for `2025`, and an en dash instead of a hyphen. Read the span,
  never the name: `rawgeotag --verbose --dry-run <empty-dir> <gpx>` prints it and
  exits before touching a photo.
- A track can be 8-9 MB on a single line. `grep -oE` over that on an SMB share is
  unusably slow — copy it local first, or parse it as XML.

**No cumulative sidecar tally lives here, deliberately.** It would change on every run,
tells a future session nothing about how to write or review code, and is exactly the
kind of hand-maintained number that went stale three times as a test count. The Status
section records verification of *correctness*; volume is not evidence of that.

## Dependency versions: check crates.io, never recall from memory

**The full process is [`docs/UPDATING.md`](docs/UPDATING.md)** — cadence, the
`cargo outdated` column reading, the verification sequence, and what to do when a
fixture hash moves after a bump (it is a regression, and the usual "re-derive the
hash" advice inverts). What follows here is only the part that has bitten twice.

The version numbers in [`docs/PLAN.md`](docs/PLAN.md)'s dependency table are
**indicative, not authoritative**. Before any of them lands in `Cargo.toml` — a new
dep, or a bump — confirm the current release with `cargo search <crate> --limit 1`.

This is not hypothetical. `indicatif` was pinned at `"0.17"` because that version was
familiar, not because it was current; 0.18 had already shipped six patch releases by
then. It sat stale until a human noticed.

What makes this bite is Cargo's `0.x` rule: for a pre-1.0 crate the **minor** is the
breaking-change position, so `"0.17"` means `>=0.17.0, <0.18.0` and *can never*
resolve to 0.18. `cargo update` respects that ceiling and reports "Locking 0
packages" while three minor releases behind — reassuring and wrong. **A clean `cargo
update` is not evidence of being current.** Only crates.io can tell you.

`1.x` deps are mostly self-correcting (`anyhow = "1"` keeps picking up 1.0.x), so the
risk concentrates in the `0.x` crates: `gpx`, `chrono`, `time`, `indicatif`.
