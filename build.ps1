# Builds Pluma in release mode and puts the executable in .\dist\pluma.exe
# Requirements: rustup with the stable-x86_64-pc-windows-gnu toolchain (no Visual Studio needed).

$ErrorActionPreference = "Stop"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
# Windows limits paths to 260 characters: build in a short folder.
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\pluma-build"

Push-Location $PSScriptRoot
try {
    cargo build --release
    New-Item -ItemType Directory -Force dist | Out-Null
    Copy-Item "$env:CARGO_TARGET_DIR\release\pluma.exe" dist\pluma.exe -Force
    Write-Output "Done: $PSScriptRoot\dist\pluma.exe"
} finally {
    Pop-Location
}
