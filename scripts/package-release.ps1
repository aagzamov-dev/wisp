[CmdletBinding()]
param(
    [string]$OutputDir = 'dist'
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$root = Split-Path -Parent $PSScriptRoot
$output = (New-Item -ItemType Directory -Path $OutputDir -Force).FullName
Push-Location $root
try {
    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -CEQ 'wisp').version
    $settingsVersion = ($metadata.packages | Where-Object name -CEQ 'wisp-settings').version
    if ($version -cnotmatch '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z' -or
        $version -cne $settingsVersion) {
        throw 'Expected matching stable versions for wisp and wisp-settings'
    }

    $payload = Join-Path $metadata.target_directory 'release'
    $name = "wisp-v$version-x86_64-windows"
    $staging = New-Item -ItemType Directory -Path (Join-Path $output $name)
    Copy-Item -LiteralPath (Join-Path $payload wisp.exe), (Join-Path $payload wisp-settings.exe), README.md, LICENSE -Destination $staging.FullName
    Compress-Archive -LiteralPath $staging.FullName -DestinationPath (Join-Path $output "$name.zip")
    Remove-Item -LiteralPath (Join-Path $staging.FullName wisp.exe), (Join-Path $staging.FullName wisp-settings.exe), (Join-Path $staging.FullName README.md), (Join-Path $staging.FullName LICENSE)
    Remove-Item -LiteralPath $staging.FullName

    Copy-Item -LiteralPath (Join-Path $payload wisp.exe) -Destination (Join-Path $output "wisp-v$version-x86_64.exe")
    Copy-Item -LiteralPath (Join-Path $payload wisp-settings.exe) -Destination (Join-Path $output "wisp-settings-v$version-x86_64.exe")

    $compiler = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6/ISCC.exe'
    if (-not (Test-Path -LiteralPath $compiler -PathType Leaf)) {
        $compiler = (Get-Command ISCC.exe -CommandType Application -ErrorAction Stop).Source
    }
    & $compiler "/DAppVersion=$version" "/DPayloadDir=$payload" "/DOutputDir=$output" 'installer/Wisp.iss'

    $files = "$name.zip", "wisp-v$version-x86_64.exe", "wisp-settings-v$version-x86_64.exe", 'WispSetup.exe'
    $files | ForEach-Object {
        (Get-FileHash -LiteralPath (Join-Path $output $_) -Algorithm SHA256).Hash + "  $_"
    } | Set-Content -LiteralPath (Join-Path $output SHA256SUMS.txt) -Encoding ascii
}
finally {
    Pop-Location
}
