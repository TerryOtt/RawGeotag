# Verification fixtures

Real camera files, pinned, for the end-to-end checks that must write sidecars.

**Run `scripts\verify-fixtures.ps1`.** It covers every supported format in one pass
and exits non-zero on any failure.

## What lives where, and why it is split

| | Location | In git |
|---|---|---|
| Harness | `scripts/verify-fixtures.ps1` | **yes** |
| Source manifests | `scripts/fixture-manifests/*.sha256` | **yes** |
| Expected aggregates | in the harness, next to each fixture | **yes** |
| The raws themselves | `..\RawGeotag-fixtures\` (sibling of the repo) | **no** — 3.7 GB |

Only the photographs stay out of version control. The script and the hashes are
project code: losing them would mean re-deriving every expected value, and a value
re-derived from whatever the code currently does is worthless as a regression check.

Because the location is *relative to the checkout*, *every* clone needs its own tree
beside it for the no-argument invocation to work — otherwise pass `-FixtureRoot`. The
copies are independent: **rebuild or correct one and the others are stale**, and the
same code will then produce different aggregates depending on which clone you ran in.
`-CheckSources` is what detects that, since it hashes the raws against the manifests
in git rather than against each other.

## The three fixtures

| Directory | Files | Size | Exercises |
|---|---|---|---|
| `cr3-malta/` | 40 CR3, `_DOO0001`–`_DOO0040` | 1.74 GB | `Streaming`; EXIF offset `+00:00` — present, but a no-op |
| `cr3-rockies/` | 30 CR3, `_50A0001`–`_50A0030` | 1.30 GB | `Streaming`; EXIF offset **`+01:00` — real conversion** |
| `nef-sedona/` | 30 NEF, `DSC_0220`–`DSC_0249` | 0.63 GB | `WholeFile`; **no** EXIF offset, so `--utc-offset` is mandatory |
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
| `cr3-malta` | 40 / 40 | `C2277B569D9058B6` |
| `cr3-rockies` | 30 / 30 | `0D969878B1B7081C` |
| `nef-sedona` | 30 / 30 | `E7E243F581F1CA93` |

`nef-sedona` must additionally **refuse all 30 and write nothing** when
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

**All three aggregates have held across every refactor so far** — the `tempfile`
change, the chrono refactor, `--jobs 1/2/8/16`, and the 2026-08-02 readability pass
that reshaped the phase structure, both outcome enums and the reporting order at
once. That last one is the case these hashes exist for: it touched everything the
determinism check polices and moved no output.

## Rebuilding the fixture

Sources are on `Q:\`, which is read-only — copy out, never write back.

| Fixture | Source |
|---|---|
| `cr3-malta` | `Q:\Lightroom\Images\2025\2025-09-18`, first 40 CR3 by name |
| `cr3-rockies` | `Q:\Lightroom\Images\2022\2022-09-27`, first 30 CR3 by name |
| `nef-sedona` | `Q:\Lightroom\Images\2019\2019-01-19`, `DSC_0220`–`DSC_0249` |
| `gpx/malta-2025-09-18.gpx` | `Q:\Photo GPX Tracks\2025\2025-09 - Malta, Sorrento\2025-09-18 - Valletta City Walk.gpx` |
| `gpx/rockies-2022-09-27.gpx` | `Q:\Photo GPX Tracks\2022\2022-09 - Canada - BC, AB - Canadian Rockies\2022-09-27- Peyto Lake, Bow Lake, Yoho.gpx` |
| `gpx/sedona-2019-01-19.gpx` | `Q:\Photo GPX Tracks\2019\[2019-01-19 15h38m14s]Sedona - Sat afternoon.gpx` |

That last one's brackets need `-LiteralPath` in PowerShell.

After rebuilding, run `scripts\verify-fixtures.ps1 -CheckSources`. It validates every
raw against `scripts/fixture-manifests/*.sha256` before running the tool, so a
mismatch tells you the *fixture* drifted rather than the code — which is the one
question a bare aggregate comparison cannot answer.
