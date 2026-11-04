param([ValidateSet('x64', 'arm64')][string]$Arch)
$ErrorActionPreference = 'Stop'

$expectedHost = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$hostTriple = ((rustc -vV | Select-String '^host:').Line -split '\s+')[1]
if ($hostTriple -ne $expectedHost) { throw "Expected Rust host $expectedHost, got $hostTriple" }

$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq 'deving-panel').version
$binary = Join-Path $PWD 'target\release\deving-panel.exe'
$dist = Join-Path $PWD 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

$portable = Join-Path $env:RUNNER_TEMP 'devingPanel-portable'
New-Item -ItemType Directory -Force $portable | Out-Null
Copy-Item $binary, 'LICENSE' $portable
Compress-Archive -Path $portable -DestinationPath (Join-Path $dist "deving-panel_${version}_windows_${Arch}_portable.zip") -Force

$installer = Join-Path $env:RUNNER_TEMP 'innosetup.exe'
Invoke-WebRequest -Uri 'https://github.com/jrsoftware/issrc/releases/download/is-7_1_1/innosetup-7.1.1-x64.exe' -OutFile $installer
$innoDir = Join-Path $env:RUNNER_TEMP 'innosetup'
$process = Start-Process -FilePath $installer -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', "/DIR=`"$innoDir`"") -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Inno Setup installation failed: $($process.ExitCode)" }
$iscc = Join-Path $innoDir 'ISCC.exe'
if (-not (Test-Path $iscc)) { throw 'ISCC.exe was not installed' }

& $iscc "/DAppVersion=$version" "/DAppArch=$Arch" "/DSourceExe=$binary" "/DOutputDir=$dist" '.github\packaging\windows.iss'
if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed: $LASTEXITCODE" }
