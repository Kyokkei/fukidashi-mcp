<#
.SYNOPSIS
    Prepare a review smoke fixture without ever serving the caller's job.

This helper copies a legacy job into a temporary jobs root and rewrites JSON
artifact paths to the copied source tree. Any browser/MCP approval smoke must
use the returned isolated path; the input directory is never passed to the
server.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $FixtureRoot,
    [string] $JobsRoot = 'E:\ComicTranslate\jobs'
)

$source = (Resolve-Path -LiteralPath $FixtureRoot -ErrorAction Stop).Path
$realJobs = [IO.Path]::GetFullPath($JobsRoot)
$sourceFull = [IO.Path]::GetFullPath($source)
if ($sourceFull.Equals($realJobs, [StringComparison]::OrdinalIgnoreCase) -or
    $sourceFull.StartsWith($realJobs.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to smoke-test a job in the live jobs root. Pass a source folder or copy it first."
}

$isolatedBase = Join-Path ([IO.Path]::GetTempPath()) ("fukidashi-review-smoke-" + [guid]::NewGuid().ToString('N'))
$isolatedJobs = Join-Path $isolatedBase 'jobs'
$isolatedSource = Join-Path $isolatedBase 'source'
New-Item -ItemType Directory -Path $isolatedJobs, $isolatedSource -Force | Out-Null
Copy-Item -Path (Join-Path $source '*') -Destination $isolatedSource -Recurse -Force

$copiedJob = Join-Path $isolatedJobs ([IO.Path]::GetFileName($sourceFull))
Move-Item -LiteralPath $isolatedSource -Destination $copiedJob

# A copied manifest/sidecar must point at the copied source. Replace both the
# JSON-escaped and ordinary spellings, while leaving image bytes untouched.
$sourceEscaped = $sourceFull.Replace('\', '\\')
$targetRoot = [IO.Path]::GetFullPath($copiedJob)
$targetEscaped = $targetRoot.Replace('\', '\\')
Get-ChildItem -LiteralPath $copiedJob -Recurse -Filter '*.json' -File | ForEach-Object {
    $text = [IO.File]::ReadAllText($_.FullName)
    $text = $text.Replace($sourceEscaped, $targetEscaped).Replace($sourceFull, $targetRoot)
    [IO.File]::WriteAllText($_.FullName, $text, (New-Object Text.UTF8Encoding($false)))
}

# Put the copied managed job under the isolated server-owned root. The caller
# may launch the MCP with FUKIDASHI_JOBS_DIR set to this value and use only the
# copied rendered path. Never submit review approval using $FixtureRoot.
[pscustomobject]@{
    IsolatedJobsRoot = $isolatedJobs
    IsolatedJob = $copiedJob
    Warning = 'Use only IsolatedJob paths for review smoke requests; the input fixture is never served.'
}
