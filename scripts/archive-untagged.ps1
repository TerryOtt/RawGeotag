<#
.SYNOPSIS
    Which shoots still have raws to geotag? Answered from inventory/, not the NAS.

.DESCRIPTION
    Joins inventory\photo-dirs.csv against inventory\gpx-tracks.csv and reports the
    directories holding CR3 or NEF files with fewer sidecars than raws. Runs in
    well under a second and touches no share; when the manifests are stale, refresh
    them with archive-inventory.ps1.

    A directory is matched to a track when the track's UTC span overlaps the UTC day
    named by the directory (Q:\Lightroom\Images\<year>\<yyyy-MM-dd>). That is the
    archive's own convention, and Terry's cameras are set to UTC -- but a shoot that
    ran past midnight, or a body left on local time, can put frames outside the day
    its folder names. -SlackHours widens the window on both sides when chasing one.

    The counts are a directory listing, not a prediction: a raw inside the span still
    needs a track point close enough in time and distance to earn a tag, so the
    untagged column is an upper bound. `--dry-run` gives the real number.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\archive-untagged.ps1

.EXAMPLE
    pwsh -NoProfile -File .\scripts\archive-untagged.ps1 -SlackHours 12 -ShowCovered
#>
[CmdletBinding()]
param(
    [string] $InventoryDir = (Join-Path $PSScriptRoot '..\inventory'),
    [double] $SlackHours   = 0,
    [switch] $ShowCovered
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$photoCsv = Join-Path $InventoryDir 'photo-dirs.csv'
$trackCsv = Join-Path $InventoryDir 'gpx-tracks.csv'
foreach ($csv in $photoCsv, $trackCsv) {
    if (-not (Test-Path -LiteralPath $csv)) {
        throw "missing $csv -- run scripts\archive-inventory.ps1 first"
    }
}

$asOf = (Get-Item -LiteralPath $photoCsv).LastWriteTime
$tracks = Import-Csv -LiteralPath $trackCsv | ForEach-Object {
    [pscustomobject] @{
        Track = $_.Track
        Start = [datetime]::Parse($_.StartUtc, [cultureinfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::AdjustToUniversal)
        End   = [datetime]::Parse($_.EndUtc, [cultureinfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::AdjustToUniversal)
    }
}

$tracked   = [System.Collections.Generic.List[object]]::new()
$untracked = [System.Collections.Generic.List[object]]::new()

foreach ($row in Import-Csv -LiteralPath $photoCsv) {
    $raws = [int] $row.Cr3 + [int] $row.Nef
    $untagged = $raws - [int] $row.Xmp
    if ($raws -eq 0) { continue }
    if ($untagged -le 0 -and -not $ShowCovered) { continue }

    [datetime] $day = [datetime]::MinValue
    $named = [datetime]::TryParseExact(
        (Split-Path $row.Dir -Leaf), 'yyyy-MM-dd', [cultureinfo]::InvariantCulture,
        [System.Globalization.DateTimeStyles]::AdjustToUniversal, [ref] $day)
    if (-not $named) {
        Write-Warning "directory name is not a date, cannot match a track: $($row.Dir)"
        continue
    }

    $from = $day.AddHours(-$SlackHours)
    $to   = $day.AddDays(1).AddHours($SlackHours)
    $hits = @($tracks | Where-Object { $_.Start -lt $to -and $_.End -gt $from })

    $record = [pscustomobject] @{
        Dir      = $row.Dir
        Raws     = $raws
        Xmp      = [int] $row.Xmp
        Untagged = $untagged
        Tracks   = $hits
    }
    if ($hits.Count -gt 0) { $tracked.Add($record) } else { $untracked.Add($record) }
}

function Show-Section {
    param([string] $Title, [object[]] $Rows)

    Write-Host ''
    Write-Host $Title
    Write-Host ('-' * $Title.Length)
    if ($Rows.Count -eq 0) { Write-Host '  (none)'; return }

    foreach ($r in $Rows | Sort-Object { $_.Untagged } -Descending) {
        Write-Host ('  {0,-20} raw {1,7:N0}   xmp {2,7:N0}   untagged {3,7:N0}' -f
            $r.Dir, $r.Raws, $r.Xmp, $r.Untagged)
        foreach ($t in $r.Tracks) {
            Write-Host ('      {0:yyyy-MM-dd HH:mm}Z .. {1:yyyy-MM-dd HH:mm}Z  {2}' -f
                $t.Start, $t.End, $t.Track)
        }
    }
}

Write-Host ("inventory as of {0:yyyy-MM-dd HH:mm}  (refresh: scripts\archive-inventory.ps1)" -f $asOf)
Show-Section 'Geotaggable now -- untagged raws with a track covering the day' $tracked.ToArray()
Show-Section 'No track for that day -- nothing to do' $untracked.ToArray()

Write-Host ''
Write-Host ('{0:N0} raws across {1:N0} directories have a covering track and no sidecar.' -f
    (($tracked | Measure-Object Untagged -Sum).Sum), $tracked.Count)
