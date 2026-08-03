<#
.SYNOPSIS
    Everything either side of the Lightroom step in the XMP drift check.

.DESCRIPTION
    docs/LIGHTROOM-XMP.md asks two questions on a major Lightroom upgrade: does
    Lightroom still read our sidecars, and has its own way of spelling a geotag
    moved. Lightroom Classic has no CLI and no COM interface, and Adobe's only
    automation surface is the Lua plugin SDK -- which was considered and declined --
    so the middle of that check is irreducibly manual.

    This script does the rest. -Stage prepares a photo and our reference sidecar and
    prints the handful of clicks to perform. -Compare reads whatever sidecar
    Lightroom wrote, pulls out the facts that matter, and diffs them against the
    recorded 15.4.1 baseline and against our own packet.

    It reports; it does not judge. Whether a difference is worth following is the
    "difference in kind, not additive" call recorded in CLAUDE.md, and that stays
    with a human.

.PARAMETER Stage
    Prepare the working directory and print the Lightroom steps.

.PARAMETER Compare
    Analyse the sidecar Lightroom wrote and report what moved.

.PARAMETER Analyze
    Analyse an arbitrary .xmp and print its facts. Useful for pointing at an
    archived Lightroom sidecar to see how an older era spelled things.

.PARAMETER Root
    Working directory. Defaults to N:\lr-xmp-check -- staging on N:\ is required,
    since this writes sidecars and constraint 5 forbids trial runs against Q:\.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Stage
    # ...do the Lightroom steps it prints...
    pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Compare

    # Spelled for cmd, which is the shell in use here and cannot run a .ps1 directly
    # -- a bare path opens it in Notepad and returns 0. From a PowerShell session it
    # is just .\scripts\lr-xmp-check.ps1 -Stage.

.EXAMPLE
    pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Analyze "Q:\Lightroom\Images\<year>\<date>\<photo>.xmp"
#>
[CmdletBinding(DefaultParameterSetName = 'Stage')]
param(
    [Parameter(ParameterSetName = 'Stage')]   [switch]$Stage,
    [Parameter(ParameterSetName = 'Compare')] [switch]$Compare,
    [Parameter(ParameterSetName = 'Analyze', Mandatory)] [string]$Analyze,
    [string]$Root = 'N:\lr-xmp-check',
    [string]$FixtureRoot = "$PSScriptRoot\..\..\RawGeotag-fixtures",
    [string]$Binary = "$PSScriptRoot\..\target\release\rawgeotag.exe"
)

$ErrorActionPreference = 'Stop'

# What Lightroom Classic 15.4.1 emitted on 2026-08-01, from the Recorded results
# section of docs/LIGHTROOM-XMP.md. A change here is the thing the check exists to
# notice; update it only after re-running the full comparison and recording why.
$Baseline = [ordered]@{
    'BOM'                 = 'absent'
    'xpacket wrapper'     = 'absent'
    'serialization'       = 'attribute'
    'coordinate form'     = 'DDD,MM.mmmk'
    'GPSVersionID'        = '2.2.0.0'
    'altitude rational'   = '/10000'
    'GPSAltitudeRef'      = 'absent'
    'GPSMapDatum'         = 'absent'
    'GPSTimeStamp'        = 'absent'
    'SidecarForExtension' = 'present'
}

function Get-Prop {
    # Attribute form first, since that is what both tools write; element form is
    # checked too, because a switch to it would itself be the finding.
    param([string]$Text, [string]$Name)
    if ($Text -match "$Name\s*=\s*`"([^`"]*)`"") { return @{ Value = $Matches[1]; Form = 'attribute' } }
    if ($Text -match "<$Name(?:\s[^>]*)?>([^<]*)</$Name>") { return @{ Value = $Matches[1]; Form = 'element' } }
    return $null
}

function Get-CoordinateForm {
    param([string]$Value)
    if (-not $Value) { return 'absent' }
    # The form our whole packet rests on: degrees, comma, decimal minutes, hemisphere.
    if ($Value -match '^\d+,\d+(\.\d+)?[NSEW]$') { return 'DDD,MM.mmmk' }
    if ($Value -match '^-?\d+\.\d+$') { return 'decimal degrees' }
    return "unrecognised ($Value)"
}

function Get-XmpFacts {
    param([string]$Path)

    $bytes = [IO.File]::ReadAllBytes($Path)
    $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
    $text = [IO.File]::ReadAllText($Path)

    $lat = Get-Prop $text 'exif:GPSLatitude'
    $alt = Get-Prop $text 'exif:GPSAltitude'

    $altRational = 'absent'
    if ($alt -and $alt.Value -match '^\d+/(\d+)$') { $altRational = "/$($Matches[1])" }
    elseif ($alt) { $altRational = "unrecognised ($($alt.Value))" }

    $form = 'none found'
    if ($lat) { $form = $lat.Form }

    [ordered]@{
        'BOM'                 = if ($hasBom) { 'present' } else { 'absent' }
        'xpacket wrapper'     = if ($text -match '<\?xpacket') { 'present' } else { 'absent' }
        'serialization'       = $form
        'coordinate form'     = Get-CoordinateForm ($lat.Value)
        'GPSVersionID'        = (Get-Prop $text 'exif:GPSVersionID').Value ?? 'absent'
        'altitude rational'   = $altRational
        'GPSAltitudeRef'      = if (Get-Prop $text 'exif:GPSAltitudeRef') { 'present' } else { 'absent' }
        'GPSMapDatum'         = if (Get-Prop $text 'exif:GPSMapDatum') { 'present' } else { 'absent' }
        'GPSTimeStamp'        = if (Get-Prop $text 'exif:GPSTimeStamp') { 'present' } else { 'absent' }
        'SidecarForExtension' = if (Get-Prop $text 'photoshop:SidecarForExtension') { 'present' } else { 'absent' }
    }
}

function Get-Extras {
    # Reported but never compared: the writer string moves every release, and a new
    # namespace is additive by definition. Both are context for a human, not signal.
    param([string]$Path)
    $text = [IO.File]::ReadAllText($Path)
    $tk = if ($text -match 'x:xmptk="([^"]*)"') { $Matches[1] } else { '(none)' }
    $ns = ([regex]::Matches($text, 'xmlns:([A-Za-z0-9]+)=') | ForEach-Object { $_.Groups[1].Value } |
           Sort-Object -Unique) -join ', '
    @{ Toolkit = $tk; Namespaces = $ns }
}

function Show-Facts {
    param([string]$Label, [string]$Path)
    $facts = Get-XmpFacts $Path
    $extra = Get-Extras $Path
    Write-Host "`n=== $Label ===" -ForegroundColor Cyan
    Write-Host "    $Path"
    Write-Host "    x:xmptk   : $($extra.Toolkit)"
    Write-Host "    namespaces: $($extra.Namespaces)"
    Write-Host ""
    foreach ($k in $facts.Keys) { "    {0,-21} {1}" -f $k, $facts[$k] | Write-Host }
    return $facts
}

# ---- -Analyze ---------------------------------------------------------------

if ($PSCmdlet.ParameterSetName -eq 'Analyze') {
    if (-not (Test-Path -LiteralPath $Analyze)) { throw "no such file: $Analyze" }
    Show-Facts 'XMP facts' (Resolve-Path -LiteralPath $Analyze).Path | Out-Null
    Write-Host ""
    exit 0
}

# ---- -Stage -----------------------------------------------------------------

if ($PSCmdlet.ParameterSetName -eq 'Stage' -or $Stage) {
    if (-not (Test-Path $Binary)) { throw "rawgeotag not found at $Binary -- cargo build --release first" }
    if (-not (Test-Path $FixtureRoot)) { throw "fixture root not found at $FixtureRoot -- see docs/FIXTURES.md" }

    $src = Join-Path (Resolve-Path $FixtureRoot) 'cr3-offset-utc'
    $gpxSrc = Join-Path (Resolve-Path $FixtureRoot) 'gpx\cr3-offset-utc.gpx'
    $photo = Get-ChildItem -LiteralPath $src -Filter *.CR3 -Force | Sort-Object Name | Select-Object -First 1
    if (-not $photo) { throw "no CR3 found in $src" }

    # Both checks in one go, because they want opposite setups and are meant to be
    # run in a single Lightroom session: the read check needs our sidecar *present*
    # so Lightroom can adopt it, the emission check needs it *absent* so Lightroom
    # writes its own. Staging them together is what stops the second being skipped.
    $read = "$Root\1-read-check"
    $emit = "$Root\2-emission-check"
    $ref = "$Root\rawgeotag-reference"

    # Fresh every time: a leftover sidecar from a previous run is exactly the thing
    # that would make the comparison meaningless.
    Remove-Item -Recurse -Force $Root -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $read, $emit, $ref | Out-Null

    Copy-Item $photo.FullName -Destination $emit
    Copy-Item $gpxSrc -Destination $emit
    $gpx = Join-Path $emit (Split-Path $gpxSrc -Leaf)

    & $Binary --no-progress $emit $gpx | Out-Null
    $sidecar = Get-ChildItem -LiteralPath $emit -Filter *.xmp -Force | Select-Object -First 1
    if (-not $sidecar) { throw "rawgeotag wrote no sidecar for $($photo.Name)" }

    # The read check gets the photo *and* our sidecar; the emission check keeps the
    # photo alone, with our packet held aside for the diff.
    Copy-Item $photo.FullName -Destination $read
    Copy-Item $sidecar.FullName -Destination $read
    Move-Item $sidecar.FullName -Destination $ref -Force

    $leaked = @(Get-ChildItem -LiteralPath $emit -Filter *.xmp -Force)
    if ($leaked.Count -ne 0) { throw "our sidecar is still in $emit; Lightroom would adopt it" }

    Write-Host "staged: $Root" -ForegroundColor Green
    Write-Host @"

In Lightroom Classic — this part cannot be automated, see docs/LIGHTROOM-XMP.md.

  Check 1 — does Lightroom still READ ours?   (~2 min)
    a. Import  $read  with **Add**.
    b. Look at the Map module, or the GPS field in the Metadata panel.
       A pin where the shoot happened means current Lightroom still ingests our packet.

  Check 2 — how does Lightroom SPELL a geotag now?   (~5 min)
    a. Import  $emit  with **Add**.
    b. Select the photo, type any coordinates into the Metadata panel's GPS field.
       They need not be correct — only the spelling is being read.
    c. Metadata > Save Metadata to File  (Ctrl+S).

Then:  pwsh -NoProfile -File .\scripts\lr-xmp-check.ps1 -Compare
"@
    exit 0
}

# ---- -Compare ---------------------------------------------------------------

$emit = "$Root\2-emission-check"
$ours = Get-ChildItem -LiteralPath "$Root\rawgeotag-reference" -Filter *.xmp -Force -ErrorAction SilentlyContinue |
        Select-Object -First 1
$theirs = Get-ChildItem -LiteralPath $emit -Filter *.xmp -Force -ErrorAction SilentlyContinue |
          Select-Object -First 1

if (-not $ours) { throw "no reference sidecar in $Root\rawgeotag-reference -- run -Stage first" }
if (-not $theirs) {
    throw "no Lightroom sidecar in $emit -- do check 2 from -Stage, including Ctrl+S"
}

$lr = Show-Facts 'Lightroom, this version' $theirs.FullName
Show-Facts 'rawgeotag' $ours.FullName | Out-Null

Write-Host "`n=== against the 15.4.1 baseline ===" -ForegroundColor Cyan
$moved = @()
foreach ($k in $Baseline.Keys) {
    $want = $Baseline[$k]
    $got = $lr[$k]
    if ($got -eq $want) {
        "    {0,-21} {1,-16} unchanged" -f $k, $got | Write-Host -ForegroundColor Green
    } else {
        "    {0,-21} {1,-16} WAS: {2}" -f $k, $got, $want | Write-Host -ForegroundColor Yellow
        $moved += $k
    }
}

Write-Host ""
if ($moved.Count -eq 0) {
    Write-Host "Lightroom's geotag encoding is unchanged since 15.4.1. Nothing to follow." -ForegroundColor Green
    exit 0
}

Write-Host "$($moved.Count) row(s) moved: $($moved -join ', ')" -ForegroundColor Yellow
Write-Host @"

That is a report, not a verdict. Apply the rule in CLAUDE.md's *The XMP we emit*:
follow a difference **in kind**, ignore one that is merely **additive**. Lightroom
writing a property we do not is not a reason to change; Lightroom writing
*coordinates* differently is. Run the full same-photo, same-track diff in
docs/LIGHTROOM-XMP.md before changing the packet, and re-derive the fixture hashes
afterwards.
"@
exit 1
