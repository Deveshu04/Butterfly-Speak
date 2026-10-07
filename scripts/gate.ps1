<#
.SYNOPSIS
Runs every check: cargo test --lib, cargo test --bins and cargo check --all-targets in
src-tauri (the check fails on any warning), then pnpm check and pnpm test when node_modules exists,
then the uninstall hook's harness when Tauri's makensis is present. Each step's full output goes to
a log dir, and one summary line per step is printed.

.PARAMETER LogDir
Where the logs go. Default: %TEMP%\bs-gate-<timestamp>.

.NOTES
Run it from anywhere inside the checkout, with SHERPA_ONNX_LIB_DIR set (see
scripts\vendor-sherpa.ps1). The first run on an empty target dir builds everything: 30
minutes or more and about 10 GB of disk. Later runs take a few minutes. Exit code: the first
failing step's, else 0.
#>
[CmdletBinding()]
param([string]$LogDir)

$ErrorActionPreference = 'Stop'
$root = (& git rev-parse --show-toplevel 2>$null)
if (-not $root) { Write-Host "gate: not inside a git checkout" -ForegroundColor Red; exit 2 }
$root = $root.Trim()
$rootWin = $root -replace '/', '\'
$crate = Join-Path $rootWin 'src-tauri'
if (-not $LogDir) { $LogDir = Join-Path $env:TEMP ("bs-gate-" + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force $LogDir | Out-Null
$t0 = Get-Date

$rust = 0
$n = 0
foreach ($step in 'test --lib', 'test --bins', 'check --all-targets') {
  $n++
  $argv = @($step -split ' ')
  $slug = ($step -replace '--', '') -replace ' ', '-'
  $out = Join-Path $LogDir ("{0}-cargo-{1}.out" -f $n, $slug)
  $err = Join-Path $LogDir ("{0}-cargo-{1}.err" -f $n, $slug)
  $s0 = Get-Date
  # Not -Wait: Windows PowerShell then also waits for every process the build
  # started, and the MSBuild and vctip.exe processes of a cold C++ build outlive
  # cargo by many minutes. Reading Handle before the exit keeps ExitCode
  # available; without it PowerShell 5.1 reports null, which reads as success.
  $p = Start-Process -FilePath 'cargo' -ArgumentList $argv -WorkingDirectory $crate -NoNewWindow -PassThru `
         -RedirectStandardOutput $out -RedirectStandardError $err
  $null = $p.Handle
  $p.WaitForExit()
  $code = $p.ExitCode
  $secs = [int]((Get-Date) - $s0).TotalSeconds
  if ($argv[0] -eq 'test') {
    $detail = ((Select-String -Path $out -Pattern '^test result:' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }) -join ' | ')
    if (-not $detail) { $detail = 'NO "test result:" LINE (build failure or crash; read the .err file)' }
  } else {
    # A log that cannot be read must fail the step: an empty result from an
    # unread file would print warnings=0 and pass.
    try {
      $warnings = @(Select-String -Path $out, $err -Pattern '^warning:' -ErrorAction Stop |
                    Where-Object { $_.Line -notmatch 'generated \d+ warning' })
      foreach ($w in $warnings) { Write-Host ("  " + $w.Line) -ForegroundColor Yellow }
      if ($warnings.Count -gt 0 -and $code -eq 0) { $code = 1 }
      $detail = "warnings=$($warnings.Count)"
    } catch {
      if ($code -eq 0) { $code = 1 }
      $detail = "WARNING SCAN FAILED: $($_.Exception.Message)"
    }
  }
  Write-Host ("gate: [{0}] cargo {1} -> exit {2} in {3} s :: {4}" -f $n, $step, $code, $secs, $detail)
  if ($code -ne 0 -and $rust -eq 0) { $rust = $code }
}

$fe = 0
if (Test-Path (Join-Path $rootWin 'node_modules')) {
  $out = Join-Path $LogDir '4-pnpm-check.out'
  $err = Join-Path $LogDir '4-pnpm-check.err'
  # This project's `pnpm check` prints svelte-check's machine line
  # (`COMPLETED N FILES E ERRORS ...`), never the human `svelte-check found ...`
  # sentence.
  $p = Start-Process -FilePath 'cmd' -ArgumentList '/c', 'pnpm', 'check' -WorkingDirectory $rootWin -NoNewWindow -PassThru `
         -RedirectStandardOutput $out -RedirectStandardError $err
  $null = $p.Handle
  $p.WaitForExit()
  $fe = $p.ExitCode
  $found = ((Select-String -Path $out, $err -Pattern 'COMPLETED .* FILES' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }) -join ' | ')
  if (-not $found) { $found = 'NO "COMPLETED ... FILES" LINE (read the .out/.err files)' }
  Write-Host ("gate: [4] pnpm check -> exit {0} :: {1}" -f $fe, $found)

  # The webview's pure modules (tests/web), on node's own test runner.
  $out = Join-Path $LogDir '5-pnpm-test.out'
  $err = Join-Path $LogDir '5-pnpm-test.err'
  $p = Start-Process -FilePath 'cmd' -ArgumentList '/c', 'pnpm', 'test' -WorkingDirectory $rootWin -NoNewWindow -PassThru `
         -RedirectStandardOutput $out -RedirectStandardError $err
  $null = $p.Handle
  $p.WaitForExit()
  $code = $p.ExitCode
  $found = ((Select-String -Path $out -Pattern '\b(pass|fail) \d+\s*$' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }) -join ' | ')
  if (-not $found) { $found = 'NO pass/fail LINES (read the .out/.err files)' }
  Write-Host ("gate: [5] pnpm test -> exit {0} :: {1}" -f $code, $found)
  if ($code -ne 0 -and $fe -eq 0) { $fe = $code }
} else {
  Write-Host "gate: [4] pnpm check skipped (no node_modules in this checkout; run 'pnpm install' first)"
}

# The uninstaller's hook, against throwaway folders, credentials and a
# registry key (src-tauri\windows\test-hooks.ps1). It needs the makensis
# Tauri downloads on its first bundle, so it is skipped until then.
$hk = 0
$makensis = Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe'
if (Test-Path $makensis) {
  $out = Join-Path $LogDir '6-uninstall-hook.out'
  $err = Join-Path $LogDir '6-uninstall-hook.err'
  # Quoted: Start-Process joins the list with spaces, and the checkout path
  # may hold one.
  $hookScript = '"' + (Join-Path $crate 'windows\test-hooks.ps1') + '"'
  $p = Start-Process -FilePath 'powershell' -ArgumentList '-NoProfile', '-File', $hookScript `
         -WorkingDirectory $rootWin -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
  $null = $p.Handle
  $p.WaitForExit()
  $hk = $p.ExitCode
  $found = ((Select-String -Path $out -Pattern '^(all cases passed|\d+ case\(s\) failed)' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }) -join ' | ')
  if (-not $found) { $found = 'NO RESULT LINE (read the .out/.err files)' }
  Write-Host ("gate: [6] uninstall hook harness -> exit {0} :: {1}" -f $hk, $found)
} else {
  Write-Host "gate: [6] uninstall hook harness skipped (no makensis at $makensis; bundle the app once first)"
}
if ($hk -ne 0 -and $fe -eq 0) { $fe = $hk }

$secs = [int]((Get-Date) - $t0).TotalSeconds
Write-Host ("gate: done in {0} s; logs in {1}" -f $secs, $LogDir)
if ($rust -ne 0) { exit $rust }
exit $fe
