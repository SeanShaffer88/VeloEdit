# VeloEdit — benchmark harness (roadmap item 50)
#
# Builds a sparse synthetic file of -SizeGB gibibytes, then times four
# headless operations with a stopwatch and reports each process's peak
# working set:
#
#   Cold launch   VeloEdit --help            (process start + first output)
#   Find          VeloEdit <file> --find TODO --headless
#   Save copy     VeloEdit <file> --save-copy <copy>
#   Extract       VeloEdit extract <file> --start 0 --length 1M --out <slice>
#
# The test file is sparse (fsutil), so creating even a 100 GB file is
# fast and uses almost no disk — but note the save-copy step writes a
# REAL full-size copy, so the disk needs roughly -SizeGB of free space
# for the duration of the run.
#
# Usage:
#   pwsh scripts\Run-Benchmarks.ps1 [-SizeGB 1] [-Exe <path>] [-Keep]
#
# CI (.github/workflows/benchmarks.yml) runs this at 1 GB on a Windows
# runner. The 10 GB and 100 GB runs are local, on a machine with the
# disk space for the save-copy step. Results are a plain table on
# stdout; nothing here is a pass/fail gate — timings are evidence for
# docs/BENCHMARKS.md, read by a human.

[CmdletBinding()]
param(
    [ValidateRange(1, 1024)]
    [int]$SizeGB = 1,
    [string]$Exe = "target\x86_64-pc-windows-msvc\release\VeloEdit.exe",
    [switch]$Keep
)

$ErrorActionPreference = "Stop"

$exePath = Resolve-Path $Exe -ErrorAction Stop
$sizeBytes = [int64]$SizeGB * 1GB
$workDir = Join-Path ([System.IO.Path]::GetTempPath()) "VeloEdit-Bench-$SizeGB`GB"
New-Item -ItemType Directory -Force $workDir | Out-Null
$testFile = Join-Path $workDir "bench-$SizeGB`GB.bin"
$copyFile = Join-Path $workDir "bench-copy.bin"
$sliceFile = Join-Path $workDir "bench-slice.bin"

# --- Build the sparse fixture -------------------------------------------
# fsutil creates the file at full logical size; marking it sparse keeps
# the untouched regions from consuming disk. A few TODO markers are
# planted (start, middle, near the end) so --find has known matches.
if (Test-Path $testFile) { Remove-Item $testFile -Force }
& fsutil file createNew $testFile $sizeBytes | Out-Null
if ($LASTEXITCODE -ne 0) { throw "fsutil file createNew failed ($LASTEXITCODE)" }
& fsutil sparse setFile $testFile | Out-Null
if ($LASTEXITCODE -ne 0) { throw "fsutil sparse setFile failed ($LASTEXITCODE)" }

$stream = [System.IO.File]::OpenWrite($testFile)
try {
    $marker = [System.Text.Encoding]::ASCII.GetBytes("TODO benchmark marker")
    foreach ($offset in @(0, [int64]($sizeBytes / 2), [int64]($sizeBytes - 4096))) {
        $stream.Seek($offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $stream.Write($marker, 0, $marker.Length)
    }
    $stream.Flush()
}
finally {
    $stream.Dispose()
}

# --- Run one operation, timed, with its peak working set -----------------
function Invoke-Bench {
    param(
        [string]$Name,
        [string[]]$Arguments,
        [int]$ExpectedExit
    )
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $exePath -ArgumentList $Arguments `
        -NoNewWindow -Wait -PassThru
    $sw.Stop()
    $peakMB = $null
    try {
        # The process object survives Wait because Start-Process was
        # given -PassThru; PeakWorkingSet64 is in bytes.
        $peakMB = [math]::Round($proc.PeakWorkingSet64 / 1MB, 1)
    }
    catch {
        $peakMB = "n/a"
    }
    $status = if ($proc.ExitCode -eq $ExpectedExit) { "ok" } else { "UNEXPECTED (want $ExpectedExit)" }
    [PSCustomObject]@{
        Operation    = $Name
        Milliseconds = [math]::Round($sw.Elapsed.TotalMilliseconds, 1)
        PeakMB       = $peakMB
        ExitCode     = $proc.ExitCode
        Status       = $status
    }
}

$results = @()
$results += Invoke-Bench "Cold launch (--help)" @("--help") 0
$results += Invoke-Bench "Find over $SizeGB GB" @($testFile, "--find", "TODO", "--headless", "--silent") 0
$results += Invoke-Bench "Save copy of $SizeGB GB" @($testFile, "--save-copy", $copyFile, "--silent") 0
$results += Invoke-Bench "Extract first 1 MB" @("extract", $testFile, "--start", "0", "--length", "1048576", "--out", $sliceFile, "--silent") 0

Write-Host ""
Write-Host "VeloEdit benchmark — file size $SizeGB GB ($sizeBytes bytes), exe: $exePath"
$results | Format-Table -AutoSize | Out-String | Write-Host

if (-not $Keep) {
    Remove-Item $workDir -Recurse -Force
    Write-Host "Removed the scratch folder $workDir (pass -Keep to keep the files)."
}
