# Start the local ReOpenSCAD server (web UI + /mcp).
# Usage:  pwsh -File scripts/start-server.ps1
#         scripts\start-server.cmd

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Manifest = Join-Path $RepoRoot 'web\backend\Cargo.toml'
$HostName = if ($env:REOPENSCAD_HOST) { $env:REOPENSCAD_HOST } else { '127.0.0.1' }
$Port = if ($env:REOPENSCAD_PORT) { [int]$env:REOPENSCAD_PORT } else { 5173 }
$BaseUrl = "http://${HostName}:${Port}"
$HealthUrl = "$BaseUrl/api/health"
$McpUrl = "$BaseUrl/mcp"

function Test-ServerUp {
    try {
        $response = Invoke-WebRequest -Uri $HealthUrl -UseBasicParsing -TimeoutSec 2
        return $response.StatusCode -eq 200
    } catch {
        return $false
    }
}

if (-not (Test-Path $Manifest)) {
    Write-Error "Cargo manifest not found: $Manifest"
}

if (Test-ServerUp) {
    Write-Host "ReOpenSCAD already running at $BaseUrl"
    Write-Host "MCP endpoint: $McpUrl"
    exit 0
}

Write-Host "Starting ReOpenSCAD at $BaseUrl ..."
Write-Host "MCP endpoint: $McpUrl"
Write-Host "Building/running release server (first launch may take a while)."
Write-Host ""

Set-Location $RepoRoot
& cargo run --manifest-path $Manifest --release -- --host $HostName --port $Port
exit $LASTEXITCODE
