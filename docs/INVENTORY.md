# What is left to geotag

*Answering "which shoots still have untagged raws?" without walking 11 TB over SMB.*

## The command

```
pwsh -NoProfile -File .\scripts\archive-untagged.ps1
```

Sub-second, reads no share. It prints every directory holding CR3 or NEF files with
fewer `.xmp` sidecars than raws, split into those a GPX track covers and those it
does not, with the covering track's real UTC span under each.

It reads two manifests under `inventory\`. Build them once, and again whenever they go
stale — a new shoot imported, a new track added:

```
pwsh -NoProfile -File .\scripts\archive-inventory.ps1
```

That one *does* walk the NAS and takes minutes, which is the entire reason its output
is cached on disk rather than recomputed per question. It is read-only: nothing is
created, modified or removed on `Q:\`.

| File | One row per |
|---|---|
| `inventory\photo-dirs.csv` | directory under `Q:\Lightroom\Images` holding a raw file — CR3, NEF, DNG and XMP counts |
| `inventory\gpx-tracks.csv` | GPX file under `Q:\Photo GPX Tracks` — its true UTC span and point count |

**Both are gitignored, and that is not incidental.** They are a directory-level listing
of a private photo library — every shoot, its date and how many frames it holds — and
this repository is public. They were committed once, on 2026-08-03, and removed the
same day; the caching argument above is a good reason to keep them on disk and no
reason at all to publish them. A fresh clone therefore starts with no manifests and
must run `archive-inventory.ps1` first, which is the correct trade.

## What the report does and does not tell you

**The untagged column is an upper bound, not a forecast.** It is arithmetic on a
directory listing: raws minus sidecars. Whether any given photo earns a tag depends on
the bracketing track points being within `--max-gap` and `--max-distance` and inside
one `<trkseg>`, which only a real run can answer. Confirm a candidate with `--dry-run`
before assuming there is anything to gain — the gap between the two has been the whole
answer more than once, and both directions have shown up: 2,256 raws with a covering
track that were all inside one 6.7-hour hole in it, and a day whose only track ended
two hours before the first frame.

**A directory is matched to a track by its name, and the match is approximate on
purpose.** `Q:\Lightroom\Images\<year>\<date>` is the archive's convention, so the
report intersects each track's span with the day the folder is named for.

**That folder date is a *local* date; track times are UTC.** The report treats the
name as a UTC day anyway, because converting properly would need a timezone database
and the track's own coordinates. The error is bounded by the zone offset — up to ~12
hours — so a shoot near either end of its local day can appear to miss a track that
in fact covers it, or vice versa. **`-SlackHours 12` is the fix, and is worth reaching
for whenever a result looks wrong by exactly one day.** The report is a shortlist, not
an adjudicator; `--dry-run` settles it.

**DNG is counted but never geotaggable.** `rawgeotag` reads CR3 and NEF only, so a
folder can look short of sidecars because it holds Lightroom's HDR merges. The column
exists so the manifest does not quietly imply otherwise.

**A stray sidecar in an otherwise untagged folder is usually Lightroom's.** Several
folders hold exactly one, from a single photo edited years ago; `exiftool -XMPToolkit`
names the writer, and constraint 6 in [`../CLAUDE.md`](../CLAUDE.md) binds on the
answer. The default skip-existing behaviour already leaves them alone — this only
matters if someone reaches for `--force`.

## Why only `<trkpt>` times count

`archive-inventory.ps1` takes each track's span from the `<time>` inside its track
points and ignores every other one in the file. Pocket Earth writes a
`<metadata><time>` holding the *export* date: on one trip's tracks it sits five months
after the shoot, and taking it as the end of the span made every one of
them appear to cover the whole autumn — which in turn made unrelated shoots look
covered. Any future span extraction has the same trap waiting.
