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

## The method, and the one thing that makes it work

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

```powershell
$src  = "C:\Users\TDO-XPS15-2024\Claude\RawGeotag-fixtures"
$root = "N:\lr-xmp-compare"
$rg   = "C:\Users\TDO-XPS15-2024\Claude\RawGeotag\target\release\rawgeotag.exe"

foreach ($p in @("$root\cr3-malta","$root\nef-sedona",
                 "$root\rawgeotag-reference\cr3-malta","$root\rawgeotag-reference\nef-sedona")) {
    New-Item -ItemType Directory -Force -Path $p | Out-Null
}

# Spread the CR3 selection: consecutive frames are often one stationary position.
$all = Get-ChildItem -LiteralPath "$src\cr3-malta" -Filter *.CR3 -Force | Sort-Object Name
foreach ($i in @(0,1,2,3,4,14,23,31,39)) { Copy-Item $all[$i].FullName -Destination "$root\cr3-malta" }
Get-ChildItem -LiteralPath "$src\nef-sedona" -Filter *.NEF -Force |
    Select-Object -First 5 | Copy-Item -Destination "$root\nef-sedona"

Copy-Item "$src\gpx\malta-2025-09-18.gpx"  "$root\cr3-malta"
Copy-Item "$src\gpx\sedona-2019-01-19.gpx" "$root\nef-sedona"

# Our reference sidecars, then moved out of the import folders.
& $rg --no-progress "$root\cr3-malta" cr3 "$root\cr3-malta\malta-2025-09-18.gpx"
& $rg --no-progress --utc-offset +0000 "$root\nef-sedona" nef "$root\nef-sedona\sedona-2019-01-19.gpx"
foreach ($s in @("cr3-malta","nef-sedona")) {
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

**Time zone.** Both cameras' clocks were on UTC. CR3 carries
`OffsetTimeOriginal="+00:00"` so Lightroom needs no help; **NEF carries no offset tag at
all**, which is why rawgeotag requires `--utc-offset` for a D3300 — if auto-tag tags
nothing, use *Set Time Zone Offset…* until capture times land inside the track span.
Sanity check: a photo landing outside Malta or Sedona means the offset is wrong.

Track spans, both UTC:

| Track | Points | From | To |
|---|---|---|---|
| `malta-2025-09-18.gpx` | 2,247 | 2025-09-18T06:50:02Z | 2025-09-18T07:39:52Z |
| `sedona-2019-01-19.gpx` | 2,290 | 2019-01-19T20:48:50Z | 2019-01-19T21:40:34Z |

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
record `SubSecTimeOriginal`, Lightroom honours it, we truncate to whole seconds. See
`CLAUDE.md` for why that is not worth adopting.

**Earlier eras, for the stability argument:** `Adobe XMP Core 5.6-c140` (2019, on `Q:\`
at `2019\2019-01-19\DSC_0001.xmp`) and `7.0-c000` / LR Classic 13.4
(`2023\2023-05-06\DSC_0218.xmp`, which has altitude). GPS encoding identical across all
three eras.

## Housekeeping

Sidecars Lightroom writes during this exercise are **Lightroom-created files**, so
constraint 6 binds on them — it follows the file, not the drive, and `N:\` being
disposable does not exempt them. Claude does not delete the staging tree; Terry does.
