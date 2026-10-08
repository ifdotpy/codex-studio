param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('StopBackend', 'RestartBackend', 'StopAll')]
    [string]$Action,
    [string]$SourceRoot
)

$ErrorActionPreference = 'Stop'
$installRoot = Join-Path $env:LOCALAPPDATA 'CodexStudio'
$configPath = Join-Path $installRoot 'server.json'
$entrypoint = Join-Path $installRoot 'codex_windows_server.py'
$config = Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
$env:CODEX_STUDIO_SOURCE_DIR = $config.sourceRoot
$env:CODEX_AGENTS_STATE_DIR = $config.stateDir

if ($Action -eq 'RestartBackend') {
    if (-not $SourceRoot) { throw 'RestartBackend requires -SourceRoot.' }
    $SourceRoot = (Resolve-Path -LiteralPath $SourceRoot).Path
    if (-not (Test-Path -LiteralPath (Join-Path $SourceRoot 'scripts\codex_canvas.py'))) {
        throw "SourceRoot is not a Studio source tree: $SourceRoot"
    }
    $config.sourceRoot = $SourceRoot
    $temporaryConfig = "$configPath.tmp"
    $config | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 -LiteralPath $temporaryConfig
    Move-Item -Force -LiteralPath $temporaryConfig -Destination $configPath
}

$requestAction = switch ($Action) {
    'StopBackend' { 'stop-backend' }
    'RestartBackend' { 'restart-backend' }
    'StopAll' { 'stop-all' }
}
$arguments = @('-B', $entrypoint, '--state', $config.stateDir,
    '--source-root', $config.sourceRoot, '--request-action', $requestAction)
if ($Action -eq 'RestartBackend') {
    $arguments += @('--request-source-root', $SourceRoot)
}
$output = & $config.python @arguments
if ($LASTEXITCODE -ne 0) { throw "The server control request failed with exit code $LASTEXITCODE" }
$request = $output | ConvertFrom-Json
$resultPath = Join-Path $config.stateDir "windows-server-control-$($request.requestId).json"
$deadline = (Get-Date).AddSeconds(45)
do {
    if (Test-Path -LiteralPath $resultPath) { break }
    Start-Sleep -Milliseconds 200
} while ((Get-Date) -lt $deadline)
if (-not (Test-Path -LiteralPath $resultPath)) {
    throw "The server did not confirm control request $($request.requestId). Check runner.log and state\backend.log."
}
$result = Get-Content -Raw -LiteralPath $resultPath | ConvertFrom-Json
if ($result.requestId -ne $request.requestId) { throw 'The server control receipt does not match the request.' }
if ($Action -eq 'StopBackend' -and $result.result -ne 'backend-stopped') {
    throw "Unexpected stop result: $($result.result)"
}
if ($Action -eq 'RestartBackend' -and $result.result -ne 'backend-restarted') {
    throw "Unexpected restart result: $($result.result)"
}
if ($Action -eq 'StopAll') {
    if ($result.result -ne 'stopped-all') { throw "Unexpected stop result: $($result.result)" }
    Unregister-ScheduledTask -TaskName 'Codex Studio Native Server' -Confirm:$false -ErrorAction SilentlyContinue
}
if ($Action -eq 'RestartBackend') {
    $deadline = (Get-Date).AddSeconds(60)
    $backendFound = $false
    do {
        $listener = Get-NetTCPConnection -State Listen -LocalPort $config.port -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($listener) {
            $process = Get-CimInstance Win32_Process -Filter "ProcessId = $($listener.OwningProcess)"
            if ($process.CommandLine.Contains($SourceRoot)) { $backendFound = $true; break }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    if (-not $backendFound) { throw "The updated backend did not listen on port $($config.port)." }
}
if ($Action -ne 'RestartBackend') {
    $deadline = (Get-Date).AddSeconds(20)
    do {
        $listener = Get-NetTCPConnection -State Listen -LocalPort $config.port -ErrorAction SilentlyContinue
        if (-not $listener) { break }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)
    if ($listener) { throw "The backend still listens on port $($config.port)." }
}
Write-Output "Windows server action '$Action' completed. Request $($request.requestId)."
