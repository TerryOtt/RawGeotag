# What is left to geotag

*Answering "which shoots still have untagged raws?" without walking 11 TB over SMB.*

## The command

```
pwsh -NoProfile -File .\scripts\archive-untagged.ps1
```

Sub-second, reads no share. It prints every directory holding CR3 or NEF files with
fewer `.xmp` sidecars than raws, split into those a GPX track covers and those it
does not, with the covering track's real UTC span under each.

It reads two committed manifests. When they are stale — a new shoot imported, a new
track added — refresh them:

```
pwsh -NoProfile -File .\scripts\archive-inventory.ps1
```

That one *does* walk the NAS and takes minutes, which is the entire reason its output
is committed rather than recomputed. It is read-only: nothing is created, modified or
removed on `Q:\`.

| File | One row per |
|---|---|
| `inventory\photo-dirs.csv` | directory under `Q:\Lightroom\Images` holding a raw file — CR3, NEF, DNG and XMP counts |
| `inventory\gpx-tracks.csv` | GPX file under `Q:\Photo GPX Tracks` — its true UTC span and point count |

## What the report does and does not tell you

**The untagged column is an upper bound, not a forecast.** It is arithmetic on a
directory listing: raws minus sidecars. Whether any given photo earns a tag depends on
the bracketing track points being within `--max-gap` and `--max-distance` and inside
one `<trkseg>`, which only a real run can answer. Confirm a candidate with `--dry-run`
before assuming there is anything to gain — the gap between the two has been the whole
answer more than once, and both directions have shown up: 2,256 raws with a covering
track that were all inside one 6.7-hour hole in it, and a day whose only track ended
two hours before the first frame.

**A directory is matched to a track by its name.** `Q:\Lightroom\Images\<year>\<date>`
is the archive's convention, so the report intersects each track's span with the UTC
day the folder is named for. Terry's cameras are set to UTC, which is what makes that
sound — but a shoot running past midnight, or a body left on local time, can put frames
outside the day their folder names. `-SlackHours 12` widens the window on both sides
when chasing one.

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
`<metadata><time>` holding the *export* date: on the Canadian Rockies tracks it sits
five months after the shoot, and taking it as the end of the span made every one of
them appear to cover the whole autumn — which in turn made unrelated shoots look
covered. Any future span extraction has the same trap waiting.
