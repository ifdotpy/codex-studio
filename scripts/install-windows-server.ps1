param(
    [string]$SourceRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$Python = (Join-Path $env:USERPROFILE 'studio-dev\venv\Scripts\python.exe')
)

$ErrorActionPreference = 'Stop'
$installRoot = Join-Path $env:LOCALAPPDATA 'CodexStudio'
$stateDir = Join-Path $installRoot 'state'
$origin = 'https://kukuka-win.tailf00fa0.ts.net:8443'
$taskName = 'Codex Studio Native Server'
$runnerPath = Join-Path $installRoot 'start-windows-server.ps1'
$managerPath = Join-Path $installRoot 'manage-windows-server.ps1'
$entrypointPath = Join-Path $installRoot 'codex_windows_server.py'
$sourceRoot = (Resolve-Path -LiteralPath $SourceRoot).Path
$python = (Resolve-Path -LiteralPath $Python).Path

function Get-ServeProxy($ServeStatus, [string]$HostName, [int]$Port) {
    if (-not $ServeStatus.Web) { return $null }
    $binding = $ServeStatus.Web.PSObject.Properties["${HostName}:$Port"]
    if (-not $binding) { return $null }
    $handlers = $binding.Value.Handlers
    if (-not $handlers) { return $null }
    $root = $handlers.PSObject.Properties['/']
    if (-not $root) { return $null }
    return $root.Value.Proxy
}

if (-not (Test-Path -LiteralPath (Join-Path $sourceRoot 'scripts\codex_canvas.py'))) {
    throw "Source root has no scripts\codex_canvas.py: $sourceRoot"
}
if (-not (Test-Path -LiteralPath $python)) {
    throw "Python was not found: $python"
}

$tailscale = Get-Command tailscale.exe -ErrorAction Stop
$before = (& $tailscale.Source serve status --json | ConvertFrom-Json)
$serveHost = 'kukuka-win.tailf00fa0.ts.net'
$https443 = Get-ServeProxy $before $serveHost 443
if ($https443 -ne 'http://127.0.0.1:4720') {
    throw "Serve HTTPS 443 does not point to the existing WSL server. Found: $https443"
}
$https8443 = Get-ServeProxy $before $serveHost 8443
if ($https8443 -and $https8443 -ne 'http://127.0.0.1:4630') {
    throw "Serve HTTPS 8443 already has a different target. Found: $https8443"
}

New-Item -ItemType Directory -Force -Path $installRoot, $stateDir | Out-Null
Copy-Item -Force -LiteralPath (Join-Path $PSScriptRoot 'start-windows-server.ps1') -Destination $runnerPath
Copy-Item -Force -LiteralPath (Join-Path $PSScriptRoot 'manage-windows-server.ps1') -Destination $managerPath
Copy-Item -Force -LiteralPath (Join-Path $PSScriptRoot 'codex_windows_server.py') -Destination $entrypointPath
$config = [ordered]@{
    sourceRoot = $sourceRoot
    python = $python
    stateDir = $stateDir
    publicOrigin = $origin
    port = 4630
}
$config | ConvertTo-Json | Set-Content -Encoding utf8 -LiteralPath (Join-Path $installRoot 'server.json')

$userId = "$env:COMPUTERNAME\$env:USERNAME"
$action = New-ScheduledTaskAction -Execute (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe') `
    -Argument ("-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$runnerPath`"") `
    -WorkingDirectory $installRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $userId
$principal = New-ScheduledTaskPrincipal -UserId $userId -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew `
    -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 2)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $taskName

$deadline = (Get-Date).AddSeconds(45)
do {
    $listener = Get-NetTCPConnection -State Listen -LocalPort 4630 -ErrorAction SilentlyContinue
    if ($listener) { break }
    Start-Sleep -Milliseconds 500
} while ((Get-Date) -lt $deadline)
if (-not $listener) {
    throw 'The native server did not listen on 127.0.0.1:4630. Read runner.log and state\backend.log.'
}

& $tailscale.Source serve --bg --https=8443 http://127.0.0.1:4630
if ($LASTEXITCODE -ne 0) { throw "Tailscale Serve returned exit code $LASTEXITCODE" }
$after = (& $tailscale.Source serve status --json | ConvertFrom-Json)
$actual443 = Get-ServeProxy $after $serveHost 443
$actual8443 = Get-ServeProxy $after $serveHost 8443
if ($actual443 -ne 'http://127.0.0.1:4720' -or $actual8443 -ne 'http://127.0.0.1:4630') {
    throw "Serve verification failed. HTTPS 443=$actual443, HTTPS 8443=$actual8443"
}
Write-Output "Installed per-user scheduled task '$taskName'. API port 4630. Serve HTTPS 8443."
