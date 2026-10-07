# Proves src-tauri/windows/hooks.nsh against throwaway data, without running
# any uninstaller: compiles hooks-harness.nsi with makensis, points its bases
# at a fresh folder under %TEMP%, its credential names at a random test
# prefix and its registry keys at a random throwaway key, runs it once per
# case, and checks what is left.
#
#   powershell -File src-tauri\windows\test-hooks.ps1 [-MakeNsis <path>]
#
# Exit code 0 when every case holds; a case this machine cannot exercise
# prints SKIP and does not fail the run. Everything it creates (the temporary
# folder, a throwaway folder under C:\ProgramData for the 8.3 cases, the test
# credentials and the test registry key) is removed at the end, pass or fail.

param(
    [string]$MakeNsis = (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe')
)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
if (-not (Test-Path $MakeNsis)) { throw "makensis not found at $MakeNsis (build the app once so Tauri downloads NSIS, or pass -MakeNsis)" }

$tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
$tempBase = (Get-Item ([IO.Path]::GetTempPath())).FullName
$root = Join-Path $tempBase "bs-hook-harness-$tag"
$prefix = "bs-hook-harness-$tag-"
$regKey = "HKCU:\Software\bs-hook-harness-$tag"
$regProduct = "$regKey\Butterfly Speak"
$accounts = @('sarvam-api-key', 'custom-endpoint-key', 'cloud-refresh-token')
$me = '*' + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value

# Never let a mistake here point at the real data.
foreach ($real in @($env:APPDATA, $env:LOCALAPPDATA)) {
    if ($root.TrimEnd('\') -ieq $real.TrimEnd('\')) { throw "refusing: test root is a real data folder" }
}
if (-not $root.StartsWith($tempBase, [StringComparison]::OrdinalIgnoreCase)) { throw "refusing: test root is outside %TEMP%" }
if ($prefix -notmatch '^bs-hook-harness-[0-9a-f]{8}-$') { throw "refusing: unexpected credential prefix" }
if ($regKey -notmatch '^HKCU:\\Software\\bs-hook-harness-[0-9a-f]{8}$') { throw "refusing: unexpected registry key" }
# A throwaway folder under C:\ProgramData, for the 8.3 cases only: ProgramData
# has an 8.3 name on most machines (PROGRA~3), while no part of a path under
# %TEMP% may have one (none does on the machine this was written on).
$pdBase = (Get-Item -Force $env:ProgramData).FullName
$pdRoot = Join-Path $pdBase "bs-hook-harness-$tag"
if ((Split-Path $pdRoot -Leaf) -notmatch '^bs-hook-harness-[0-9a-f]{8}$' -or (Split-Path $pdRoot -Parent) -ne $pdBase) { throw "refusing: unexpected ProgramData folder" }

$roaming = Join-Path $root 'roaming'
$local = Join-Path $root 'local'
$temp = Join-Path $root 'temp'
$bin = Join-Path $root 'bin'
# Tauri's own folder name (the bundle identifier), and a converted import's
# name as ScratchWav mints it (import/mod.rs).
$tauri = 'com.butterflyspeak.app'
$wavName = 'bs-import-0f8c2d1e-3b4a-4c5d-8e9f-0a1b2c3d4e5f.wav'
$log = Join-Path $root 'hook.log'
$results = New-Object System.Collections.Generic.List[string]
$failures = 0
$locked = New-Object System.Collections.Generic.List[string]

function Target([string]$account) { "$prefix$account.ButterflySpeak" }

function Test-Credential([string]$target) {
    $out = & cmdkey.exe "/list:$target" | Out-String
    # cmdkey echoes the name it was asked about even when nothing matches, so
    # only a Target: line counts.
    return $out -match ('Target:[^\r\n]*' + [regex]::Escape($target))
}

function Remove-TestCredentials {
    foreach ($a in $accounts) {
        $t = Target $a
        if (Test-Credential $t) { & cmdkey.exe "/delete:$t" | Out-Null }
    }
}

Add-Type -Namespace BsHarness -Name K -MemberDefinition @'
[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
public static extern bool MoveFileW(string from, string to);
[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
public static extern uint GetFileAttributesW(string path);
[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
public static extern IntPtr CreateFileW(string path, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
[DllImport("kernel32.dll", SetLastError=true)]
public static extern bool DeviceIoControl(IntPtr h, uint code, byte[] inBuf, int inLen, IntPtr outBuf, int outLen, out int returned, IntPtr overlapped);
[DllImport("kernel32.dll", SetLastError=true)]
public static extern bool CloseHandle(IntPtr h);
[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
public static extern uint GetShortPathNameW(string path, System.Text.StringBuilder buf, uint len);
'@

# Exact-name helpers. A name ending in a space or a dot can only be made, seen
# or removed through a \\?\ path; plain Win32 paths strip those characters.
function Test-Exact([string]$path) { [BsHarness.K]::GetFileAttributesW("\\?\$path") -ne [uint32]::MaxValue }
function Move-Exact([string]$from, [string]$to) {
    if (-not [BsHarness.K]::MoveFileW("\\?\$from", "\\?\$to")) {
        throw "MoveFileW $from -> $to failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }
}
function New-Junction([string]$link, [string]$target) {
    $o = & cmd.exe /c "mklink /J `"$link`" `"$target`"" 2>&1
    if ($LASTEXITCODE -ne 0) { throw "mklink /J failed: $o" }
}

# A junction nobody can remove: DELETE denied on the link itself, and
# delete-child denied on its folder. Undone by Unlock-All.
function Lock-Junction([string]$link) {
    $parent = Split-Path $link -Parent
    & icacls.exe $link /L /deny "${me}:(DE)" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls deny on $link failed" }
    & icacls.exe $parent /deny "${me}:(DC)" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls deny on $parent failed" }
    $script:locked.Add($link)
}
function Unlock-All {
    foreach ($link in $script:locked) {
        & icacls.exe (Split-Path $link -Parent) /remove:d $me | Out-Null
        if (Test-Exact $link) { & icacls.exe $link /L /remove:d $me | Out-Null }
    }
    $script:locked.Clear()
}

# Read-only entries (links and their targets), made writable again by
# Clear-ReadOnly so the tree can be removed.
$readOnly = New-Object System.Collections.Generic.List[string]
function Set-ReadOnly([string]$path, [switch]$Link) {
    if ($Link) { & attrib.exe +R $path /L | Out-Null } else { & attrib.exe +R $path | Out-Null }
    if (([BsHarness.K]::GetFileAttributesW($path) -band 1) -eq 0) { throw "attrib +R on $path did not take" }
    $script:readOnly.Add($path)
}
function Test-ReadOnly([string]$path) { $a = [BsHarness.K]::GetFileAttributesW($path); ($a -ne [uint32]::MaxValue) -and (($a -band 1) -ne 0) }
function Clear-ReadOnly {
    foreach ($p in $script:readOnly) {
        if (Test-Exact $p) { & attrib.exe -R $p /L | Out-Null }
    }
    $script:readOnly.Clear()
}

# A symbolic link needs Developer Mode or the right to create one; returns
# $false when this machine does not allow it.
function New-Symlink([string]$link, [string]$target, [switch]$Directory) {
    $flag = if ($Directory) { '/D ' } else { '' }
    & cmd.exe /c "mklink $flag`"$link`" `"$target`"" 2>&1 | Out-Null
    return ($LASTEXITCODE -eq 0)
}

# An empty folder carrying a reparse point that is neither a junction nor a
# symbolic link (a made-up third-party tag, as a cloud-files or other filter
# folder carries its own). Without a filter behind the tag such a folder can
# only be empty, but the uninstaller must still refuse to vouch for it.
function New-OtherReparseFolder([string]$path) {
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    # GENERIC_WRITE | FILE_WRITE_ATTRIBUTES; OPEN_EXISTING;
    # FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT.
    $h = [BsHarness.K]::CreateFileW("\\?\$path", 0x40000100, 7, [IntPtr]::Zero, 3, 0x02200000, [IntPtr]::Zero)
    if ($h -eq [IntPtr](-1)) { throw "open $path failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
    try {
        # REPARSE_GUID_DATA_BUFFER: tag 0x1234, 4 bytes of data, a GUID.
        $buf = New-Object byte[] 28
        [BitConverter]::GetBytes([uint32]0x1234).CopyTo($buf, 0)
        [BitConverter]::GetBytes([uint16]4).CopyTo($buf, 4)
        ([guid]'4b8d4c33-3a5d-4e57-9b2a-1d4f1a7c9e01').ToByteArray().CopyTo($buf, 8)
        $n = 0
        # FSCTL_SET_REPARSE_POINT
        if (-not [BsHarness.K]::DeviceIoControl($h, 0x900A4, $buf, $buf.Length, [IntPtr]::Zero, 0, [ref]$n, [IntPtr]::Zero)) {
            throw "FSCTL_SET_REPARSE_POINT on $path failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
        }
    } finally { [BsHarness.K]::CloseHandle($h) | Out-Null }
}

function Remove-Tree([string]$path) {
    # rmdir /s does not follow junctions, so a junction's target survives. The
    # \\?\ form reaches names ending in a space or a dot.
    if (Test-Exact $path) { & cmd.exe /c "rmdir /s /q `"\\?\$path`"" | Out-Null }
}

function Remove-TestKey { if (Test-Path $regKey) { Remove-Item -LiteralPath $regKey -Recurse -Force } }

function New-Fixture {
    Unlock-All
    Clear-ReadOnly
    Remove-Tree $roaming
    Remove-Tree $local
    Remove-Tree $temp
    Remove-Tree (Join-Path $root 'outside')
    foreach ($p in @(
            "$roaming\ButterflySpeak\settings.json",
            "$roaming\ButterflySpeak\history.db",
            "$local\ButterflySpeak\logs\butterfly-speak.log.2026-09-28",
            "$local\ButterflySpeak\models\model.bin",
            "$roaming\$tauri\state.json",
            "$local\$tauri\EBWebView\Default\Local Storage\leveldb\000003.log",
            "$temp\$wavName",
            "$temp\other.wav",
            "$roaming\OtherApp\keep.txt",
            "$local\OtherApp\keep.txt")) {
        New-Item -ItemType File -Force -Path $p -Value 'test' | Out-Null
    }
    foreach ($a in $accounts) {
        & cmdkey.exe "/generic:$(Target $a)" "/user:bs-hook-harness" "/pass:throwaway-$tag" | Out-Null
    }
    # What Tauri's installer writes: the install location and the language.
    Remove-TestKey
    New-Item -Path $regProduct -Force | Out-Null
    Set-ItemProperty -LiteralPath $regProduct -Name '(default)' -Value "$root\install"
    Set-ItemProperty -LiteralPath $regProduct -Name 'Installer Language' -Value '1033'
    if (Test-Path $log) { Remove-Item -LiteralPath $log -Force }
}

# -Simulate also compiles in the harness's copy of Tauri's own block, which
# removes <base>\com.butterflyspeak.app; only for bases under the test root.
function Build-Harness([string]$roamingBase, [string]$localBase, [string]$tempBase, [string]$out, [switch]$Simulate) {
    $mkArgs = @(
        '/V2',
        "/DBS_ROAMING_BASE=$roamingBase",
        "/DBS_LOCAL_BASE=$localBase",
        "/DBS_TEMP_BASE=$tempBase",
        "/DBS_CRED_PREFIX=$prefix",
        "/DBS_REG_TAG=$tag",
        "/DBS_HOOK_LOG=$log",
        "/DBS_HARNESS_OUT=$out"
    )
    if ($Simulate) { $mkArgs += '/DBS_SIMULATE_TAURI_BLOCK=1' }
    $mkArgs += (Join-Path $here 'hooks-harness.nsi')
    $text = & $MakeNsis @mkArgs | Out-String
    if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE):`n$text" }
    $warn = ($text -split "`n" | Where-Object { $_ -match 'warning' }) -join "`n"
    return $warn
}

# A cancelled run ends in Abort, which exits non-zero; -AnyExit accepts that.
function Invoke-Harness([string]$exe, [string]$switches, [string]$cwd = $bin, [switch]$AnyExit) {
    $sp = @{ FilePath = $exe; WorkingDirectory = $cwd; Wait = $true; PassThru = $true; WindowStyle = 'Hidden' }
    if ($switches) { $sp.ArgumentList = $switches }
    $p = Start-Process @sp
    if ($p.ExitCode -ne 0 -and -not $AnyExit) { throw "harness exited with $($p.ExitCode)" }
    return $p.ExitCode
}

function Get-State {
    [ordered]@{
        roamingData  = Test-Exact "$roaming\ButterflySpeak"
        localData    = Test-Exact "$local\ButterflySpeak"
        roamingOther = Test-Path "$roaming\OtherApp\keep.txt"
        localOther   = Test-Path "$local\OtherApp\keep.txt"
        creds        = @($accounts | Where-Object { Test-Credential (Target $_) }).Count
        roamingTauri = Test-Exact "$roaming\$tauri"
        localTauri   = Test-Exact "$local\$tauri"
        importWav    = Test-Path "$temp\$wavName"
        otherTemp    = Test-Path "$temp\other.wav"
        reg          = Test-Path $regProduct
        tauriRan     = (Test-Path $log) -and ((Get-Content $log) -match "Tauri's block ran").Count -gt 0
    }
}

function Check([string]$name, [bool]$ok, [string]$detail) {
    $script:results.Add(("{0,-5} {1}  {2}" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $name, $detail))
    if (-not $ok) { $script:failures++ }
}
# A case this machine cannot exercise: neither a pass nor a failure.
function Skip([string]$name, [string]$detail) {
    $script:results.Add(("SKIP  {0}  {1}" -f $name, $detail))
}

function Describe($s) { "roaming=$($s.roamingData) local=$($s.localData) creds=$($s.creds)/3 siblings=$($s.roamingOther -and $s.localOther) tauri=$($s.roamingTauri)/$($s.localTauri) reg=$($s.reg) tauri-block-ran=$($s.tauriRan) import-wav=$($s.importWav) other-temp=$($s.otherTemp)" }
function Log-Line { '      log: ' + ((Get-Content $log | Where-Object { $_ -notmatch 'credential' }) -join ' | ') }

# Everything of the user's is gone, Tauri's block's work included (its
# folders and its registry keys), and nothing beside it.
function All-Gone($s) {
    (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.importWav) -and $s.otherTemp -and (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) -and $s.roamingOther -and $s.localOther -and (Test-Path $roaming) -and (Test-Path $local)
}

try {
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $exe = Join-Path $bin 'harness.exe'
    $warn = Build-Harness $roaming $local $temp $exe -Simulate
    if ($warn) { Check 'compile' $false "makensis warnings:`n$warn" } else { Check 'compile' $true 'makensis: no warnings' }

    # 1. Ticked: everything of the user's goes, and nothing beside it: both
    #    ButterflySpeak folders, the credentials, a converted import left in
    #    %TEMP%, both com.butterflyspeak.app folders and Tauri's registry
    #    keys. Tauri's own block never runs: the hook does its work.
    New-Fixture
    Invoke-Harness $exe '/TICK' | Out-Null
    $s = Get-State
    Check 'ticked removes all' ((All-Gone $s) -and (-not $s.tauriRan)) (Describe $s)
    $results.Add((Log-Line))

    # 2..5. Every case where nothing of the hook's own may go. Tauri's block
    #       itself runs on a ticked passive or in-place run, so there its
    #       work (its folders and keys, now done by the hook) is expected.
    foreach ($case in @(
            @{ name = 'unticked keeps all'; sw = ''; tauriKept = $true },
            @{ name = 'update keeps all'; sw = '/TICK /UPDATE'; tauriKept = $true },
            @{ name = 'passive keeps all'; sw = '/TICK /P'; tauriKept = $false },
            @{ name = 'reinstall keeps all'; sw = '/TICK /INPLACE'; tauriKept = $false })) {
        New-Fixture
        Invoke-Harness $exe $case.sw | Out-Null
        $s = Get-State
        $tauriOk = if ($case.tauriKept) { $s.roamingTauri -and $s.localTauri -and $s.reg } else { (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) }
        Check $case.name ($s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.importWav -and $s.otherTemp -and $tauriOk -and (-not $s.tauriRan) -and $s.roamingOther -and $s.localOther) (Describe $s)
    }

    # 6. The data folder itself is a junction: skipped, its target untouched.
    New-Fixture
    Remove-Tree "$roaming\ButterflySpeak"
    New-Item -ItemType File -Force -Path "$root\outside\roaming-target\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$roaming\ButterflySpeak" "$root\outside\roaming-target"
    Invoke-Harness $exe '/TICK' | Out-Null
    $sentinel = Test-Path "$root\outside\roaming-target\sentinel.txt"
    $s = Get-State
    Check 'junction at the data folder is skipped' ($sentinel -and $s.roamingData -and (-not $s.localData)) "junction-target-sentinel=$sentinel $(Describe $s)"
    $results.Add((Log-Line))
    Remove-Tree "$roaming\ButterflySpeak"

    # 7. Junctions inside the data folder (models moved to another drive by
    #    hand): unlinked, never followed.
    New-Fixture
    Remove-Tree "$local\ButterflySpeak\models"
    New-Item -ItemType File -Force -Path "$root\outside\models-target\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$local\ButterflySpeak\models" "$root\outside\models-target"
    New-Item -ItemType File -Force -Path "$root\outside\deep-target\sentinel.txt" -Value 'keep' | Out-Null
    New-Item -ItemType Directory -Force -Path "$local\ButterflySpeak\logs\deep" | Out-Null
    New-Junction "$local\ButterflySpeak\logs\deep\link" "$root\outside\deep-target"
    Invoke-Harness $exe '/TICK' | Out-Null
    $sentinel = Test-Path "$root\outside\models-target\sentinel.txt"
    $deep = Test-Path "$root\outside\deep-target\sentinel.txt"
    $s = Get-State
    Check 'nested junctions: targets kept, folder removed' ($sentinel -and $deep -and (-not $s.localData)) "models-target=$sentinel deep-target=$deep $(Describe $s)"
    $results.Add((Log-Line))

    # 8. Guards: an empty base and a relative base are refused, even with a
    #    matching folder (or converted import) sitting where a relative path
    #    would land. Five refusals: two bundle folders, two data folders and
    #    the temp folder.
    $guardExe = Join-Path $bin 'harness-guard.exe'
    $warn = Build-Harness '' 'relative-base' 'relative-base' $guardExe
    New-Fixture
    $cwd = Join-Path $root 'cwd'
    New-Item -ItemType File -Force -Path "$cwd\relative-base\ButterflySpeak\sentinel.txt" -Value 'keep' | Out-Null
    New-Item -ItemType File -Force -Path "$cwd\relative-base\$tauri\sentinel.txt" -Value 'keep' | Out-Null
    New-Item -ItemType File -Force -Path "$cwd\relative-base\$wavName" -Value 'keep' | Out-Null
    Invoke-Harness $guardExe '/TICK' $cwd | Out-Null
    $sentinel = Test-Path "$cwd\relative-base\ButterflySpeak\sentinel.txt"
    $bundleSentinel = Test-Path "$cwd\relative-base\$tauri\sentinel.txt"
    $wavKept = Test-Path "$cwd\relative-base\$wavName"
    $s = Get-State
    $logText = (Get-Content $log) -join ' | '
    $skips = ([regex]::Matches($logText, 'its base is not an absolute path')).Count
    Check 'empty and relative bases refused' ($sentinel -and $bundleSentinel -and $wavKept -and $skips -eq 5 -and $s.roamingData -and $s.localData) "relative-sentinel=$sentinel relative-bundle=$bundleSentinel relative-wav=$wavKept skips=$skips $(Describe $s)"
    $results.Add('      log: ' + $logText)

    # 9. Names Win32 rewrites. A plain path strips a trailing space or dot
    #    from its last component, but RMDir /r later uses the same name as a
    #    middle component, where a trailing space stays. A junction the walk
    #    looked up under the rewritten name was missed, and RMDir /r followed
    #    it. Every case here must leave the junction's target alone, and the
    #    rest (the roaming folder, both bundle folders, the credentials and
    #    the registry keys) must still go: a folder that is kept is kept on
    #    its own.
    $odd = @(
        @{ name = 'A plain folder "a " holding a junction'; kept = $true; build = {
                New-Item -ItemType Directory -Force -Path "$local\ButterflySpeak\models\a" | Out-Null
                New-Junction "$local\ButterflySpeak\models\a\link" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\models\a" "$local\ButterflySpeak\models\a " } },
        @{ name = 'B junction named "b "'; kept = $true; build = {
                New-Junction "$local\ButterflySpeak\models\b" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\models\b" "$local\ButterflySpeak\models\b " } },
        @{ name = 'C junction named "c."'; kept = $true; build = {
                New-Junction "$local\ButterflySpeak\models\c" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\models\c" "$local\ButterflySpeak\models\c." } },
        @{ name = 'D plain folder "d." holding a junction'; kept = $true; build = {
                New-Item -ItemType Directory -Force -Path "$local\ButterflySpeak\logs\d" | Out-Null
                New-Junction "$local\ButterflySpeak\logs\d\link" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\logs\d" "$local\ButterflySpeak\logs\d." } },
        @{ name = 'J junction "j " beside a plain folder "j"'; kept = $true; build = {
                New-Item -ItemType Directory -Force -Path "$local\ButterflySpeak\models\j" | Out-Null
                New-Junction "$local\ButterflySpeak\models\jtmp" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\models\jtmp" "$local\ButterflySpeak\models\j " } },
        @{ name = 'K junction named "nul"'; kept = $false; build = {
                New-Junction "$local\ButterflySpeak\models\ntmp" "$root\outside\target"
                Move-Exact "$local\ButterflySpeak\models\ntmp" "$local\ButterflySpeak\models\nul" } }
    )
    foreach ($case in $odd) {
        New-Fixture
        New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
        & $case.build
        Invoke-Harness $exe '/TICK' | Out-Null
        $sentinel = Test-Path "$root\outside\target\sentinel.txt"
        $s = Get-State
        $restGone = (-not $s.roamingData) -and $s.creds -eq 0 -and (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) -and (-not $s.importWav)
        Check $case.name ($sentinel -and $restGone -and ($s.localData -eq $case.kept)) "target-sentinel=$sentinel $(Describe $s)"
        $results.Add((Log-Line))
    }

    # 10. A base with a trailing separator. The hook folds "/\" and "\/"
    #     before its exact-name (\\?\) lookups, which take a path as written,
    #     so the junction inside is still unlinked and every folder still goes.
    $slashExe = Join-Path $bin 'harness-slash.exe'
    $warn = Build-Harness "$roaming/" "$local\/" "$temp/" $slashExe -Simulate
    New-Fixture
    Remove-Tree "$local\ButterflySpeak\models"
    New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$local\ButterflySpeak\models" "$root\outside\target"
    Invoke-Harness $slashExe '/TICK' | Out-Null
    $sentinel = Test-Path "$root\outside\target\sentinel.txt"
    $s = Get-State
    Check 'trailing-separator bases' ($sentinel -and (All-Gone $s)) "target-sentinel=$sentinel $(Describe $s)"
    $results.Add((Log-Line))

    # 11.. The com.butterflyspeak.app folders. Tauri's own block would run
    #    RmDir /r on both, which follows junctions; the hook keeps that block
    #    from running and removes each folder itself, after the app check,
    #    with the same checks as its own folders. Every case must leave the
    #    junction's target alone.
    $tauriCases = @(
        @{ name = 'a Tauri folder that is itself a junction: that one kept, the rest goes'; sw = '/TICK'
            build = {
                Remove-Tree "$roaming\$tauri"
                New-Junction "$roaming\$tauri" "$root\outside\target" }
            expect = { param($s) (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and $s.roamingTauri -and (-not $s.localTauri) -and (-not $s.reg) -and (-not $s.importWav) } },
        @{ name = 'a junction inside a Tauri folder: unlinked, both go'; sw = '/TICK'
            build = { New-Junction "$local\$tauri\EBWebView\moved" "$root\outside\target" }
            expect = { param($s) All-Gone $s } },
        @{ name = 'a junction named "b " inside a Tauri folder: that one kept, the rest goes'; sw = '/TICK'
            build = {
                New-Junction "$local\$tauri\EBWebView\b" "$root\outside\target"
                Move-Exact "$local\$tauri\EBWebView\b" "$local\$tauri\EBWebView\b " }
            expect = { param($s) (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.roamingTauri) -and $s.localTauri -and (-not $s.reg) } },
        @{ name = 'reinstall with a junction inside a Tauri folder'; sw = '/TICK /INPLACE'
            build = { New-Junction "$local\$tauri\EBWebView\moved" "$root\outside\target" }
            expect = { param($s) $s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.importWav -and (-not $s.roamingTauri) -and (-not $s.localTauri) } },
        @{ name = 'update with a Tauri folder that is a junction'; sw = '/TICK /UPDATE'
            build = {
                Remove-Tree "$roaming\$tauri"
                New-Junction "$roaming\$tauri" "$root\outside\target" }
            expect = { param($s) $s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.roamingTauri -and $s.localTauri -and $s.reg -and $s.importWav } }
    )
    foreach ($case in $tauriCases) {
        New-Fixture
        New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
        & $case.build
        Invoke-Harness $exe $case.sw | Out-Null
        $sentinel = Test-Path "$root\outside\target\sentinel.txt"
        $s = Get-State
        Check $case.name ($sentinel -and (-not $s.tauriRan) -and (& $case.expect $s)) "target-sentinel=$sentinel $(Describe $s)"
        $results.Add((Log-Line))
    }

    # 16. Cancelled at the app check. The pre-uninstall hook runs before
    #     Tauri's "is the app running?" prompt, and Cancel there stops the
    #     uninstall: nothing at all may have changed, links included.
    New-Fixture
    New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
    New-Item -ItemType File -Force -Path "$root\outside\t2\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$local\$tauri\EBWebView\moved" "$root\outside\target"
    New-Junction "$local\ButterflySpeak\models\moved" "$root\outside\t2"
    $code = Invoke-Harness $exe '/TICK /CANCEL' -AnyExit
    $links = (Test-Exact "$local\$tauri\EBWebView\moved") -and (Test-Exact "$local\ButterflySpeak\models\moved")
    $sentinel = (Test-Path "$root\outside\target\sentinel.txt") -and (Test-Path "$root\outside\t2\sentinel.txt")
    $cancelled = ((Get-Content $log) -match 'cancelled at the app check').Count -gt 0
    $s = Get-State
    Check 'cancelled at the app check: nothing changed' ($cancelled -and $links -and $sentinel -and $s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.roamingTauri -and $s.localTauri -and $s.reg -and $s.importWav) "exit=$code links-kept=$links targets-kept=$sentinel $(Describe $s)"
    $results.Add((Log-Line))

    # 17..19. Partial failure: a folder holding a junction the walk could
    #    unlink and one it cannot vouch for. The whole check runs before any
    #    unlink, so the first link must still be there, the folder kept, both
    #    targets kept, and everything else removed.
    $partial = @(
        @{ name = 'partial: a Tauri folder with "a-moved" and "z "'; dir = "$local\$tauri\EBWebView"; lock = $false
            expect = { param($s) (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.roamingTauri) -and $s.localTauri -and (-not $s.reg) } },
        @{ name = 'partial: a data folder with "a-moved" and "z "'; dir = "$local\ButterflySpeak\models"; lock = $false
            expect = { param($s) (-not $s.roamingData) -and $s.localData -and $s.creds -eq 0 -and (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) } },
        @{ name = 'partial: a Tauri folder with "a-moved" and a junction that cannot be removed'; dir = "$local\$tauri\EBWebView"; lock = $true
            expect = { param($s) (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.roamingTauri) -and $s.localTauri -and (-not $s.reg) } }
    )
    foreach ($case in $partial) {
        New-Fixture
        New-Item -ItemType File -Force -Path "$root\outside\t1\sentinel.txt" -Value 'keep' | Out-Null
        New-Item -ItemType File -Force -Path "$root\outside\t2\sentinel.txt" -Value 'keep' | Out-Null
        New-Junction "$($case.dir)\a-moved" "$root\outside\t1"
        if ($case.lock) {
            New-Junction "$($case.dir)\z-locked" "$root\outside\t2"
            Lock-Junction "$($case.dir)\z-locked"
        } else {
            New-Junction "$($case.dir)\z" "$root\outside\t2"
            Move-Exact "$($case.dir)\z" "$($case.dir)\z "
        }
        Invoke-Harness $exe '/TICK' | Out-Null
        $first = Test-Exact "$($case.dir)\a-moved"
        $sentinel = (Test-Path "$root\outside\t1\sentinel.txt") -and (Test-Path "$root\outside\t2\sentinel.txt")
        $s = Get-State
        Unlock-All
        Check $case.name ($first -and $sentinel -and (-not $s.tauriRan) -and (& $case.expect $s)) "a-moved-kept=$first targets-kept=$sentinel $(Describe $s)"
        $results.Add((Log-Line))
    }

    # 20. One folder fails, the rest goes: the roaming data folder holds a
    #     junction named "b ", so it is kept; the other data folder, both
    #     com.butterflyspeak.app folders, the credentials, the converted
    #     import and Tauri's registry keys all still go.
    New-Fixture
    New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$roaming\ButterflySpeak\b" "$root\outside\target"
    Move-Exact "$roaming\ButterflySpeak\b" "$roaming\ButterflySpeak\b "
    Invoke-Harness $exe '/TICK' | Out-Null
    $sentinel = Test-Path "$root\outside\target\sentinel.txt"
    $s = Get-State
    Check 'one folder fails, the rest goes' ($sentinel -and $s.roamingData -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.importWav) -and (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) -and (-not $s.tauriRan)) "target-sentinel=$sentinel $(Describe $s)"
    $results.Add((Log-Line))

    # 21..23. A read-only link. Opening it for deletion succeeds, but removing
    #    it fails until its own read-only bit is cleared. The walk clears that
    #    bit on the link only, so the folder goes, every target is kept, and a
    #    read-only target stays read-only. Whatever happens, a folder that is
    #    kept must still hold "a-moved": no link goes before the folder is
    #    known to go.
    $roCases = @(
        @{ name = 'a data folder with "a-moved" and a read-only junction'; kind = 'junction' },
        @{ name = 'a data folder with "a-moved" and a read-only file symbolic link'; kind = 'file' },
        @{ name = 'a data folder with "a-moved" and a read-only folder symbolic link'; kind = 'dir' }
    )
    foreach ($case in $roCases) {
        New-Fixture
        $dir = "$local\ButterflySpeak\models"
        New-Item -ItemType File -Force -Path "$root\outside\t1\sentinel.txt" -Value 'keep' | Out-Null
        New-Item -ItemType File -Force -Path "$root\outside\t2\sentinel.txt" -Value 'keep' | Out-Null
        New-Junction "$dir\a-moved" "$root\outside\t1"
        $made = $true
        switch ($case.kind) {
            'junction' { New-Junction "$dir\z-ro" "$root\outside\t2"; $target = "$root\outside\t2" }
            'file' { $target = "$root\outside\t2\sentinel.txt"; $made = New-Symlink "$dir\z-ro" $target }
            'dir' { $target = "$root\outside\t2"; $made = New-Symlink "$dir\z-ro" $target -Directory }
        }
        if (-not $made) {
            Skip $case.name '(this machine cannot create symbolic links; turn on Developer Mode to run it)'
            continue
        }
        Set-ReadOnly $target
        Set-ReadOnly "$dir\z-ro" -Link
        Invoke-Harness $exe '/TICK' | Out-Null
        $first = Test-Exact "$dir\a-moved"
        $sentinel = (Test-Path "$root\outside\t1\sentinel.txt") -and (Test-Path "$root\outside\t2\sentinel.txt")
        $targetRo = Test-ReadOnly $target
        $s = Get-State
        $noPartial = (-not $s.localData) -or $first
        Check $case.name ($sentinel -and $targetRo -and $noPartial -and (-not $s.tauriRan) -and (All-Gone $s)) "a-moved-kept=$first targets-kept=$sentinel target-still-read-only=$targetRo $(Describe $s)"
        $results.Add((Log-Line))
    }

    # 24. A folder reparse point that is neither a junction nor a symbolic
    #     link (a cloud-files folder, say). Removing only its reparse entry
    #     fails when it holds anything, so the walk cannot vouch for it: that
    #     com.butterflyspeak.app folder is kept with "a-moved" still in it,
    #     and everything else goes.
    New-Fixture
    New-Item -ItemType File -Force -Path "$root\outside\t1\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$local\$tauri\EBWebView\a-moved" "$root\outside\t1"
    New-OtherReparseFolder "$local\$tauri\EBWebView\z-other"
    Invoke-Harness $exe '/TICK' | Out-Null
    $first = Test-Exact "$local\$tauri\EBWebView\a-moved"
    $sentinel = Test-Path "$root\outside\t1\sentinel.txt"
    $s = Get-State
    Check 'a folder reparse point that is not a link keeps its folder whole' ($first -and $sentinel -and $s.localTauri -and (-not $s.roamingTauri) -and (-not $s.roamingData) -and (-not $s.localData) -and $s.creds -eq 0 -and (-not $s.reg) -and (-not $s.tauriRan)) "a-moved-kept=$first target-kept=$sentinel $(Describe $s)"
    $results.Add((Log-Line))

    # 25. The app closes at the app check. Before it, the web view still has a
    #     file the walk cannot vouch for ("busy ", a name ending in a space);
    #     the app closing removes it. The com.butterflyspeak.app folder is
    #     judged after the app check, so it goes with the rest, junction
    #     target kept.
    New-Fixture
    New-Item -ItemType File -Force -Path "$root\outside\target\sentinel.txt" -Value 'keep' | Out-Null
    New-Junction "$local\$tauri\EBWebView\a-moved" "$root\outside\target"
    New-Item -ItemType File -Force -Path "$local\$tauri\EBWebView\busy" -Value 'in use' | Out-Null
    Move-Exact "$local\$tauri\EBWebView\busy" "$local\$tauri\EBWebView\busy "
    Invoke-Harness $exe '/TICK /SETTLE' | Out-Null
    $sentinel = Test-Path "$root\outside\target\sentinel.txt"
    $settled = ((Get-Content $log) -match 'the app closed at the app check').Count -gt 0
    $s = Get-State
    Check 'a Tauri folder is judged after the app check' ($settled -and $sentinel -and (-not $s.tauriRan) -and (All-Gone $s)) "settled=$settled target-sentinel=$sentinel $(Describe $s)"
    $results.Add((Log-Line))

    # 26.. A reinstall whose $INSTDIR names the harness's own folder another
    #    way. Tauri's reinstall passes _?=<install location> as the registry
    #    holds it (as the user typed it with /D=), NSIS takes that word for
    #    word, and $EXEDIR is the path the uninstaller was started by. Every
    #    spelling of the same folder is a reinstall: the data is kept
    #    (Tauri's own work, which the tick asks for, still goes). A different
    #    folder that exists is not: everything goes. A $INSTDIR the hook
    #    cannot open, for a reason other than its not being there, counts as
    #    a reinstall: the data is kept.
    #
    #    The harness runs Tauri's RMDir "$INSTDIR" before
    #    NSIS_HOOK_POSTUNINSTALL, as the real uninstaller does. It removes a
    #    junction or a directory symbolic link although the folder it points
    #    to holds the harness, so for those two the hook must have decided
    #    before it; the log must show that RMDir removed the link.
    #
    #    "short" and "long" run a copy of the harness from a throwaway folder
    #    under C:\ProgramData, whose own 8.3 name gives a second spelling:
    #    "short" is started by its long path and "long" by its 8.3 path. When
    #    the two spellings come out the same (no 8.3 name anywhere on the
    #    path), the case prints SKIP: it would test nothing.
    $pdExe = $null
    $pdExeShort = $null
    try {
        New-Item -ItemType Directory -Force -Path "$pdRoot\bin" | Out-Null
        Copy-Item -LiteralPath $exe -Destination "$pdRoot\bin\harness.exe"
        $pdExe = "$pdRoot\bin\harness.exe"
        $sb = New-Object System.Text.StringBuilder 1024
        if ([BsHarness.K]::GetShortPathNameW($pdExe, $sb, 1024) -gt 0) { $pdExeShort = $sb.ToString() }
    } catch {
        $results.Add("      note: could not set up $pdRoot ($($_.Exception.Message)); the 8.3 cases run from %TEMP%")
    }
    foreach ($kind in @('dot', 'dotdot', 'doubled', 'trailingdot', 'short', 'long', 'junction', 'symlink', 'denied', 'sibling')) {
        New-Fixture
        $runExe = $exe
        $runCwd = $bin
        switch ($kind) {
            'short' { if ($pdExe) { $runExe = $pdExe; $runCwd = "$pdRoot\bin" } }
            'long' { if ($pdExeShort) { $runExe = $pdExeShort; $runCwd = "$pdRoot\bin" } }
            'junction' { if (-not (Test-Exact "$root\bin-junction")) { New-Junction "$root\bin-junction" $bin } }
            'symlink' { if (-not (Test-Exact "$root\bin-symlink")) { $null = New-Symlink "$root\bin-symlink" $bin -Directory } }
            'sibling' { New-Item -ItemType Directory -Force -Path "$root\install" | Out-Null }
            'denied' { New-Item -ItemType Directory -Force -Path "$root\denied" | Out-Null }
        }
        if ($kind -eq 'symlink' -and -not (Test-Exact "$root\bin-symlink")) {
            Skip "reinstall with `$INSTDIR spelled $kind keeps the data" '(this machine cannot create symbolic links; turn on Developer Mode to run it)'
            continue
        }
        if ($kind -eq 'denied') {
            & icacls.exe "$root\denied" /deny "${me}:(F)" | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "icacls deny on $root\denied failed" }
        }
        try { Invoke-Harness $runExe "/TICK /INSTDIR=$kind" $runCwd | Out-Null }
        finally { if ($kind -eq 'denied' -and (Test-Path "$root\denied")) { & icacls.exe "$root\denied" /remove:d $me | Out-Null } }
        $s = Get-State
        $logLines = Get-Content $log
        # Whether $INSTDIR was spelled differently at all, and whether RMDir
        # removed it.
        $differs = ($logLines | Where-Object { $_ -match "^harness: exedir='(.*)' instdir='(.*)'$" } | ForEach-Object { $Matches[1] -cne $Matches[2] }) -contains $true
        $rmdirRemoved = ($logLines -match '^harness: RMDir removed ').Count -gt 0
        if ($kind -eq 'sibling') {
            Check "another existing folder as `$INSTDIR ($kind) removes all" ((All-Gone $s) -and (-not $s.tauriRan)) "rmdir-removed=$rmdirRemoved $(Describe $s)"
        } else {
            $kept = $s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.importWav -and $s.otherTemp -and (-not $s.roamingTauri) -and (-not $s.localTauri) -and (-not $s.reg) -and (-not $s.tauriRan) -and $s.roamingOther -and $s.localOther
            $name = "reinstall with `$INSTDIR spelled $kind keeps the data"
            $detail = "spelled-differently=$differs rmdir-removed=$rmdirRemoved $(Describe $s)"
            if ($kind -eq 'denied') {
                $note = ($logLines -match 'so this counts as a reinstall').Count -gt 0
                Check "a `$INSTDIR that cannot be opened ($kind) counts as a reinstall" ($kept -and $note) "noted=$note rmdir-removed=$rmdirRemoved $(Describe $s)"
            } elseif ($kind -eq 'junction' -or $kind -eq 'symlink') {
                Check $name ($kept -and $rmdirRemoved) $detail
            } elseif (-not $differs -and $kept) {
                Skip $name "(the two spellings are the same here, so this tests nothing: $detail)"
            } else {
                Check $name ($kept -and $differs) $detail
            }
        }
        $results.Add((Log-Line))
    }

    # A cancelled uninstall whose $INSTDIR is a junction to the harness's
    # folder. NSIS_HOOK_PREUNINSTALL opens both folders to compare them;
    # that, and the cancel after it, must change nothing: the junction, what
    # it points to and all the data stay.
    New-Fixture
    if (-not (Test-Exact "$root\bin-junction")) { New-Junction "$root\bin-junction" $bin }
    $code = Invoke-Harness $exe '/TICK /CANCEL /INSTDIR=junction' -AnyExit
    $link = (Test-Exact "$root\bin-junction") -and (([BsHarness.K]::GetFileAttributesW("$root\bin-junction") -band 0x400) -ne 0) -and (Test-Path "$root\bin-junction\harness.exe")
    $cancelled = ((Get-Content $log) -match 'cancelled at the app check').Count -gt 0
    $s = Get-State
    Check 'cancelled with $INSTDIR a junction to the uninstaller''s folder: nothing changed' ($cancelled -and $link -and $s.roamingData -and $s.localData -and $s.creds -eq 3 -and $s.roamingTauri -and $s.localTauri -and $s.reg -and $s.importWav) "exit=$code junction-kept=$link $(Describe $s)"
    $results.Add((Log-Line))
}
finally {
    Unlock-All
    Clear-ReadOnly
    Remove-TestCredentials
    Remove-TestKey
    Remove-Tree $root
    Remove-Tree $pdRoot
    $leftCreds = @($accounts | Where-Object { Test-Credential (Target $_) }).Count
    $results.Add("cleanup: temp folder removed=$(-not (Test-Path $root)) ProgramData folder removed=$(-not (Test-Path $pdRoot)) test credentials left=$leftCreds test key left=$(Test-Path $regKey)")
}

$results | ForEach-Object { Write-Output $_ }
$passed = @($results | Where-Object { $_ -like 'PASS *' }).Count
$skipped = @($results | Where-Object { $_ -like 'SKIP *' }).Count
Write-Output "$passed passed, $failures failed, $skipped skipped"
if ($failures -gt 0) { Write-Output "$failures case(s) failed"; exit 1 }
Write-Output 'all cases passed'
exit 0
