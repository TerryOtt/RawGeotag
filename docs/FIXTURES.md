# Verification fixtures

Real camera files, pinned, for the end-to-end checks that must write sidecars.

**Run `verify-fixtures.ps1` from the repo root:**

```
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1
```

It covers every supported format in one pass and exits non-zero on any failure.
[`TESTING.md`](TESTING.md) is the standard these serve — including the blind spot they
structurally cannot cover.

**Why `pwsh -File` and not the bare path — this is the one thing to know before your
first run.** The shell here is cmd, and cmd cannot execute a `.ps1`: it runs only what
is in `PATHEXT`, which does not list `.ps1`. It hands the file to its association
instead — Notepad, on this machine — so a bare `.\scripts\verify-fixtures.ps1` at a cmd
prompt **opens the script in an editor, prints nothing, and leaves `ERRORLEVEL` at 0**.
That zero means *the file opened*, not *the fixtures passed*, and it is the failure
worth remembering: the one command that proves output has not moved is the worst
possible one to skip silently.

*(`assoc .ps1` reports no association at all, which is misleading: the one in effect is
a per-user `UserChoice` under `HKCU:\...\Explorer\FileExts\.ps1`, and `assoc` reads only
the machine-level table. Verified 2026-08-02 — after `assoc` had already sent this very
note out with the wrong reason in it.)*

`-File` rather than `-Command` is what hands the script's exit code back, so
`cargo test && pwsh -NoProfile -File .\scripts\verify-fixtures.ps1` still short-
circuits; cmd understands `&&` too. `-NoProfile` keeps the run independent of a
PowerShell profile nobody here maintains.

**Inside a PowerShell session the bare path is correct** and the prefix is redundant —
that is the form Claude uses, whose shell tool is already pwsh. **The multi-line
recipes further down are PowerShell as well** (`Get-ChildItem`, `Get-FileHash`, and the
rebuild steps): type `pwsh` to drop into a session, paste, then `exit`.

## What lives where, and why it is split

| | Location | In git |
|---|---|---|
| Harness | `scripts/verify-fixtures.ps1` | **yes** |
| Source manifests | `scripts/fixture-manifests/*.sha256` | **yes** |
| Expected aggregates | in the harness, next to each fixture | **yes** |
| The raws themselves | `..\RawGeotag-fixtures\` (sibling of the repo) | **no** — 222 MB, and personal photographs |

Only the photographs stay out of version control. The script and the hashes are
project code: losing them would mean re-deriving every expected value, and a value
re-derived from whatever the code currently does is worthless as a regression check.

Because the location is *relative to the checkout*, *every* clone needs its own tree
beside it for the no-argument invocation to work — otherwise pass `-FixtureRoot`. The
copies are independent: **rebuild or correct one and the others are stale**, and the
same code will then produce different aggregates depending on which clone you ran in.
`-CheckSources` is what detects that, since it hashes the raws against the manifests
in git rather than against each other.

**If you are not the author, you do not have this tree — see *Bring your own raws*
below.** The short version is that you cannot have it, and do not need it.

## The three fixtures

| Directory | Files | Size | Exercises |
|---|---|---|---|
| `cr3-malta/` | 2 CR3, `_DOO0001`–`_DOO0002` | 87 MB | `Streaming`; EXIF offset `+00:00` — present, but a no-op |
| `cr3-rockies/` | 2 CR3, `_50A0001`–`_50A0002` | 88 MB | `Streaming`; EXIF offset **`+01:00` — real conversion** |
| `nef-sedona/` | 2 NEF, `DSC_0220`–`DSC_0221` | 43 MB | `WholeFile`; **no** EXIF offset, so `--utc-offset` is mandatory |
| `gpx/` | 3 tracks | 1.5 MB | the tracks covering those three shoots |

**Verify every format, every time.** `RawFormat::read_strategy` sends CR3 through
`Streaming` and NEF through `WholeFile` — different code in `raw.rs` — so one format
passing says nothing about the other.

**Why three and not two.** The timezone cases differ, and that matters more than the
file count. A bug that dropped the EXIF offset entirely would pass Malta (`+00:00`
is a no-op) *and* Sedona (no offset to drop), while corrupting every Rockies photo.

Measured, not hypothetical: `_50A0001.CR3` reads `15:02:05` with `+01:00`, i.e.
`14:02:05Z`, and tags at `51.352357, -116.088200`, matching the raw GPX. Read as
naive UTC it tags at `51.717543, -116.507579` — **49.9 km away** — and it *still
tags*, because `15:02:05Z` also falls inside the track. No error, no warning, no
skip. That is the project mantra's exact failure mode, and `cr3-rockies` is the only
fixture that catches it.

## Expected results

| Fixture | Tagged | Aggregate |
|---|---|---|
| `cr3-malta` | 2 / 2 | `CF2D1DA68FA359AA` |
| `cr3-rockies` | 2 / 2 | `047EF9B17BE64472` |
| `nef-sedona` | 2 / 2 | `F858DA7AA022AF2B` |

`nef-sedona` must additionally **refuse both and write nothing** when
`--utc-offset` is omitted — the D3300 records no EXIF timezone, so this set
exercises the gate for free.

The aggregate is SHA-256 over the concatenated per-file SHA-256s, name-sorted:

```powershell
$x = Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force | Sort-Object Name
$h = ($x | ForEach-Object { (Get-FileHash $_.FullName -Algorithm SHA256).Hash }) -join ""
(Get-FileHash -InputStream ([IO.MemoryStream]::new([Text.Encoding]::UTF8.GetBytes($h))) -Algorithm SHA256).Hash
```

**Two ways to get a false result.** Leftover `.xmp` files are skipped rather than
rewritten and change the aggregate — the harness clears them before and after each
fixture, so only hand-runs are exposed. And a deliberate change to `xmp.rs`'s packet,
or to the crate version in `x:xmptk`, moves all three hashes legitimately: re-derive
and update `verify-fixtures.ps1` and this file rather than hunting a regression.

**No code change has ever moved these** — not the `tempfile` change, the chrono
refactor, `--jobs 1/2/8/16`, nor the 2026-08-02 readability pass that reshaped the
phase structure, both outcome enums and the reporting order at once. That last one is
the case these hashes exist for: it touched everything the determinism check polices
and moved no output.

**The values above were re-derived on 2026-08-02 all the same**, when the sets were
trimmed from 40/30/30 files to two apiece. That is the *fixture* changing, not the
code — an aggregate is a hash over however many sidecars the set contains, so fewer
files necessarily means a different number. It is the second of the two legitimate
moves listed above, and the reason the list is there.

## Bring your own raws

The tree is not in git, not in LFS, and not published. This is not an oversight, and
it is not a barrier to contributing.

**The expected aggregates are not portable, so nobody else can use them anyway.** Each
one is a SHA-256 over the sidecars produced from *those exact bytes* with *that exact
track*. Different raws produce different sidecars produce a different aggregate. A
contributor working from their own photographs must derive their own numbers no matter
how the files are distributed, so shipping them would buy only the ability to
reproduce one person's specific figures.

**Most of the safety net is already in git.** The unit suite pins the emitted packet
byte for byte (`the_packet_is_exactly_this` in `xmp.rs`), the coordinate encoding, the
gap rules, the timezone policy and the gate. `cargo test` alone catches the large
majority of regressions. What the fixtures add on top is real-file parsing — the two
read strategies, and real EXIF from real bodies.

### What a good fixture set needs

Not many files. The count here is inherited from "the first N by name", not from any
requirement — **one photo per case would exercise every distinct path**, which is
about 220 MB rather than the 3.7 GB this once was:

| Case | Why it matters |
|---|---|
| a CR3, or any format read with `Streaming` | one of the two read strategies |
| a NEF, or any format read with `WholeFile` | the other, which fails differently |
| a body that records `OffsetTimeOriginal` **non-zero** | the only case that catches a dropped EXIF offset; `+00:00` is a no-op and proves nothing |
| a body that records **no** offset tag | makes the `--utc-offset` gate fire |
| a GPX track covering each shoot | otherwise every photo is *outside track* |

The third row is the one people will be tempted to skip, and it is the one that
matters most — see *Why three and not two* above for the 49.9 km it catches.

### Recording your own numbers

1. Put your sets under a directory of your choosing, one per case.
2. Point the harness at it:

   ```
   pwsh -NoProfile -File .\scripts\verify-fixtures.ps1 -FixtureRoot <path-to-your-tree>
   ```

3. It will fail, reporting the aggregates it actually got. Those are your baselines.
4. Put them in your copy of `verify-fixtures.ps1`, along with the file counts and a
   one-line note of what each set exercises.
5. Regenerate the manifests so `-CheckSources` works for you, one file per set under
   `scripts/fixture-manifests/`:

   ```powershell
   Get-ChildItem -LiteralPath <dir> -Filter *.EXT -Force | Get-FileHash -Algorithm SHA256
   ```

From then on the check does for you exactly what it does here: tells you whether
*your* change altered *your* output. That is the whole job. It was never able to tell
you whether your output matches someone else's — the unit tests do that.

**Do not commit your aggregates upstream.** They describe your photographs, not the
code, and would fail for everyone else.

## Rebuilding the fixture

Sources are on `Q:\`, which is read-only — copy out, never write back.

| Fixture | Source |
|---|---|
| `cr3-malta` | `Q:\Lightroom\Images\2025\2025-09-18`, first 2 CR3 by name |
| `cr3-rockies` | `Q:\Lightroom\Images\2022\2022-09-27`, first 2 CR3 by name |
| `nef-sedona` | `Q:\Lightroom\Images\2019\2019-01-19`, `DSC_0220`–`DSC_0221` |
| `gpx/malta-2025-09-18.gpx` | `Q:\Photo GPX Tracks\2025\2025-09 - Malta, Sorrento\2025-09-18 - Valletta City Walk.gpx` |
| `gpx/rockies-2022-09-27.gpx` | `Q:\Photo GPX Tracks\2022\2022-09 - Canada - BC, AB - Canadian Rockies\2022-09-27- Peyto Lake, Bow Lake, Yoho.gpx` |
| `gpx/sedona-2019-01-19.gpx` | `Q:\Photo GPX Tracks\2019\[2019-01-19 15h38m14s]Sedona - Sat afternoon.gpx` |

That last one's brackets need `-LiteralPath` in PowerShell.

**"First 40 by name" is how each set was originally chosen, not what defines it now.**
That selection returns the same 40 files only *by luck*: add one file to that folder
and it quietly means a different set, while the recorded aggregate still looks like it
is passing. What actually defines a fixture is its per-file manifest in
`scripts/fixture-manifests/` — which is why the rebuild is only finished once
`-CheckSources` agrees.

After rebuilding, run:

```
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1 -CheckSources
```

It validates every raw against `scripts/fixture-manifests/*.sha256` before running the
tool, so a mismatch tells you the *fixture* drifted rather than the code — which is the
one question a bare aggregate comparison cannot answer.
