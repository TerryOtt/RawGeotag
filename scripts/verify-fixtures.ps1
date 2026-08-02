<#
.SYNOPSIS
    Verify rawgeotag against every supported format in one pass.

.DESCRIPTION
    Verification means ALL formats, not whichever one is on your mind. CR3 and NEF
    take different code paths -- RawFormat::read_strategy returns Streaming for one
    and WholeFile for the other -- so passing on one says nothing about the other.

    Three fixtures, chosen so their timezone cases differ. That matters more than
    the file count: a bug that dropped the EXIF offset entirely would pass Malta
    (+00:00 is a no-op) and Sedona (no offset at all) while silently misplacing
    every Rockies photo by ~50 km, which would still tag and never warn.

    Each fixture is run three times, which is why the loop below invokes the binary
    more than once: a dry run that must write nothing, the real run whose sidecars
    are hashed against a recorded aggregate, and a --dry-run --force rehearsal --
    the documented preview of a destructive run -- that must leave those sidecars
    untouched.

    This script and the manifests live in the repo; the raws do not, since they are
    222 MB. See docs/FIXTURES.md for what the fixture holds and how to rebuild it.

.PARAMETER FixtureRoot
    Directory holding cr3-malta/, cr3-rockies/, nef-sedona/ and gpx/.

.PARAMETER Binary
    Path to rawgeotag.exe. Defaults to this repo's release build.

.PARAMETER CheckSources
    Also verify every source raw against its recorded SHA-256. Cheap now that the
    sets are two files each -- reach for it when a hash mismatch might mean the
    fixture drifted rather than the code changing.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\verify-fixtures.ps1

    # Spelled for cmd, which is the shell in use here and cannot run a .ps1 directly
    # -- a bare path opens it in Notepad and returns 0, which reads like a pass. From
    # a PowerShell session it is just .\scripts\verify-fixtures.ps1.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\verify-fixtures.ps1 -CheckSources
#>
[CmdletBinding()]
param(
    [string]$FixtureRoot = "$PSScriptRoot\..\..\RawGeotag-fixtures",
    [string]$Binary = "$PSScriptRoot\..\target\release\rawgeotag.exe",
    [switch]$CheckSources
)

$ErrorActionPreference = 'Stop'

# Expected aggregates. A deliberate change to the XMP packet or to the crate
# version in x:xmptk moves these legitimately -- re-derive, do not hunt a bug.
#
# Two files per set, not forty. The old counts were inherited from "the first N by
# name" and bought nothing: within a set the files are the same camera on the same
# shoot, so file forty exercises no code that file one does not. What the three sets
# do differ in -- read strategy and timezone case -- is preserved exactly, which is
# where all the coverage was. See "Bring your own raws" in docs/FIXTURES.md.
$Fixtures = @(
    @{ Name = 'cr3-malta';   Gpx = 'malta-2025-09-18.gpx';   Args = @();
       Count = 2; Hash = 'CF2D1DA68FA359AA'
       Exercises = 'Streaming read path; EXIF offset +00:00 (present, no-op)' }
    @{ Name = 'cr3-rockies'; Gpx = 'rockies-2022-09-27.gpx'; Args = @();
       Count = 2; Hash = '047EF9B17BE64472'
       Exercises = 'Streaming read path; EXIF offset +01:00 (real conversion)' }
    @{ Name = 'nef-sedona';  Gpx = 'sedona-2019-01-19.gpx';  Args = @('--utc-offset', '+0000');
       Count = 2; Hash = 'F858DA7AA022AF2B'
       Exercises = 'WholeFile read path; no EXIF offset, so --utc-offset is required' }
)

function Get-AggregateHash([string]$Dir) {
    # -LiteralPath -Force, never a wildcard path: the wildcard form has served a
    # stale directory listing over SMB and reported 3 files where 30 existed.
    $xmp = Get-ChildItem -LiteralPath $Dir -Filter *.xmp -Force | Sort-Object Name
    if (-not $xmp) { return @{ Count = 0; Hash = '(none)' } }
    $joined = ($xmp | ForEach-Object { (Get-FileHash $_.FullName -Algorithm SHA256).Hash }) -join ''
    $bytes = [Text.Encoding]::UTF8.GetBytes($joined)
    $agg = (Get-FileHash -InputStream ([IO.MemoryStream]::new($bytes)) -Algorithm SHA256).Hash
    @{ Count = $xmp.Count; Hash = $agg.Substring(0, 16) }
}

if (-not (Test-Path $Binary)) {
    throw "rawgeotag not found at $Binary -- run cargo build --release first"
}
if (-not (Test-Path $FixtureRoot)) {
    throw "fixture root not found at $FixtureRoot -- see docs/FIXTURES.md to rebuild it"
}

$FixtureRoot = (Resolve-Path $FixtureRoot).Path
Write-Host "binary : $((Resolve-Path $Binary).Path)"
Write-Host "fixture: $FixtureRoot`n"

$failed = @()

foreach ($f in $Fixtures) {
    $dir = Join-Path $FixtureRoot $f.Name
    $gpx = Join-Path $FixtureRoot "gpx\$($f.Gpx)"
    Write-Host "=== $($f.Name) ===" -ForegroundColor Cyan
    Write-Host "    exercises: $($f.Exercises)"

    if (-not (Test-Path $dir)) {
        Write-Host "    MISSING  : $dir" -ForegroundColor Red
        $failed += "$($f.Name): fixture directory missing"
        continue
    }

    if ($CheckSources) {
        $manifest = Join-Path $PSScriptRoot "fixture-manifests\$($f.Name).sha256"
        $bad = 0
        foreach ($line in Get-Content $manifest) {
            $want, $name = $line -split '\s+', 2
            $path = Join-Path $dir $name.Trim()
            if (-not (Test-Path $path)) { $bad++; continue }
            if ((Get-FileHash $path -Algorithm SHA256).Hash -ne $want) { $bad++ }
        }
        if ($bad) {
            Write-Host "    sources  : FAIL -- $bad file(s) differ from the manifest" -ForegroundColor Red
            $failed += "$($f.Name): $bad source file(s) do not match the recorded SHA-256"
        } else {
            Write-Host "    sources  : match the recorded SHA-256  OK" -ForegroundColor Green
        }
    }

    # A leftover sidecar is SKIPPED rather than rewritten, which silently changes
    # the aggregate. Clearing first is not optional.
    Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force | Remove-Item -Force

    # The gate: a format with no EXIF offset must refuse the whole run when
    # --utc-offset is absent. Only meaningful where the body records no offset.
    if ($f.Args -contains '--utc-offset') {
        & $Binary $dir $gpx 2>&1 | Out-Null
        $leaked = (Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force).Count
        if ($leaked -eq 0) {
            Write-Host "    gate     : refused without --utc-offset, wrote nothing  OK" -ForegroundColor Green
        } else {
            Write-Host "    gate     : FAIL -- wrote $leaked sidecars without --utc-offset" -ForegroundColor Red
            $failed += "$($f.Name): gate leaked $leaked sidecars"
        }
    }

    # --dry-run does every bit of the work and writes nothing. Checked on a clean
    # directory, so any sidecar present afterwards was written by this invocation.
    & $Binary @($f.Args) --dry-run $dir $gpx 2>$null | Out-Null
    $leaked = @(Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force).Count
    if ($leaked -eq 0) {
        Write-Host "    dry run  : wrote nothing  OK" -ForegroundColor Green
    } else {
        Write-Host "    dry run  : FAIL -- wrote $leaked sidecars" -ForegroundColor Red
        $failed += "$($f.Name): --dry-run wrote $leaked sidecars"
    }

    # A no-op when the check above passed, which is the point: it keeps the real run
    # below independent of it. Without this a dry run that wrongly wrote would hand
    # the aggregate its own output to hash, and the count could read OK while two
    # things were broken at once.
    Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force | Remove-Item -Force

    & $Binary @($f.Args) $dir $gpx 2>$null | Out-Null
    $got = Get-AggregateHash $dir

    if ($got.Count -ne $f.Count) {
        Write-Host "    count    : FAIL -- $($got.Count), expected $($f.Count)" -ForegroundColor Red
        $failed += "$($f.Name): wrote $($got.Count) sidecars, expected $($f.Count)"
    } else {
        Write-Host "    count    : $($got.Count) sidecars  OK" -ForegroundColor Green
    }

    if ($got.Hash -eq $f.Hash) {
        Write-Host "    aggregate: $($got.Hash)  OK" -ForegroundColor Green
    } else {
        Write-Host "    aggregate: FAIL -- $($got.Hash), expected $($f.Hash)" -ForegroundColor Red
        $failed += "$($f.Name): aggregate $($got.Hash), expected $($f.Hash)"
    }

    # --dry-run --force is the documented rehearsal for a forced run: force gets the
    # existing sidecars past the skip-existing check so they are reported as tagged,
    # and dry_run still returns before the write. The sidecars from the run above are
    # what make this a real check -- on an empty directory --force has nothing to
    # overwrite and it would prove nothing.
    #
    # Compared by LAST WRITE TIME and not by hash, which is the whole point: a
    # rehearsal that wrongly wrote would re-render the same photo from the same
    # track and lay down byte-identical sidecars, so no content comparison can see
    # it. Measured, not reasoned -- the first version of this check compared
    # aggregates and passed a mutation that made --dry-run --force write.
    $stamps = {
        (Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force | Sort-Object Name |
            ForEach-Object { "$($_.Name):$($_.LastWriteTimeUtc.Ticks)" }) -join '|'
    }
    $before = & $stamps
    & $Binary @($f.Args) --dry-run --force $dir $gpx 2>$null | Out-Null
    if ((& $stamps) -eq $before) {
        Write-Host "    rehearsal: --dry-run --force left them untouched  OK" -ForegroundColor Green
    } else {
        Write-Host "    rehearsal: FAIL -- --dry-run --force rewrote the sidecars" -ForegroundColor Red
        $failed += "$($f.Name): --dry-run --force rewrote existing sidecars"
    }

    Get-ChildItem -LiteralPath $dir -Filter *.xmp -Force | Remove-Item -Force
    Write-Host ""
}

if ($failed) {
    Write-Host "FAILED:" -ForegroundColor Red
    $failed | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
Write-Host "all fixtures pass" -ForegroundColor Green
