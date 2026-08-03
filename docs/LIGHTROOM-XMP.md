# Comparing our XMP against Lightroom's

Why this exists: **the latest Lightroom XMP format is our gold standard.** Lightroom
consumes everything this tool writes, so what current Lightroom emits defines correct.
The XMP spec is loose enough that conforming to it proves little; conforming to
Lightroom proves the thing we actually care about.

`CLAUDE.md`'s *The XMP we emit* section holds the conclusions and the decision. **This
file holds the procedure**, so the comparison can be re-run when Lightroom ships a new
version. It is versioned for the same reason the fixture harness is: the staged copy
lives on disposable storage and the recorded findings are worth nothing if the method
that produced them is gone.

## Check both directions

The requirement is that Lightroom **reads** ours. But reading is a *lagging*
indicator: by the time it fails you are already broken, and possibly broken in a
hotel room with a card reader and 2,000 photos waiting. What Lightroom **emits** is
the *leading* one. If its spelling of a geotag starts moving, follow it — so the gap
never widens into something that one day stops being accepted.

**Neither check substitutes for the other, and together they cost under ten minutes.**

| | Question | Tells you | Cost |
|---|---|---|---|
| **1** | Does Lightroom still read ours? | whether we are broken *now* | 2 min |
| **2** | How does Lightroom spell a geotag *now*? | whether the format is *moving* | ~5 min |
| **3** | Does its position agree with ours on the same photo and track? | whether our interpolation is right | ~1 hr |

Run **3** only when 2 shows movement, or when our own interpolation has changed and
positional agreement needs re-establishing — that is what it was built for, and it is
why it costs what it costs.

### When to run 1 and 2

**On a major Lightroom version — 15.x to 16.0 — or after any release whose notes
touch metadata, Map or XMP. Not on dot releases.** A format change lands in a feature
release; checking 15.4.1 against 15.4.2 is noise, and a check run out of ritual stops
being read carefully.

In practice that is roughly annual, and **the natural place for it is the pre-trip
routine that [`UPDATING.md`](UPDATING.md) already describes for dependencies** — same
reasoning, different trigger. You want to discover that Lightroom moved while you are
at home with the fixtures and the archive, not in a hotel room with a card reader.

*(An earlier draft said "every upgrade". That was an over-correction against the
opposite error of treating emission-watching as optional. Both checks being cheap is
what makes them worth doing; it is not a reason to do them for no cause.)*

**`scripts\lr-xmp-check.ps1` does everything either side of the Lightroom step.**
`-Stage` builds both folders and prints the clicks; `-Compare` reads what Lightroom
wrote and diffs it against the 15.4.1 baseline. The middle is irreducibly manual, for
the reason recorded at the bottom of this file.

```
pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Stage    # then checks 1 and 2 in Lightroom
pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Compare
```

(cmd cannot run a `.ps1` directly — it opens the file in Notepad and returns success.
[`FIXTURES.md`](FIXTURES.md) has the mechanism.)

`-Analyze <path>` prints the same facts for any single `.xmp`, which is how an
archived Lightroom sidecar can be read for comparison.

### 1. Does Lightroom still read ours?

Import `1-read-check` — the photo *with* our sidecar beside it — and look at the Map
module, or the GPS field in the Metadata panel.

If the pin lands where it should, current Lightroom ingests our packet and nothing is
broken. This works *because* of the hazard step 3 must avoid — Lightroom adopts GPS
from a sidecar present at import. What ruins the emission diff is exactly what makes
this check possible.

### 2. How does Lightroom spell a geotag now?

**You do not need a tracklog to read a format off Lightroom.** Import
`2-emission-check` — the same photo, with our sidecar held aside — set GPS by hand in
the Metadata panel, then **Metadata ▸ Save Metadata to File**, and run `-Compare`.

No tracklog, and none of the timezone trap below: both belong to the positional diff
in step 3, which is asking a different question and pays for it.

Answer *The questions to ask* further down, and compare against the table in
CLAUDE.md's *The XMP we emit*. Then apply the rule already recorded there: **follow a
difference in kind, ignore an additive one.** Lightroom writing a new property we do
not is not a reason to move; Lightroom writing *coordinates* differently is.

**Known limit of this shortcut, and the reason for it:** placing a photo by hand gives
Lightroom a latitude and longitude and *no elevation to write* — there is no source
for one. Lightroom is not dropping altitude; it never had any. Confirmed 2026-08-02.

So this check reads every row except altitude. **If altitude is what you need to see**
— the `/10000` rational, and whether `GPSAltitudeRef` has finally appeared — **it takes
a tracklog run**, because a GPX carries elevation and a dropped pin does not. That is
step 3, or at least its Lightroom half.

*(Manual placement being the one path that yields lat/lon with no altitude is recalled
from experience rather than re-tested here, and possibly from Flickr rather than
Lightroom. It does not change the instruction — a tracklog run is how you see that row
either way — but treat the mechanism as likely rather than established.)*

### 3. The full same-photo, same-track diff

**Lightroom and rawgeotag must tag the same photo from the same track.** Then any
difference is purely encoding. Without that control, a position difference could be the
two tools disagreeing about *where the camera was* rather than about *how to write it
down*, and those are not remotely the same finding.

**Lightroom must not see our sidecars.** If ours are present at import, Lightroom adopts
our GPS and re-emits it, so the comparison measures our own output round-tripped through
Lightroom. Hold them aside in a separate directory.

## Rebuild the staging set

Sources are the pinned fixtures in `..\RawGeotag-fixtures\` (see `FIXTURES.md`), which
already pair raws with a covering track.

**Run this from the repo root** — `$src` and `$rg` are relative to it, so it works from
whichever checkout you are in. Unlike the script calls above, this block *is*
PowerShell rather than a one-shot invocation of one: from cmd, type `pwsh` to open a
session, paste, then `exit`.

```powershell
$src  = "..\RawGeotag-fixtures"
$root = "N:\lr-xmp-compare"
$rg   = ".\target\release\rawgeotag.exe"

foreach ($p in @("$root\cr3-offset-utc","$root\nef-no-offset",
                 "$root\rawgeotag-reference\cr3-offset-utc","$root\rawgeotag-reference\nef-no-offset")) {
    New-Item -ItemType Directory -Force -Path $p | Out-Null
}

# Everything in each set. The fixture is two photos per set now, so there is nothing
# to select between -- an earlier version of this script indexed @(0,1,2,3,4,14,23,
# 31,39) into a forty-file set to spread the sample, since consecutive frames
# are often one stationary position. If you want a wider positional spread than two
# frames gives, pull extra photos straight from Q:\ rather than growing the fixture.
Get-ChildItem -LiteralPath "$src\cr3-offset-utc"  -Filter *.CR3 -Force | Copy-Item -Destination "$root\cr3-offset-utc"
Get-ChildItem -LiteralPath "$src\nef-no-offset" -Filter *.NEF -Force | Copy-Item -Destination "$root\nef-no-offset"

Copy-Item "$src\gpx\cr3-offset-utc.gpx"  "$root\cr3-offset-utc"
Copy-Item "$src\gpx\nef-no-offset.gpx" "$root\nef-no-offset"

# Our reference sidecars, then moved out of the import folders. (No extension
# argument: it was removed 2026-08-02, and these lines carried a stale `cr3` /
# `nef` between DIR and the track until a review caught them failing.)
& $rg --no-progress "$root\cr3-offset-utc" "$root\cr3-offset-utc\cr3-offset-utc.gpx"
& $rg --no-progress --utc-offset +0000 "$root\nef-no-offset" "$root\nef-no-offset\nef-no-offset.gpx"
foreach ($s in @("cr3-offset-utc","nef-no-offset")) {
    Get-ChildItem -LiteralPath "$root\$s" -Filter *.xmp -Force |
        Move-Item -Destination "$root\rawgeotag-reference\$s" -Force
}
```

**Verify no `.xmp` remains in either import folder before importing.** That is the step
the whole comparison depends on.

Staging on `N:\` is mandatory, not convenience — this writes sidecars, and constraint 5
forbids trial runs against `Q:\`.

## In Lightroom

1. Import each folder with **Add**.
2. Map module → **Map ▸ Tracklog ▸ Load Tracklog…** → the `.gpx` in that folder.
3. Select all → **Map ▸ Tracklog ▸ Auto-Tag Selected Photos**.
4. **Metadata ▸ Save Metadata to File** (Ctrl+S). Without this Lightroom keeps the GPS
   in the catalog and never writes a sidecar.

### Time zone — Lightroom needs an offset for *every* photo, including CR3

**This is the step that wastes an afternoon if you assume otherwise.** Lightroom's
*Set Time Zone Offset…* has to be set for both sets. It is **not** like rawgeotag, which
reads `OffsetTimeOriginal` from the CR3 and needs `--utc-offset` only for the D3300 that
lacks the tag. Lightroom's tracklog matching ignores the EXIF offset and works from an
offset you supply, so a CR3 with `OffsetTimeOriginal="+00:00"` still needs one.

**The offset is not a fixed number — it changes per photo set.** Measured on a machine
in **US Eastern**, where both cameras' clocks were on UTC:

| Set | Photo taken | Zone then | Offset that worked |
|---|---|---|---|
| `cr3-offset-utc` | a September day | EDT, `UTC-4` | **+0400** |
| `nef-no-offset` | a January day | EST, `UTC-5` | **+0500** |

**It follows daylight saving as of the photo's date, not the date you run the
comparison.** Both sets were tagged in a single Lightroom session in August, seconds
apart — so the PC's own offset was identical for both. If that were the varying term,
both would have wanted `+0400`. The January set wanted `+0500`, and January is EST.
One session,
one PC offset, two answers: the term that moved is the photo's date. So the rule is
**add back whatever the machine's local zone was on the day the photo was taken.**

*What appears to be happening, which explains the otherwise baffling part —* why a
camera already set to UTC needs any offset at all. Lightroom seems to convert the
**track's** UTC timestamps into machine-local time using the DST rules in effect on the
track's date, then match that against the camera's naive capture time. The slider exists
to cancel that conversion. Under that model both measured values fall out exactly, and
`0` would only be right on a machine whose local zone is UTC.

Two consequences for a future run. **Re-tagging the same two sets in winter does not
change these numbers** — they key off the capture dates, which do not move. But
**a different machine timezone, or a photo set from another date, changes them
entirely**, so derive rather than copy. This is inferred from two data points; the
positional check below is what actually confirms it.

**The reliable check is positional, not arithmetic:** a photo landing somewhere the
photographer was not means the offset is wrong. Don't save that — fix the offset and
re-tag. Each set's track is small enough to eyeball: **2,247 and 2,290 points, each
covering under an hour**, so a correct fix lands inside a short walk and a wrong one
is obvious on the map rather than subtle.

The exact spans are in `inventory/fixture-sources.md`, which is gitignored — they are
dates and places from a private library, and constraint 7 in
[`../CLAUDE.md`](../CLAUDE.md) keeps them out of a public repository. Nothing in the
procedure needs them; `rawgeotag --verbose --dry-run <empty-dir> <gpx>` prints the
span of whatever track you have.

## The questions to ask

1. Does it still write `exif:GPSLatitude` as `DDD,MM.mmmk` with a hemisphere letter?
   **Our whole packet rests on this one.**
2. Does tracklog auto-tag carry altitude, in what rational, and does it write
   `exif:GPSAltitudeRef`?
3. Still no `<?xpacket?>` wrapper and no BOM in sidecars?
4. Does it write `exif:GPSTimeStamp` or `exif:GPSMapDatum`?
5. Anything structurally new — namespaces, serialization form, GPS moving out of `exif:`?

Compare positions numerically as well as by eye; the encodings differ in precision, so
decode both to decimal degrees rather than diffing strings.

## Recorded results

**LR Classic 15.4.1, 2026-08-01** — 9 CR3 + 5 NEF. Answers: (1) unchanged,
`35,53.72480316N`; (2) altitude carried as `443400/10000`, **no `GPSAltitudeRef`**;
(3) no wrapper, no BOM — file starts `<x:xmpmeta`, ends `</x:xmpmeta>\n`; (4) neither;
(5) nothing new. `x:xmptk` byte-identical to 13.4. Writes
`photoshop:SidecarForExtension` for both formats.

Agreement with rawgeotag: **0.02-0.12 m on CR3, 0.33-0.53 m on NEF**, altitude usually
exact and 0.245 m at worst. The NEF gap is sub-second capture times — both cameras
record `SubSecTimeOriginal`, Lightroom honors it, we truncate to whole seconds. See
`CLAUDE.md` for why that is not worth adopting.

**Earlier eras, for the stability argument:** `Adobe XMP Core 5.6-c140` and
`7.0-c000` / LR Classic 13.4 (the latter carrying altitude), both read off Lightroom
sidecars on `Q:\` — six years apart. GPS encoding identical across all three eras.
`exiftool -XMPToolkit` on any sidecar in the archive finds more of them; the specific
files are not named here under constraint 7.

## Automating this with a Lightroom plugin — considered and declined 2026-08-02

The idea, in full, so it does not have to be re-derived: a Lua plugin using the
Lightroom Classic SDK that creates a temp catalog, imports one raw of every supported
type, applies arbitrary GPS to them, exports XMP, and deletes the catalog — leaving
sidecars to diff. It would turn the hour below into a command.

**Declined. Five reasons, roughly in order of weight.**

1. **What it would automate now costs about five minutes.** Step 2 above is the
   emission monitoring the plugin was for, and it turns out not to need a tracklog, a
   staged set, or the timezone dance — those belong to the positional diff, which is
   a different job done for a different reason. Automating a five-minute manual check
   with a Lua plugin is not a trade that pays.

   *An earlier version of this entry argued the plugin "automates the proxy, not the
   requirement", treating emission-watching as second-class. That was wrong and the
   argument is withdrawn: emission is the leading indicator, and watching it is how
   we avoid the day Lightroom stops accepting a format we let drift. The reason the
   plugin loses is cost, not purpose.*
2. **The guard shares a failure mode with the thing it guards, and bills you between
   uses.** A plugin is built on the Lightroom SDK, which Adobe also revises — so it
   would break on exactly the events it exists to check, and you would find out at
   the moment you needed it, having trusted it in the meantime. A guard that fails
   silently at the point of use is worse than no guard.

   **The standing cost is the worse half.** The plugin would have to be kept current
   against a shifting SDK *forever*, to serve a check run roughly once a year. A
   manual procedure has zero carrying cost between uses; a plugin's carrying cost
   never stops, and is paid in the same currency — Adobe changing something — that
   the check exists to detect. Keeping the process manual skips that entirely.
3. **The base rate is zero.** GPS encoding is unchanged across Adobe XMP Core
   5.6-c140 (2019), 7.0-c000 (2024) and 15.4.1 (2026), while namespaces were added
   around it. Adobe is demonstrably additive here.
4. **The failure is bounded and announces itself.** If Lightroom ever stopped reading
   our sidecars, the next import shows a photo with no location. That is a *missing*
   tag, which is the acceptable side of this project's own rule — the nightmare is a
   wrong tag, and this failure mode cannot produce one.
5. **Feasibility is unverified and may be fatal.** Plugins run inside an already-open
   catalog; whether the SDK can create or switch one is unconfirmed, and step (a)
   dies without it. Whether `setRawMetadata` can write GPS *altitude* rather than
   just latitude and longitude is likewise unconfirmed, as is driving "Save Metadata
   to File" without a human. Speculative work whose feasibility is unknown, against a
   risk that has never once materialised, is the wrong trade twice over.

**And there is no third door.** Checked 2026-08-02: Lightroom Classic has no CLI and
no COM interface on Windows, and Adobe's only supported automation surface *is* the
Lua plugin SDK. So "use an API instead of writing a plugin" is not an alternative —
it is the same proposal. Claude has no MCP server or skill for Lightroom either;
driving the UI through PowerShell SendKeys or UI Automation against a custom-drawn
interface would be more brittle than doing it by hand, and would still need a human
to set up catalog state.

**What *can* be automated is everything either side of the Lightroom step** — staging
the photo, running rawgeotag, and diffing whatever sidecar Lightroom produces against
the recorded 15.4.1 baseline. The irreducible manual part is opening Lightroom,
setting GPS on one photo, and pressing Ctrl+S.

**What would reopen the plugin question:** the quick check failing, or two consecutive
Lightroom releases that each move the GPS encoding — at which point the base rate
argument is dead and the manual cost starts compounding. Not before.

## Housekeeping

Sidecars Lightroom writes during this exercise are **Lightroom-created files**, so
constraint 6 binds on them — it follows the file, not the drive, and `N:\` being
disposable does not exempt them. Claude does not delete the staging tree; Terry does.
