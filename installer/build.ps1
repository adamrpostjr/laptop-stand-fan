# Build the FanStand MSI.
#
#   pwsh installer\build.ps1
#
# Needs: Rust, and WiX v5 as a .NET tool:
#   dotnet tool install --global wix --version 5.0.2
#   wix extension add -g WixToolset.Util.wixext/5.0.2
# (v5 on purpose: v6+ asks you to accept the WiX OSMF EULA first.)
# Output: installer\out\FanStand-<version>-x64.msi

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $PSScriptRoot 'out'
New-Item -ItemType Directory -Force $out | Out-Null

Push-Location $root
try {
    cargo build --release -p fanstand-controller -p fanstand-app
    if ($LASTEXITCODE) { throw 'cargo build failed' }

    $version = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).packages |
        Where-Object name -eq 'fanstand-controller' | Select-Object -ExpandProperty version
    $bin = Join-Path $root 'target\release'
    $icon = Join-Path $out 'fanstand.ico'

    # The app draws its own icon; have it write the .ico for the installer.
    & (Join-Path $bin 'fanstand-app.exe') --write-icon $icon | Out-Null
    if (-not (Test-Path $icon)) { throw 'icon was not written' }

    $msi = Join-Path $out "FanStand-$version-x64.msi"
    wix build (Join-Path $PSScriptRoot 'fanstand.wxs') -arch x64 -ext WixToolset.Util.wixext `
        -d "Version=$version" -d "BinDir=$bin" -d "IconFile=$icon" -o $msi
    if ($LASTEXITCODE) { throw 'wix build failed' }
    Write-Host "built $msi"
}
finally {
    Pop-Location
}
