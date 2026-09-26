[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Executable,

    [string]$OutputPath = (Join-Path (Get-Location) 'quicksearch-startup.csv'),

    [ValidateRange(5, 3600)]
    [int]$SampleSeconds = 60,

    [ValidateRange(50, 5000)]
    [int]$IntervalMilliseconds = 250,

    [switch]$StopAfterSample,

    [switch]$StartupTrace
)

$ErrorActionPreference = 'Stop'
$resolvedExecutable = (Resolve-Path -LiteralPath $Executable).Path
$outputParent = Split-Path -Parent $OutputPath
if ($outputParent -and -not (Test-Path -LiteralPath $outputParent)) {
    New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
}

# This is intentionally a process-level measurement. It does not clear the
# Windows file cache, checkpoint SQLite, or walk any configured roots itself.
# Run it once for a cold-ish launch and again for a warm launch, then compare
# the two CSV files. Endpoint protection and other background processes remain
# part of the observation rather than being disabled or hidden.
$startedAt = Get-Date
$previousStartupTrace = $env:QUICKSEARCH_STARTUP_TRACE
$tracePath = Join-Path $env:TEMP ("quicksearch-startup-trace-{0}.log" -f ([guid]::NewGuid()))
if ($StartupTrace) {
    $env:QUICKSEARCH_STARTUP_TRACE = $tracePath
}
try {
    $process = Start-Process -FilePath $resolvedExecutable -PassThru
} finally {
    if ($null -eq $previousStartupTrace) {
        Remove-Item Env:QUICKSEARCH_STARTUP_TRACE -ErrorAction SilentlyContinue
    } else {
        $env:QUICKSEARCH_STARTUP_TRACE = $previousStartupTrace
    }
}
$samples = [System.Collections.Generic.List[object]]::new()
$deadline = $startedAt.AddSeconds($SampleSeconds)

while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds $IntervalMilliseconds
    try {
        $process.Refresh()
        if ($process.HasExited) { break }

        $samples.Add([pscustomobject]@{
            timestamp = (Get-Date).ToUniversalTime().ToString('o')
            elapsed_ms = [math]::Round(((Get-Date) - $startedAt).TotalMilliseconds, 0)
            process_id = $process.Id
            working_set_bytes = $process.WorkingSet64
            private_bytes = $process.PrivateMemorySize64
            paged_memory_bytes = $process.PagedMemorySize64
            io_read_operations = $process.ReadOperationCount
            io_write_operations = $process.WriteOperationCount
            io_other_operations = $process.OtherOperationCount
            cpu_seconds = $process.TotalProcessorTime.TotalSeconds
        })
    } catch [System.ComponentModel.Win32Exception] {
        # The process can disappear between Refresh and property access.
        break
    }
}

if ($StopAfterSample -and -not $process.HasExited) {
    Stop-Process -Id $process.Id -Force
}

if ($samples.Count -eq 0) {
    throw "No samples were collected from $resolvedExecutable. The process may have exited before the first sample."
}

$samples | Export-Csv -LiteralPath $OutputPath -NoTypeInformation
$first = $samples[0]
$last = $samples[$samples.Count - 1]
Write-Host ("Collected {0} samples for PID {1}." -f $samples.Count, $first.process_id)
Write-Host ("Peak working set: {0:N0} MB; read operations: {1:N0}; write operations: {2:N0}." -f `
    (($samples | Measure-Object -Property working_set_bytes -Maximum).Maximum / 1MB),
    ($last.io_read_operations - $first.io_read_operations),
    ($last.io_write_operations - $first.io_write_operations))
Write-Host "Wrote $OutputPath"
if ($StartupTrace) {
    if (Test-Path -LiteralPath $tracePath) {
        Write-Host "Startup trace:"
        Get-Content -LiteralPath $tracePath
    } else {
        Write-Warning "Startup trace was not written; an existing single instance may have handled the launch."
    }
}
