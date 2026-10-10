$ErrorActionPreference = 'Stop'
$installRoot = Join-Path $env:LOCALAPPDATA 'CodexStudio'
$configPath = Join-Path $installRoot 'server.json'
$config = Get-Content -Raw -LiteralPath $configPath | ConvertFrom-Json
$env:CODEX_STUDIO_SOURCE_DIR = $config.sourceRoot
$env:CODEX_AGENTS_STATE_DIR = $config.stateDir
$env:CODEX_CANVAS_PUBLIC_ORIGIN = $config.publicOrigin
$env:PYTHONIOENCODING = 'utf-8'
$env:PATH = "$env:APPDATA\npm;D:\Git\cmd;C:\Program Files\nodejs;$env:PATH"
$entrypoint = Join-Path $installRoot 'codex_windows_server.py'
$log = Join-Path $installRoot 'runner.log'
& $config.python -B $entrypoint --source-root $config.sourceRoot `
    --state $config.stateDir --port $config.port --public-origin $config.publicOrigin `
    *> $log
exit $LASTEXITCODE
