<#
.SYNOPSIS
Downloads the prebuilt sherpa-onnx static libraries the build links against, checks their
SHA-256, extracts them under %LOCALAPPDATA%\butterfly-speak\vendor and prints the lib
directory. Point SHERPA_ONNX_LIB_DIR at that directory before building.

.PARAMETER GithubEnv
Also append SHERPA_ONNX_LIB_DIR=<dir> to $env:GITHUB_ENV (GitHub Actions).

.PARAMETER SetUserEnv
Also set SHERPA_ONNX_LIB_DIR for the current Windows user (new shells pick it up).
#>
[CmdletBinding()]
param([switch]$GithubEnv, [switch]$SetUserEnv)
$ErrorActionPreference = 'Stop'
# Windows PowerShell's progress bar slows Invoke-WebRequest down many times over.
$ProgressPreference = 'SilentlyContinue'
$Version = 'v1.13.3'
$Asset   = "sherpa-onnx-$Version-win-x64-static-MT-Release-lib.tar.bz2"
$Url     = "https://github.com/k2-fsa/sherpa-onnx/releases/download/$Version/$Asset"
$Sha256  = 'F6555701D6397D74F1302B0666A661F32708B599A14A5FDE80835D4902FCD315'
$Root    = Join-Path $env:LOCALAPPDATA 'butterfly-speak\vendor'
$Dir     = Join-Path $Root "sherpa-onnx-$Version"
$Lib     = Join-Path $Dir 'lib'
if (-not (Test-Path (Join-Path $Lib 'sherpa-onnx-c-api.lib'))) {
  New-Item -ItemType Directory -Force $Root | Out-Null
  $tmp = Join-Path $Root $Asset
  Invoke-WebRequest -Uri $Url -OutFile $tmp -UseBasicParsing
  $got = (Get-FileHash $tmp -Algorithm SHA256).Hash
  if ($got -ne $Sha256) { Remove-Item $tmp; throw "sherpa-onnx archive hash mismatch: $got" }
  # Windows' own bsdtar: a GNU tar found earlier on PATH (Git's) reads "C:" in a path
  # as a remote host name.
  & (Join-Path $env:SystemRoot 'System32\tar.exe') -xjf $tmp -C $Root
  if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE)" }
  $extracted = Join-Path $Root ($Asset -replace '\.tar\.bz2$', '')
  if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
  Rename-Item $extracted $Dir
  Remove-Item $tmp
}
# Out-File -Encoding utf8 writes a byte order mark in Windows PowerShell, which would
# become part of the first variable's name.
if ($GithubEnv)  {
  [IO.File]::AppendAllText($env:GITHUB_ENV, "SHERPA_ONNX_LIB_DIR=$Lib`n", (New-Object Text.UTF8Encoding $false))
}
if ($SetUserEnv) { [Environment]::SetEnvironmentVariable('SHERPA_ONNX_LIB_DIR', $Lib, 'User') }
Write-Output $Lib
