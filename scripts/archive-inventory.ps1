<#
.SYNOPSIS
    Refresh the archive manifests under inventory/ by walking the NAS.

.DESCRIPTION
    Writes two CSVs that let `archive-untagged.ps1` answer "which shoots could
    still be geotagged?" without touching the NAS again. This script is the slow
    half: a recursive walk of an 11 TB share over SMB takes minutes, which is the
    whole reason its results are committed.

    inventory\photo-dirs.csv   one row per directory holding a raw file
    inventory\gpx-tracks.csv   one row per GPX track, with its true UTC span

    Read-only. It creates nothing on Q:\ and modifies nothing there.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\archive-inventory.ps1
#>
[CmdletBinding()]
param(
    [string] $ImageRoot = 'Q:\Lightroom\Images',
    [string] $TrackRoot = 'Q:\Photo GPX Tracks',
    [string] $OutDir    = (Join-Path $PSScriptRoot '..\inventory')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

foreach ($root in $ImageRoot, $TrackRoot) {
    if (-not (Test-Path -LiteralPath $root)) { throw "not found: $root" }
}
$OutDir = (New-Item -ItemType Directory -Force -Path $OutDir).FullName

# .NET enumeration rather than Get-ChildItem: it yields paths instead of
# constructing a FileInfo per file, which over SMB is the difference between
# minutes and many more of them.
$walk = [System.IO.EnumerationOptions] @{
    RecurseSubdirectories = $true
    IgnoreInaccessible    = $true
    AttributesToSkip      = 'None'
}

Write-Host "walking $ImageRoot ..."
$counts = @{}
foreach ($path in [System.IO.Directory]::EnumerateFiles($ImageRoot, '*', $walk)) {
    $ext = [System.IO.Path]::GetExtension($path).ToLowerInvariant()
    # Dng is counted but not geotaggable: rawgeotag reads CR3 and NEF only, and a
    # folder that looks short of sidecars may simply hold Lightroom's HDR merges.
    if ($ext -notin '.cr3', '.nef', '.dng', '.xmp') { continue }

    $dir = [System.IO.Path]::GetDirectoryName($path)
    if (-not $counts.ContainsKey($dir)) {
        $counts[$dir] = [pscustomobject] @{ Dir = $dir; Cr3 = 0; Nef = 0; Dng = 0; Xmp = 0 }
    }
    switch ($ext) {
        '.cr3' { $counts[$dir].Cr3++ }
        '.nef' { $counts[$dir].Nef++ }
        '.dng' { $counts[$dir].Dng++ }
        '.xmp' { $counts[$dir].Xmp++ }
    }
}

$photoCsv = Join-Path $OutDir 'photo-dirs.csv'
$counts.Values |
    Where-Object { $_.Cr3 + $_.Nef + $_.Dng -gt 0 } |
    Sort-Object Dir |
    Select-Object @{ n = 'Dir'; e = { [System.IO.Path]::GetRelativePath($ImageRoot, $_.Dir) } }, Cr3, Nef, Dng, Xmp |
    Export-Csv -LiteralPath $photoCsv -NoTypeInformation -Encoding utf8

Write-Host "walking $TrackRoot ..."
$tracks = foreach ($path in [System.IO.Directory]::EnumerateFiles($TrackRoot, '*.gpx', $walk)) {
    # Only <trkpt> times count. A <metadata><time> is the export date -- on the
    # Rockies tracks it sits months after the shoot, and taking it as the span's
    # end made every one of them look like it covered the whole autumn.
    $times = foreach ($chunk in ([System.IO.File]::ReadAllText($path) -split '<trkpt' | Select-Object -Skip 1)) {
        if ($chunk -match '<time>([^<]+)</time>') {
            [datetime]::Parse($matches[1], [cultureinfo]::InvariantCulture,
                [System.Globalization.DateTimeStyles]::AdjustToUniversal -bor
                [System.Globalization.DateTimeStyles]::AssumeUniversal)
        }
    }
    if (-not $times) {
        Write-Warning "no timed track points: $path"
        continue
    }
    $measure = $times | Measure-Object -Minimum -Maximum
    [pscustomobject] @{
        Track    = [System.IO.Path]::GetRelativePath($TrackRoot, $path)
        StartUtc = $measure.Minimum.ToString('yyyy-MM-ddTHH:mm:ssZ')
        EndUtc   = $measure.Maximum.ToString('yyyy-MM-ddTHH:mm:ssZ')
        Points   = $times.Count
    }
}

$trackCsv = Join-Path $OutDir 'gpx-tracks.csv'
$tracks | Sort-Object StartUtc | Export-Csv -LiteralPath $trackCsv -NoTypeInformation -Encoding utf8

Write-Host ''
Write-Host ("{0,6} photo directories -> {1}" -f (Import-Csv $photoCsv).Count, $photoCsv)
Write-Host ("{0,6} GPX tracks         -> {1}" -f (Import-Csv $trackCsv).Count, $trackCsv)
