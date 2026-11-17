$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$env:Path = "C:\Users\admin\scoop\persist\rustup\.cargo\bin;$env:Path"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not (Test-Path "$root\target\release\ai-gateway.exe")) {
    Write-Host "Building release binary..." -ForegroundColor Cyan
    cargo build --release --manifest-path "$root\Cargo.toml"
}

Set-Location $root
& "$root\target\release\ai-gateway.exe"
