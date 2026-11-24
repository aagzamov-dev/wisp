[CmdletBinding()]
param(
    [string]$ReleaseDir = 'dist'
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

# Exercise the real default paths and registry without touching a developer installation.
if ($env:GITHUB_ACTIONS -cne 'true' -or $env:RUNNER_ENVIRONMENT -cne 'github-hosted') {
    throw 'Installer smoke tests require a disposable GitHub-hosted Windows runner'
}

$output = (Resolve-Path -LiteralPath $ReleaseDir).Path
$setup = Join-Path $output WispSetup.exe
$install = Join-Path $env:LOCALAPPDATA 'Programs/Wisp'
$data = Join-Path $env:APPDATA wisp
$programs = [Environment]::GetFolderPath('Programs')
$links = @{
    'wisp.exe' = Join-Path $programs 'Wisp.lnk'
    'wisp-settings.exe' = Join-Path $programs 'Wisp Settings.lnk'
}
$uninstallRoot = 'Software\Microsoft\Windows\CurrentVersion\Uninstall'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$scratch = (New-Item -ItemType Directory -Path (Join-Path $env:RUNNER_TEMP "wisp-setup-$([guid]::NewGuid())")).FullName
$shell = New-Object -ComObject WScript.Shell

function Get-WispRegistration {
    Get-ChildItem -LiteralPath "HKCU:\$uninstallRoot" -ErrorAction SilentlyContinue |
        Where-Object { $_.GetValue('DisplayName') -eq 'Wisp' }
}

function Get-WispAutorun {
    if (Test-Path -LiteralPath $runKey) {
        (Get-Item -LiteralPath $runKey).GetValue('Wisp')
    }
}

foreach ($path in (@($install, $data) + @($links.Values))) {
    if (Test-Path -LiteralPath $path) { throw "Unexpected existing Wisp path $path" }
}
if ((Get-WispRegistration) -or (Get-WispAutorun) -or (Get-Process wisp,wisp-settings -ErrorAction SilentlyContinue)) {
    throw 'Wisp is already installed, registered to start, or running'
}

function Assert-Hash($Path, $Expected) {
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
    if ($actual -ne $Expected) { throw "Unexpected SHA256 for $Path (got $actual)" }
}

function Invoke-Installer($Path, $Phase) {
    $log = Join-Path $scratch "$Phase.log"
    $process = Start-Process -FilePath $Path -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"" -PassThru -Wait
    if ($process.ExitCode -ne 0) {
        if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log | Out-Host }
        throw "$Phase failed with exit code $($process.ExitCode)"
    }
}

function Assert-Installed {
    foreach ($file in $links.Keys) {
        $exe = Join-Path $install $file
        Assert-Hash $exe $candidateHashes[$file]
        $link = $links[$file]
        if (-not (Test-Path -LiteralPath $link -PathType Leaf) -or $shell.CreateShortcut($link).TargetPath -ne $exe) {
            throw "Missing or unexpected Start Menu shortcut $link"
        }
    }
    $entry = @(Get-WispRegistration)
    if ($entry.Count -ne 1 -or $entry[0].GetValue('DisplayVersion') -ne $version -or
        (Test-Path -LiteralPath "HKLM:\$uninstallRoot\$($entry[0].PSChildName)")) {
        throw "Expected one per-user uninstall entry for Wisp $version"
    }
    if (Get-Process wisp,wisp-settings -ErrorAction SilentlyContinue) {
        throw 'Setup unexpectedly started Wisp or Settings'
    }
    $uninstaller = $entry[0].GetValue('UninstallString').Trim('"')
    if ((Split-Path -Parent $uninstaller) -ne $install -or
        (Split-Path -Leaf $uninstaller) -notmatch '^unins\d{3}\.exe$') {
        throw "Unexpected uninstaller path $uninstaller"
    }
    return $uninstaller
}

function Assert-Uninstalled {
    foreach ($file in $links.Keys) {
        if ((Test-Path -LiteralPath (Join-Path $install $file)) -or (Test-Path -LiteralPath $links[$file])) {
            throw "Uninstall left an executable or shortcut for $file"
        }
    }
    if (Get-WispRegistration) { throw 'Uninstall left a per-user registration' }
}

function Assert-DataPreserved {
    foreach ($file in $dataHashes.Keys) {
        Assert-Hash (Join-Path $data $file) $dataHashes[$file]
    }
}

$zip = @(Get-ChildItem -LiteralPath $output -Filter 'wisp-v*-x86_64-windows.zip' -File)
if ($zip.Count -ne 1 -or $zip[0].Name -cnotmatch '^wisp-v(\d+\.\d+\.\d+)-x86_64-windows\.zip$') {
    throw 'Expected exactly one versioned release ZIP'
}
$version = $Matches[1]
$assetNames = @{
    'wisp.exe' = "wisp-v$version-x86_64.exe"
    'wisp-settings.exe' = "wisp-settings-v$version-x86_64.exe"
}
$sums = @{}
foreach ($line in (Get-Content -LiteralPath (Join-Path $output SHA256SUMS.txt))) {
    if ($line -cnotmatch '^([A-F0-9]{64})  ([^ ]+)$') { throw "Invalid checksum line $line" }
    $sums.Add($Matches[2], $Matches[1])
}
foreach ($asset in @($zip[0].Name, 'WispSetup.exe') + @($assetNames.Values)) {
    Assert-Hash (Join-Path $output $asset) $sums[$asset]
}
$candidate = Join-Path $scratch candidate
Expand-Archive -LiteralPath $zip[0].FullName -DestinationPath $candidate
$candidateHashes = @{}
foreach ($file in $links.Keys) {
    $candidateHashes[$file] = $sums[$assetNames[$file]]
    Assert-Hash (Join-Path (Join-Path $candidate $zip[0].BaseName) $file) $candidateHashes[$file]
}

# Pin the actual published 0.7.0 ZIP from the fork served by its settings updater.
# Digest from https://api.github.com/repos/aagzamov-dev/wisp/releases/tags/v0.7.0.
$baselineName = 'wisp-v0.7.0-x86_64-windows.zip'
$baselineZip = Join-Path $scratch $baselineName
Invoke-WebRequest -Uri "https://github.com/aagzamov-dev/wisp/releases/download/v0.7.0/$baselineName" -OutFile $baselineZip
Assert-Hash $baselineZip 'ba5ec5b6cc4e0c1fd4fce57405e7619e902fb5ac68220b195faf4b669c169958'
$baseline = Join-Path $scratch baseline
Expand-Archive -LiteralPath $baselineZip -DestinationPath $baseline

Write-Host 'Checking clean installation and uninstall'
Invoke-Installer $setup clean-install
$uninstaller = Assert-Installed
if ((Test-Path -LiteralPath $data) -or (Get-WispAutorun)) {
    throw 'Clean setup unexpectedly created settings or enabled Start with Windows'
}
Invoke-Installer $uninstaller clean-uninstall
Assert-Uninstalled

Write-Host 'Checking upgrade over the published v0.7.0 ZIP'
New-Item -ItemType Directory -Path $install, $data -Force | Out-Null
foreach ($file in $links.Keys) {
    $oldExe = @(Get-ChildItem -LiteralPath $baseline -Filter $file -File -Recurse)
    if ($oldExe.Count -ne 1 -or (Get-FileHash -LiteralPath $oldExe[0].FullName).Hash -eq $candidateHashes[$file]) {
        throw "Expected one distinct v0.7.0 $file"
    }
    Copy-Item -LiteralPath $oldExe[0].FullName -Destination $install
}
Set-Content -LiteralPath (Join-Path $data config.toml) -Encoding utf8 -Value 'dock_autohide = false', 'theme = "dark"'
Set-Content -LiteralPath (Join-Path $data dock-pins.toml) -Encoding utf8 -Value '[[apps]]', 'id = ''C:\Windows\notepad.exe''', 'name = "Notepad"', 'path = ''C:\Windows\notepad.exe'''
Set-Content -LiteralPath (Join-Path $data start-pins.toml) -Encoding utf8 -Value 'ids = ["Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"]'
$dataHashes = @{}
foreach ($file in 'config.toml', 'dock-pins.toml', 'start-pins.toml') {
    $dataHashes[$file] = (Get-FileHash -LiteralPath (Join-Path $data $file) -Algorithm SHA256).Hash
}
$runValue = '"' + (Join-Path $install wisp.exe) + '"'
if (-not (Test-Path -LiteralPath $runKey)) { New-Item -Path $runKey | Out-Null }
Set-ItemProperty -LiteralPath $runKey -Name Wisp -Value $runValue
Invoke-Installer $setup upgrade
$uninstaller = Assert-Installed
Assert-DataPreserved
if ((Get-WispAutorun) -ne $runValue) { throw 'Upgrade changed Start with Windows' }
Write-Host 'v0.7.0 upgrade passed'

Write-Host 'Checking uninstall after upgrade'
Invoke-Installer $uninstaller upgrade-uninstall
Assert-Uninstalled
Assert-DataPreserved
if (Get-WispAutorun) { throw 'Uninstall left Start with Windows pointing to the removed launcher' }
Write-Host 'Uninstall after upgrade passed'
