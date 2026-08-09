param(
    [string]$DirigentPath = (Join-Path $PSScriptRoot "dirigent-diagnostic.exe")
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

function Write-Step([string]$Message) {
    Write-Host "`n==> $Message" -ForegroundColor Cyan
}

function Capture-ImmediateDump(
    [string]$ProcDump,
    [int]$ProcessId,
    [string]$OutputPath
) {
    & $ProcDump -accepteula -mm $ProcessId $OutputPath
    if ($LASTEXITCODE -ne 0) {
        throw "ProcDump failed with exit code $LASTEXITCODE while writing $OutputPath"
    }
}

$DirigentPath = [IO.Path]::GetFullPath($DirigentPath)
if (-not (Test-Path -LiteralPath $DirigentPath -PathType Leaf)) {
    throw "Diagnostic executable not found: $DirigentPath"
}
if (Get-Process -Name "dirigent*" -ErrorAction SilentlyContinue) {
    throw "Close all existing Dirigent windows before starting diagnostics."
}

Write-Host "Dirigent hang diagnostics" -ForegroundColor Cyan
Write-Host "Keep this window open and use Dirigent normally."

$toolRoot = Join-Path $env:LOCALAPPDATA "DirigentDiagnostics\tools"
$procDump = Join-Path $toolRoot "procdump64.exe"
if (-not (Test-Path -LiteralPath $procDump -PathType Leaf)) {
    Write-Step "Downloading Microsoft Sysinternals ProcDump"
    New-Item -ItemType Directory -Path $toolRoot -Force | Out-Null
    $archive = Join-Path $toolRoot "Procdump.zip"
    Invoke-WebRequest -Uri "https://download.sysinternals.com/files/Procdump.zip" -OutFile $archive
    Expand-Archive -LiteralPath $archive -DestinationPath $toolRoot -Force
}

$signature = Get-AuthenticodeSignature -LiteralPath $procDump
if ($signature.Status.ToString() -ne "Valid" -or
    $signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
    throw "ProcDump does not have a valid Microsoft signature. Status: $($signature.Status)"
}

$startedAt = Get-Date
$stamp = $startedAt.ToUniversalTime().ToString("yyyyMMdd_HHmmss")
$captureRoot = Join-Path $env:LOCALAPPDATA "DirigentDiagnostics\captures\$stamp"
New-Item -ItemType Directory -Path $captureRoot -Force | Out-Null

Write-Step "Starting diagnostic Dirigent"
$process = Start-Process -FilePath $DirigentPath -PassThru
$manifest = @(
    "capture_started_utc=$($startedAt.ToUniversalTime().ToString('o'))"
    "process_id=$($process.Id)"
    "executable=$DirigentPath"
    "executable_sha256=$((Get-FileHash -LiteralPath $DirigentPath -Algorithm SHA256).Hash.ToLowerInvariant())"
    "windows=$([Environment]::OSVersion.VersionString)"
) -join "`r`n"
$manifest | Set-Content -LiteralPath (Join-Path $captureRoot "manifest.txt") -Encoding UTF8

try {
    Get-CimInstance Win32_VideoController |
        Select-Object Name, DriverVersion, DriverDate, AdapterRAM, Status |
        Format-List |
        Out-File -LiteralPath (Join-Path $captureRoot "display-adapters.txt") -Encoding UTF8
} catch {
    "Could not collect display adapter information: $_" |
        Set-Content -LiteralPath (Join-Path $captureRoot "display-adapters.txt") -Encoding UTF8
}

Write-Step "Waiting for a window hang of at least five seconds"
Write-Host "Use Dirigent normally. Leave this window open. Dumps will be automatic."
$firstDump = Join-Path $captureRoot "hang-1.dmp"
& $procDump -accepteula -mm -h -n 1 $process.Id $firstDump
$monitorExit = $LASTEXITCODE
$process.Refresh()
if ($monitorExit -ne 0 -or $process.HasExited) {
    if ($process.HasExited) {
        throw "Dirigent exited before a hang dump was captured."
    }
    throw "ProcDump hang monitor stopped with exit code $monitorExit."
}

Write-Step "First hang captured; collecting two follow-up stack snapshots"
Start-Sleep -Seconds 2
$process.Refresh()
if (-not $process.HasExited) {
    Capture-ImmediateDump $procDump $process.Id (Join-Path $captureRoot "hang-2.dmp")
}
Start-Sleep -Seconds 2
$process.Refresh()
if (-not $process.HasExited) {
    Capture-ImmediateDump $procDump $process.Id (Join-Path $captureRoot "hang-3.dmp")
}

$logDirectory = Join-Path $env:APPDATA "dirigent\v0\logs"
try {
    if (Test-Path -LiteralPath $logDirectory -PathType Container) {
        $logDestination = Join-Path $captureRoot "logs"
        New-Item -ItemType Directory -Path $logDestination -Force | Out-Null
        Get-ChildItem -LiteralPath $logDirectory -Filter "*.log" -File |
            Where-Object { $_.LastWriteTime -ge $startedAt.AddMinutes(-1) } |
            Copy-Item -Destination $logDestination
    }
} catch {
    "Could not copy application logs: $_" |
        Set-Content -LiteralPath (Join-Path $captureRoot "log-copy-error.txt") -Encoding UTF8
}

$desktop = [Environment]::GetFolderPath([Environment+SpecialFolder]::Desktop)
$zipPath = Join-Path $desktop "dirigent-hang-$stamp.zip"
if (Test-Path -LiteralPath $zipPath) {
    Remove-Item -LiteralPath $zipPath -Force
}
Compress-Archive -Path (Join-Path $captureRoot "*") -DestinationPath $zipPath -CompressionLevel Optimal

Write-Step "Diagnostics captured"
Write-Host "Send this ZIP to the Dirigent developers:" -ForegroundColor Green
Write-Host $zipPath -ForegroundColor Yellow

Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.MessageBox]::Show(
    "The Dirigent freeze was captured. Send this file to the developer:`n`n$zipPath",
    "Dirigent diagnostics captured",
    [System.Windows.Forms.MessageBoxButtons]::OK,
    [System.Windows.Forms.MessageBoxIcon]::Information
) | Out-Null
