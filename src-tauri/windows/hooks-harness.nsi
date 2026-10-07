; Test harness for hooks.nsh. Compiled and run by test-hooks.ps1; never
; shipped. It is an ordinary silent program, not an uninstaller: it declares
; the variables Tauri's generated installer.nsi declares, sets them from its
; own command line, and inserts NSIS_HOOK_PREUNINSTALL and
; NSIS_HOOK_POSTUNINSTALL exactly where Tauri's Section Uninstall does, with
; Tauri's RMDir "$INSTDIR" and a copy of Tauri's own "Delete app data" block
; between them.
;
; Compile-time parameters (makensis /D...):
;   BS_ROAMING_BASE, BS_LOCAL_BASE  stand-ins for $APPDATA and $LOCALAPPDATA
;   BS_TEMP_BASE                    stand-in for $TEMP
;   BS_CRED_PREFIX                  prefix for throwaway credential targets
;   BS_REG_TAG                      names the throwaway registry key
;                                   HKCU\Software\bs-hook-harness-<tag>, the
;                                   harness's MANUKEY
;   BS_HOOK_LOG                     file the hook appends its notes to
;   BS_HARNESS_OUT                  the .exe to write
; All of those are required. Optional:
;   BS_SIMULATE_TAURI_BLOCK         also run the copy of Tauri's block, which
;                                   removes <base>\com.butterflyspeak.app.
;                                   Only for builds whose bases are absolute
;                                   folders under the test root.
;
; Run-time switches:
;   /TICK    "Delete app data" ticked ($DeleteAppDataCheckboxState = 1)
;   /UPDATE  update mode ($UpdateMode = 1)
;   /P       passive mode ($PassiveMode = 1)
;   /INPLACE running in place, as a reinstall does ($INSTDIR = $EXEDIR)
;   /INSTDIR=<kind>
;            $INSTDIR spelled another way. Tauri's reinstall passes
;            _?=<install location> as the registry holds it, and NSIS takes
;            that word for word, while $EXEDIR is canonical. The same folder:
;              dot          $EXEDIR\.
;              dotdot       $EXEDIR\..\<its own name>
;              doubled      <parent>\\<name>
;              trailingdot  $EXEDIR.
;              short, long  $EXEDIR's 8.3 or long form (the same as $EXEDIR
;                           when no part of it has an 8.3 name)
;              junction     <parent>\<name>-junction, a junction to $EXEDIR
;                           that test-hooks.ps1 makes
;              symlink      <parent>\<name>-symlink, a directory symbolic
;                           link to $EXEDIR that test-hooks.ps1 makes
;            Another folder:
;              sibling      <parent>\install, which test-hooks.ps1 makes
;            A folder the hook cannot open, so it cannot tell:
;              denied       <parent>\denied, which test-hooks.ps1 makes and
;                           denies the user all access to
;   /SETTLE  right after NSIS_HOOK_PREUNINSTALL, delete the file
;            <local base>\com.butterflyspeak.app\EBWebView\"busy ", as the
;            app closing at CheckIfAppIsRunning can change its folders
;   /CANCEL  stop right after NSIS_HOOK_PREUNINSTALL, as Tauri's
;            CheckIfAppIsRunning does when the user cancels its prompt
;
; Every run that is not cancelled then runs RMDir "$INSTDIR", as Tauri's
; Section Uninstall does before NSIS_HOOK_POSTUNINSTALL.

Unicode true
!include LogicLib.nsh
!include FileFunc.nsh

!ifndef BS_ROAMING_BASE
  !error "BS_ROAMING_BASE is required; the harness must never fall back to the real %APPDATA%"
!endif
!ifndef BS_LOCAL_BASE
  !error "BS_LOCAL_BASE is required; the harness must never fall back to the real %LOCALAPPDATA%"
!endif
!ifndef BS_TEMP_BASE
  !error "BS_TEMP_BASE is required; the harness must never fall back to the real %TEMP%"
!endif
!ifndef BS_CRED_PREFIX
  !error "BS_CRED_PREFIX is required; the harness must never name the real credentials"
!endif
!if "${BS_CRED_PREFIX}" == ""
  !error "BS_CRED_PREFIX must not be empty; the harness must never name the real credentials"
!endif
!ifndef BS_REG_TAG
  !error "BS_REG_TAG is required; the harness must never name the real registry key"
!endif
!if "${BS_REG_TAG}" == ""
  !error "BS_REG_TAG must not be empty; the harness must never name the real registry key"
!endif
!ifndef BS_HOOK_LOG
  !error "BS_HOOK_LOG is required"
!endif
!ifndef BS_HARNESS_OUT
  !error "BS_HARNESS_OUT is required"
!endif

; Tauri's names for the keys its "Delete app data" block clears, pointed at
; a throwaway key.
!define MANUKEY "Software\bs-hook-harness-${BS_REG_TAG}"
!define MANUPRODUCTKEY "${MANUKEY}\Butterfly Speak"

Name "Butterfly Speak hook harness"
OutFile "${BS_HARNESS_OUT}"
RequestExecutionLevel user
SilentInstall silent

; The names Tauri's installer.nsi uses, which the hook reads.
Var DeleteAppDataCheckboxState
Var UpdateMode
Var PassiveMode

!include "hooks.nsh"

Section
  ${GetParameters} $0

  StrCpy $DeleteAppDataCheckboxState 0
  ${GetOptions} $0 "/TICK" $1
  ${IfNot} ${Errors}
    StrCpy $DeleteAppDataCheckboxState 1
  ${EndIf}

  ${GetOptions} $0 "/UPDATE" $1
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}

  ${GetOptions} $0 "/P" $1
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}

  StrCpy $INSTDIR "$EXEDIR\not-the-install-folder"
  ${GetOptions} $0 "/INPLACE" $1
  ${IfNot} ${Errors}
    StrCpy $INSTDIR $EXEDIR
  ${EndIf}
  ${GetOptions} $0 "/INSTDIR=" $1
  ${IfNot} ${Errors}
    ${GetParent} $EXEDIR $2
    ${GetFileName} $EXEDIR $3
    ${If} $1 == "dot"
      StrCpy $INSTDIR "$EXEDIR\."
    ${ElseIf} $1 == "dotdot"
      StrCpy $INSTDIR "$EXEDIR\..\$3"
    ${ElseIf} $1 == "doubled"
      StrCpy $INSTDIR "$2\\$3"
    ${ElseIf} $1 == "trailingdot"
      StrCpy $INSTDIR "$EXEDIR."
    ${ElseIf} $1 == "short"
      StrCpy $4 $EXEDIR
      System::Call 'kernel32::GetShortPathNameW(w r4, w .r5, i ${NSIS_MAX_STRLEN}) i .r6'
      StrCpy $INSTDIR $5
    ${ElseIf} $1 == "long"
      StrCpy $4 $EXEDIR
      System::Call 'kernel32::GetLongPathNameW(w r4, w .r5, i ${NSIS_MAX_STRLEN}) i .r6'
      StrCpy $INSTDIR $5
    ${ElseIf} $1 == "junction"
      StrCpy $INSTDIR "$2\$3-junction"
    ${ElseIf} $1 == "symlink"
      StrCpy $INSTDIR "$2\$3-symlink"
    ${ElseIf} $1 == "sibling"
      StrCpy $INSTDIR "$2\install"
    ${ElseIf} $1 == "denied"
      StrCpy $INSTDIR "$2\denied"
    ${Else}
      !insertmacro BS_HOOK_NOTE "harness: unknown /INSTDIR= kind '$1'"
      Abort
    ${EndIf}
  ${EndIf}
  !insertmacro BS_HOOK_NOTE "harness: exedir='$EXEDIR' instdir='$INSTDIR'"

  !insertmacro BS_HOOK_NOTE "harness: tick=$DeleteAppDataCheckboxState update=$UpdateMode passive=$PassiveMode"

  !ifmacrodef NSIS_HOOK_PREUNINSTALL
    !insertmacro NSIS_HOOK_PREUNINSTALL
  !endif

  ; Tauri's CheckIfAppIsRunning comes next. It closes the app, which can
  ; change what is in the app's folders ...
  ${GetOptions} $0 "/SETTLE" $1
  ${IfNot} ${Errors}
    StrCpy $1 "\\?\${BS_LOCAL_BASE}\${BS_BUNDLE_FOLDER}\EBWebView\busy "
    System::Call 'kernel32::DeleteFileW(w r1) i .r2'
    !insertmacro BS_HOOK_NOTE "harness: the app closed at the app check (busy removed: $2)"
  ${EndIf}

  ; ... and aborts the uninstall when the user cancels its prompt.
  ${GetOptions} $0 "/CANCEL" $1
  ${IfNot} ${Errors}
    !insertmacro BS_HOOK_NOTE "harness: cancelled at the app check"
    Abort
  ${EndIf}

  ; Then Tauri deletes the app's files and the uninstaller and runs
  ; RMDir "$INSTDIR". That fails on a folder that still holds anything (a
  ; reinstall's uninstaller cannot delete itself), but it removes a junction
  ; or a directory symbolic link even when its target holds files. Without
  ; /r it never removes a folder that holds anything, so here it can only
  ; remove an empty folder or a link.
  ClearErrors
  RMDir "$INSTDIR"
  ${If} ${Errors}
    !insertmacro BS_HOOK_NOTE "harness: RMDir kept $INSTDIR"
  ${Else}
    !insertmacro BS_HOOK_NOTE "harness: RMDir removed $INSTDIR"
  ${EndIf}

  ; Tauri 2.11.4's own block, as in the generated installer.nsi, with the
  ; harness's bases in place of $APPDATA and $LOCALAPPDATA.
  !ifdef BS_SIMULATE_TAURI_BLOCK
    ${If} $DeleteAppDataCheckboxState = 1
    ${AndIf} $UpdateMode <> 1
      !insertmacro BS_HOOK_NOTE "harness: Tauri's block ran"
      DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
      DeleteRegKey /ifempty SHCTX "${MANUKEY}"
      DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
      DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
      DeleteRegKey /ifempty HKCU "${MANUKEY}"
      SetShellVarContext current
      RmDir /r "${BS_ROAMING_BASE}\${BS_BUNDLE_FOLDER}"
      RmDir /r "${BS_LOCAL_BASE}\${BS_BUNDLE_FOLDER}"
    ${EndIf}
  !endif

  !insertmacro NSIS_HOOK_POSTUNINSTALL
  !insertmacro BS_HOOK_NOTE "harness: done"
SectionEnd
