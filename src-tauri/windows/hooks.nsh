; Butterfly Speak's NSIS installer hooks (tauri.conf.json:
; bundle.windows.nsis.installerHooks).
;
; What this file does: when the person uninstalling ticks "Delete app data",
; remove everything Butterfly Speak keeps for them, not just the two
; com.butterflyspeak.app folders Tauri's own uninstaller removes:
;
;   %APPDATA%\ButterflySpeak       settings.json (dictionary, snippets,
;                                  prompts), history.db (dictation history,
;                                  notes, learned corrections), theme and
;                                  updater sidecars
;   %LOCALAPPDATA%\ButterflySpeak  logs\ and models\ (the downloaded models)
;   %TEMP%\bs-import-*.wav         converted audio from an import the app
;                                  was killed in (import/mod.rs ScratchWav)
;   Credential Manager             sarvam-api-key.ButterflySpeak,
;                                  custom-endpoint-key.ButterflySpeak,
;                                  cloud-refresh-token.ButterflySpeak
;
; The folders are settings::config_dir(), lib.rs logs_dir() and
; settings::models_root(); the credential names are sarvam::key's KeySlot
; accounts plus its SERVICE, the way the keyring crate names a Windows
; target (<account>.<service>). A unit test in sarvam/key.rs reads this file
; and fails if a KeySlot is missing here.
;
; It also takes over Tauri's own "Delete app data" block, which removes
; %APPDATA%\com.butterflyspeak.app and %LOCALAPPDATA%\com.butterflyspeak.app
; (the webview's storage) with RMDir /r. RMDir /r follows a junction and
; empties its target, and Tauri's block runs before NSIS_HOOK_POSTUNINSTALL,
; while NSIS_HOOK_PREUNINSTALL runs before Tauri's "is the app running?"
; check, which the user can cancel. So:
;
;   NSIS_HOOK_PREUNINSTALL   changes no file and checks no data folder. It
;                            keeps the user's choice, clears the checkbox
;                            state so Tauri's block does not run, and, for a
;                            ticked uninstall, notes whether it is a
;                            reinstall (BS_RUNS_IN_PLACE), before Tauri's
;                            RMDir "$INSTDIR" can remove a link it needs.
;   NSIS_HOOK_POSTUNINSTALL  runs after the app check. It does Tauri's block
;                            itself: the same registry lines, then each
;                            com.butterflyspeak.app folder on its own. Then
;                            the hook's own removals above. Every folder is
;                            checked here, once the app is closed.
;
; Tauri's registry lines are copied from its 2.11.4 template (installer.nsi,
; Section Uninstall); re-check them when Tauri is upgraded.
;
; When the removals do nothing:
;   - "Delete app data" not ticked (the default). Nothing of the user's goes.
;   - An update. Tauri 2.11.4's installer never runs the old uninstaller in
;     update mode (PageLeaveReinstall skips to reinst_done when $UpdateMode
;     is 1), so this is a second layer: an uninstaller started with /UPDATE
;     keeps everything.
; And the hook's own removals (not Tauri's block, which checks only the tick
; and update mode) also do nothing in:
;   - Passive mode (/P): the confirm page, and so the checkbox, never shows.
;   - A reinstall: the installer's "uninstall first" path runs the old
;     uninstaller in place with _?=<install dir> and waits for it. A normal
;     uninstall copies itself to a folder under %TEMP% and runs from there
;     (NSIS manual 3.2, and the 2.08 changelog), so $EXEDIR is the install
;     folder only when something ran it in place. That is the reinstall.
;     $INSTDIR is then the install location word for word as the registry
;     holds it, so the two are compared as folders, not as text (see
;     BS_RUNS_IN_PLACE), in NSIS_HOOK_PREUNINSTALL, and a comparison that
;     cannot be made counts as a reinstall.
;
; Every folder removal is guarded so that an empty or unexpected base can
; never widen what is deleted: the base must be an absolute path (X:\... or
; \\server\...), the leaf name is a compile-time constant, and the exact
; target must be an existing directory that is not a junction or symbolic
; link. Inside it, a full check pass runs before anything is changed; a
; folder that fails it is kept, on its own, and the others still go. The
; check can foresee the failures listed at BS_WALK, not every one (see
; there), but no failure ever touches what a link points to.
;
; Test harness: src-tauri/windows/hooks-harness.nsi, run by
; src-tauri/windows/test-hooks.ps1. It points the bases, the credential
; prefix and the registry keys at a temporary folder, throwaway credentials
; and a throwaway key; never test this by running a real uninstaller.

!include LogicLib.nsh

; The bases and the credential prefix are overridable only so the harness can
; point them somewhere harmless. The shipped build uses these defaults.
!ifndef BS_ROAMING_BASE
  !define BS_ROAMING_BASE "$APPDATA"
!endif
!ifndef BS_LOCAL_BASE
  !define BS_LOCAL_BASE "$LOCALAPPDATA"
!endif
!ifndef BS_TEMP_BASE
  !define BS_TEMP_BASE "$TEMP"
!endif
!ifndef BS_CRED_PREFIX
  !define BS_CRED_PREFIX ""
!endif

!define BS_DATA_FOLDER "ButterflySpeak"
!define BS_CRED_SERVICE "ButterflySpeak"
; Tauri's own folder under each base. The installer defines BUNDLEID only
; after it includes this file, so BS_TAKE_BUNDLEID takes it from there when a
; hook is inserted; this is the harness's value, and the same string.
!define BS_BUNDLE_FOLDER "com.butterflyspeak.app"

!if "${BS_DATA_FOLDER}" == ""
  !error "BS_DATA_FOLDER must name a folder; an empty leaf would delete the base itself"
!endif

; The user's "Delete app data" choice, kept by NSIS_HOOK_PREUNINSTALL before
; it clears $DeleteAppDataCheckboxState.
Var BsDeleteAppData
; 1 when this uninstall is a reinstall (BS_RUNS_IN_PLACE), or cannot be told
; from one, and 0 when it is not. NSIS_HOOK_PREUNINSTALL sets it, before
; Tauri's RMDir "$INSTDIR"; NSIS_HOOK_POSTUNINSTALL reads it.
Var BsInPlace
; BS_CHECK_FOLDER's answer: the canonical target, and 0 (it can be removed),
; 1 (keep it) or 2 (nothing there).
Var BsFolder
Var BsFolderState

; BS_BUNDLE_FOLDER from the installer's BUNDLEID, when there is one, and
; never empty: an empty leaf would make the target the base itself.
!macro BS_TAKE_BUNDLEID
  !ifdef BUNDLEID
    !define /redef BS_BUNDLE_FOLDER "${BUNDLEID}"
  !endif
  !if "${BS_BUNDLE_FOLDER}" == ""
    !error "BS_BUNDLE_FOLDER must name a folder; an empty leaf would delete the base itself"
  !endif
!macroend

; A line in the uninstaller's details list, and in the harness's log file
; when the harness defines BS_HOOK_LOG.
!macro BS_HOOK_NOTE MSG
  DetailPrint "${MSG}"
  !ifdef BS_HOOK_LOG
    Push $R6
    FileOpen $R6 "${BS_HOOK_LOG}" a
    FileSeek $R6 0 END
    FileWrite $R6 "${MSG}$\r$\n"
    FileClose $R6
    Pop $R6
  !endif
!macroend

; OUT = 1 when PATH is absolute (X:\... or \\...), else 0. OUT and PATH must
; differ, and neither may be $R8.
!macro BS_IS_ABSOLUTE PATH OUT
  Push $R8
  StrCpy ${OUT} 0
  StrLen $R8 ${PATH}
  ${If} $R8 >= 3
    StrCpy $R8 ${PATH} 2 1 ; the two characters after the drive letter
    ${If} $R8 == ":\"
      StrCpy ${OUT} 1
    ${EndIf}
    StrCpy $R8 ${PATH} 2 ; UNC, for a redirected profile folder
    ${If} $R8 == "\\"
      StrCpy ${OUT} 1
    ${EndIf}
  ${EndIf}
  Pop $R8
!macroend

; OUT = the \\?\ form of the absolute path IN, which the file system takes
; name for name (no trailing-space, trailing-dot or device-name rewriting).
; IN must already be canonical (BS_CHECK_FOLDER makes it so), because \\?\
; does not fold "\\", "/" or "..". IN and OUT must differ.
!macro BS_EXACT_PATH IN OUT
  StrCpy ${OUT} ${IN} 2
  ${If} ${OUT} == "\\"
    StrCpy ${OUT} ${IN} "" 2
    StrCpy ${OUT} "\\?\UNC\${OUT}"
  ${Else}
    StrCpy ${OUT} "\\?\${IN}"
  ${EndIf}
!macroend

; Walk the folder TOP. MODE "check" changes nothing: RESULT is set to 1 when
; any entry cannot be vouched for, and to 0 otherwise. MODE "unlink" also
; removes every junction or symbolic link inside TOP (the link only, never
; what it points at); run it only after a "check" of the same folder passed,
; so that a folder the check says to keep never loses a link first.
;
; What "check" cannot promise: that every removal in "unlink" then works.
; It rules out every cause this file knows of (the list below), and the app
; has been closed by then, but a removal can still fail for a reason no
; probe shows ahead of time, or because something changed the folder in
; between. Then the links before that one are gone, the folder is kept, and
; nothing a link points to is touched.
;
; Why links matter: RMDir /r in NSIS 3.11 follows a junction and empties its
; target (test-hooks.ps1 case 7 proved it), so without this a models folder a
; user had moved to another drive with a junction would lose whatever sits at
; the other end.
;
; What cannot be vouched for, each of which keeps the whole folder:
;   - a folder the walk cannot list, or an entry whose attributes cannot be
;     read;
;   - a name ending in a space or a dot. A plain Win32 path is rewritten
;     before it reaches the file system: a trailing space or dot is stripped
;     from its last component, and a device name such as NUL can be mapped
;     to the device. RMDir /r, though, reaches the same entry as a middle
;     component ("...\a \*.*"), where a trailing space is kept, and the walk
;     descends with plain paths too (test-hooks.ps1 cases A, B and J);
;   - a link that cannot be opened, without following it, for deletion (and,
;     when it is read-only, for changing its attributes: "unlink" clears a
;     link's own read-only bit before removing it, since a read-only link
;     cannot be removed). The open changes nothing. It is how the check sees
;     a link that the permissions keep, but it is not a promise that the
;     removal will work: see above;
;   - a folder reparse point that is neither a junction nor a symbolic link
;     (a cloud-files folder, say). It has contents of its own, and its entry
;     cannot be removed while it holds anything;
;   - in "unlink" mode, a removal that fails.
; Every entry is looked up, probed and removed by its exact name, through a
; \\?\ path.
;
; A hard link is not a link in this walk's sense: it is the file itself,
; under one more name, and deleting that name never removes the data behind
; another. But RMDir /r and Delete clear a file's read-only bit before
; deleting it, and that bit belongs to the file, so a file outside that
; shares a hard link with an entry here (or with a %TEMP%\bs-import-*.wav)
; keeps its content and loses its read-only bit. Checking every file's link
; count is not worth what it would add.
;
; The walk keeps its work list on the NSIS stack, above a marker, so it
; needs no function (a function would have to exist twice, as un.* for the
; uninstaller and plain for the harness). RESULT must not be $R0-$R7.
!macro BS_WALK TOP MODE RESULT
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  StrCpy $R5 0
  Push "bs-walk-end"
  Push "${TOP}"
  ${Do}
    Pop $R0
    ${If} $R0 == "bs-walk-end"
      ${ExitDo}
    ${EndIf}
    ; Once the folder is being kept, stop looking: only drain the stack.
    ${If} $R5 = 0
      ClearErrors
      FindFirst $R1 $R2 "$R0\*.*"
      ${If} ${Errors}
        StrCpy $R5 1
        !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not list $R0"
      ${EndIf}
      ${DoUntil} $R2 == ""
        ${If} $R5 = 0
        ${AndIf} $R2 != "."
        ${AndIf} $R2 != ".."
          StrCpy $R3 "$R0\$R2"
          StrCpy $R7 $R2 1 -1 ; the name's last character
          ${If} $R7 == " "
          ${OrIf} $R7 == "."
            StrCpy $R5 1
            !insertmacro BS_HOOK_NOTE "Butterfly Speak: '$R3' has a name ending in a space or a dot, which Windows rewrites"
          ${Else}
            !insertmacro BS_EXACT_PATH $R3 $R6
            System::Call 'kernel32::GetFileAttributesW(w R6) i .R4'
            ${If} $R4 = -1
              StrCpy $R5 1
              !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not read the attributes of $R3"
            ${Else}
              IntOp $R7 $R4 & 0x400 ; FILE_ATTRIBUTE_REPARSE_POINT
              ${If} $R7 <> 0
                ; $0 keeps the link's own attributes, $1 is scratch, and
                ; $R4 becomes its FILE_ATTRIBUTE_DIRECTORY bit.
                Push $0
                Push $1
                StrCpy $0 $R4
                IntOp $R4 $0 & 0x10
                !if "${MODE}" == "check"
                  ; DELETE (0x10000), and FILE_WRITE_ATTRIBUTES (0x100) too
                  ; when the link is read-only, since "unlink" clears that
                  ; bit first; shared read, write and delete; OPEN_EXISTING;
                  ; FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                  ; so the link itself is opened (a folder link included),
                  ; never what it points to.
                  IntOp $1 $0 & 0x1 ; FILE_ATTRIBUTE_READONLY
                  ${If} $1 <> 0
                    StrCpy $1 0x10100
                  ${Else}
                    StrCpy $1 0x10000
                  ${EndIf}
                  System::Call 'kernel32::CreateFileW(w R6, i r1, i 7, i 0, i 3, i 0x02200000, i 0) i .R7'
                  ${If} $R7 = -1
                    StrCpy $R5 1
                    !insertmacro BS_HOOK_NOTE "Butterfly Speak: the link $R3 cannot be removed"
                  ${Else}
                    ${If} $R4 <> 0
                      ; A folder: only a junction or a symbolic link is a
                      ; link. Any other reparse point (a cloud-files folder,
                      ; say) is a folder with its own contents, whose entry
                      ; cannot be removed while it holds anything.
                      ; FileAttributeTagInfo (9) is { attributes, tag }.
                      System::Call '*(i 0, i 0) p .r1'
                      System::Call 'kernel32::GetFileInformationByHandleEx(i R7, i 9, p r1, i 8) i .R4'
                      ${If} $R4 <> 0
                        System::Call '*$1(i, i .R4)'
                      ${EndIf}
                      System::Free $1
                      ${If} $R4 <> 0xA0000003 ; IO_REPARSE_TAG_MOUNT_POINT
                      ${AndIf} $R4 <> 0xA000000C ; IO_REPARSE_TAG_SYMLINK
                        StrCpy $R5 1
                        !insertmacro BS_HOOK_NOTE "Butterfly Speak: $R3 is a reparse point that is neither a junction nor a symbolic link"
                      ${EndIf}
                    ${EndIf}
                    System::Call 'kernel32::CloseHandle(i R7)'
                  ${EndIf}
                !else if "${MODE}" == "unlink"
                  ; A read-only link cannot be removed, so clear the bit
                  ; first. SetFileAttributesW changes the link's own
                  ; attributes, never its target's (test-hooks.ps1 checks
                  ; that a read-only target stays read-only).
                  IntOp $1 $0 & 0x1
                  ${If} $1 <> 0
                    IntOp $1 $0 & 0xFFFFFFFE
                    System::Call 'kernel32::SetFileAttributesW(w R6, i r1)'
                  ${EndIf}
                  ; Both remove the link itself, never what it points to.
                  ${If} $R4 <> 0
                    System::Call 'kernel32::RemoveDirectoryW(w R6) i .R7'
                  ${Else}
                    System::Call 'kernel32::DeleteFileW(w R6) i .R7'
                  ${EndIf}
                  System::Call 'kernel32::GetFileAttributesW(w R6) i .R4'
                  ${If} $R7 <> 0
                  ${AndIf} $R4 = -1
                    !insertmacro BS_HOOK_NOTE "Butterfly Speak: removed the link $R3 and kept what it points to"
                  ${Else}
                    StrCpy $R5 1
                    ; Put back the read-only bit on a link that stays.
                    IntOp $1 $0 & 0x1
                    ${If} $1 <> 0
                      System::Call 'kernel32::SetFileAttributesW(w R6, i r0)'
                    ${EndIf}
                    !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not remove the link $R3"
                  ${EndIf}
                !else
                  !error "BS_WALK: MODE must be check or unlink"
                !endif
                Pop $1
                Pop $0
              ${Else}
                IntOp $R4 $R4 & 0x10 ; FILE_ATTRIBUTE_DIRECTORY
                ${If} $R4 <> 0
                  Push $R3
                ${EndIf}
              ${EndIf}
            ${EndIf}
          ${EndIf}
        ${EndIf}
        FindNext $R1 $R2
      ${Loop}
      FindClose $R1
    ${EndIf}
  ${Loop}
  StrCpy ${RESULT} $R5
  Pop $R7
  Pop $R6
  Pop $R5
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; Check <BASE>\<LEAF> without changing anything. Sets $BsFolder to the
; canonical target and $BsFolderState to 0 when a removal can only remove
; what is in that folder (a plain folder, every entry inside vouched for), 1
; when it must be kept (the base is not absolute, the target is itself a
; junction or link, or something inside failed the check), and 2 when there
; is nothing there to remove (nothing at all, or a file, which RMDir leaves
; alone).
!macro BS_CHECK_FOLDER BASE LEAF
  Push $R9
  Push $R8
  Push $R7
  StrCpy $BsFolder "${BASE}"
  StrCpy $BsFolderState 1 ; fails closed until the folder is vouched for
  !insertmacro BS_IS_ABSOLUTE $BsFolder $R7
  ${If} $R7 <> 1
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: skipped ${LEAF}, its base is not an absolute path: '$BsFolder'"
  ${Else}
    StrCpy $R9 "$BsFolder\${LEAF}"
    StrCpy $BsFolder $R9
    ; Canonical form ("\\" and "/" folded), which the walk's \\?\ lookups
    ; need: a \\?\ path is taken exactly as written. Passed by register, not
    ; spliced into the call string, so a path with brackets or commas in it
    ; reaches the API unchanged.
    System::Call 'kernel32::GetFullPathNameW(w R9, i ${NSIS_MAX_STRLEN}, w .R8, i 0) i .R7'
    ${If} $R7 = 0
    ${OrIf} $R7 >= ${NSIS_MAX_STRLEN}
      !insertmacro BS_HOOK_NOTE "Butterfly Speak: skipped $R9, it could not be resolved"
    ${Else}
      StrCpy $BsFolder $R8
      StrCpy $R9 $R8
      ; INVALID_FILE_ATTRIBUTES (-1) covers a missing folder and a name the
      ; file system rejects (wildcards included).
      System::Call 'kernel32::GetFileAttributesW(w R9) i .R8'
      ${If} $R8 = -1
        StrCpy $BsFolderState 2
      ${Else}
        IntOp $R7 $R8 & 0x400 ; FILE_ATTRIBUTE_REPARSE_POINT
        IntOp $R8 $R8 & 0x10 ; FILE_ATTRIBUTE_DIRECTORY
        ${If} $R7 <> 0
          !insertmacro BS_HOOK_NOTE "Butterfly Speak: kept $R9, it is a junction or link"
        ${ElseIf} $R8 = 0
          StrCpy $BsFolderState 2
        ${Else}
          !insertmacro BS_WALK $R9 "check" $R8
          ${If} $R8 = 0
            StrCpy $BsFolderState 0
          ${Else}
            !insertmacro BS_HOOK_NOTE "Butterfly Speak: kept $R9, something inside it could not be vouched for"
          ${EndIf}
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Pop $R7
  Pop $R8
  Pop $R9
!macroend

; Remove <BASE>\<LEAF> and everything in it, or keep it with a note: a full
; check first, then the links inside are unlinked, then RMDir /r.
!macro BS_REMOVE_FOLDER BASE LEAF
  Push $R8
  !insertmacro BS_CHECK_FOLDER "${BASE}" "${LEAF}"
  ${If} $BsFolderState = 2
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: nothing to remove at $BsFolder"
  ${ElseIf} $BsFolderState = 0
    !insertmacro BS_WALK $BsFolder "unlink" $R8
    ${If} $R8 <> 0
      !insertmacro BS_HOOK_NOTE "Butterfly Speak: kept $BsFolder, a link inside it could not be removed"
    ${Else}
      ClearErrors
      RMDir /r $BsFolder
      ${If} ${Errors}
        !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not remove all of $BsFolder"
      ${Else}
        !insertmacro BS_HOOK_NOTE "Butterfly Speak: removed $BsFolder"
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Pop $R8
!macroend

; Delete one generic Windows credential by its exact target name. CredDeleteW
; takes no wildcard, so this can only ever remove the one named target.
!macro BS_DELETE_CREDENTIAL ACCOUNT
  Push $R8
  StrCpy $R8 "${BS_CRED_PREFIX}${ACCOUNT}.${BS_CRED_SERVICE}"
  ; CRED_TYPE_GENERIC (1), the type the keyring crate writes.
  System::Call 'advapi32::CredDeleteW(w R8, i 1, i 0) i .R8'
  ${If} $R8 <> 0
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: removed the saved credential ${BS_CRED_PREFIX}${ACCOUNT}.${BS_CRED_SERVICE}"
  ${Else}
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: no saved credential ${BS_CRED_PREFIX}${ACCOUNT}.${BS_CRED_SERVICE}"
  ${EndIf}
  Pop $R8
!macroend

; Delete the converted imports a killed run left in <BASE> (%TEMP%): files
; named bs-import-*.wav only. Delete removes files, never folders, and a
; file link is removed as the link (a hard link: see the note under BS_WALK).
; The app also sweeps these at startup.
!macro BS_DELETE_SCRATCH_IMPORTS BASE
  Push $R9
  Push $R7
  StrCpy $R9 "${BASE}"
  !insertmacro BS_IS_ABSOLUTE $R9 $R7
  ${If} $R7 <> 1
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: skipped the temp folder, its base is not an absolute path: '$R9'"
  ${Else}
    Delete "$R9\bs-import-*.wav"
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: removed any converted imports left in $R9"
  ${EndIf}
  Pop $R7
  Pop $R9
!macroend

; OUT = 1 when this uninstaller runs from the install folder ($INSTDIR), as
; the installer's "uninstall first" path runs it (in place, with
; _?=<install dir>), and 0 when it does not.
;
; $EXEDIR is a full path (NSIS takes it from GetModuleFileName), in the form
; the uninstaller was started by (an 8.3 name, say). $INSTDIR is the _?=
; value word for word, and Tauri passes the install location as the
; registry holds it: as the user gave it with /D= or on the directory page,
; which can hold "\\", a "." or ".." part, a trailing dot, an 8.3 name or a
; junction. So both are made full paths (GetFullPathNameW folds "\\" and
; "/", resolves "." and "..", and drops a trailing dot or space from the last
; part) and compared without a trailing "\" and without regard to case; when
; they still differ, both folders are opened and compared by volume serial
; number and file index, which also sees an 8.3 name, a junction or a subst
; drive.
;
; "Cannot tell" counts as in place, which keeps the data: a path that cannot
; be resolved, a folder that cannot be opened or read for any reason but
; not existing, or $EXEDIR that cannot be opened. A $INSTDIR that does not
; exist (file or path not found) is not in place: the folder this uninstaller
; runs from exists. Nothing here changes a file: the folders are opened with
; no access asked for, and closed again.
;
; It must run before Tauri's Section Uninstall reaches RMDir "$INSTDIR", so
; NSIS_HOOK_PREUNINSTALL runs it. That RMDir fails on the install folder
; while it still holds the running uninstaller, but when $INSTDIR is a
; junction or a directory symbolic link it removes the link, however full
; the folder behind it is. Asked afterwards, $INSTDIR would no longer exist,
; and a reinstall would look like an uninstall.
; OUT must not be $0-$9 or $R0-$R4.
!macro BS_RUNS_IN_PLACE OUT
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $0
  Push $1
  Push $2
  Push $3
  Push $4
  Push $5
  Push $6
  Push $7
  Push $8
  Push $9
  StrCpy ${OUT} 1 ; fails closed: in place until shown otherwise
  StrCpy $R0 $EXEDIR
  StrCpy $R1 $INSTDIR
  System::Call 'kernel32::GetFullPathNameW(w R0, i ${NSIS_MAX_STRLEN}, w .R2, i 0) i .r0'
  System::Call 'kernel32::GetFullPathNameW(w R1, i ${NSIS_MAX_STRLEN}, w .R3, i 0) i .r1'
  ${If} $0 = 0
  ${OrIf} $0 >= ${NSIS_MAX_STRLEN}
  ${OrIf} $1 = 0
  ${OrIf} $1 >= ${NSIS_MAX_STRLEN}
    !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not resolve '$R0' or '$R1', so this counts as a reinstall"
  ${Else}
    ; Without a trailing "\" (StrCmp, and so "==", ignores case). The full
    ; paths, root "\" included, are what gets opened below.
    StrCpy $0 $R2
    StrCpy $2 $0 1 -1
    ${If} $2 == "\"
      StrCpy $0 $0 -1
    ${EndIf}
    StrCpy $1 $R3
    StrCpy $2 $1 1 -1
    ${If} $2 == "\"
      StrCpy $1 $1 -1
    ${EndIf}
    ${If} $0 != $1
      ; No access asked for; shared read, write and delete; OPEN_EXISTING;
      ; FILE_FLAG_BACKUP_SEMANTICS, which a folder needs. A junction is
      ; followed, so a link to the install folder counts as that folder.
      System::Call 'kernel32::CreateFileW(w R3, i 0, i 7, p 0, i 3, i 0x02000000, p 0) p .r3 ?e'
      Pop $2
      ${If} $3 = -1
        ${If} $2 = 2 ; ERROR_FILE_NOT_FOUND
        ${OrIf} $2 = 3 ; ERROR_PATH_NOT_FOUND
          StrCpy ${OUT} 0
        ${Else}
          !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not open $R3 (error $2), so this counts as a reinstall"
        ${EndIf}
      ${Else}
        System::Call 'kernel32::CreateFileW(w R2, i 0, i 7, p 0, i 3, i 0x02000000, p 0) p .r4 ?e'
        Pop $2
        ${If} $4 = -1
          !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not open $R2 (error $2), so this counts as a reinstall"
        ${Else}
          ; BY_HANDLE_FILE_INFORMATION is 13 DWORDs: the volume serial
          ; number is the 8th, the file index the 12th (high) and 13th (low).
          System::Alloc 52
          Pop $R4
          System::Call 'kernel32::GetFileInformationByHandle(p r3, p R4) i .r0'
          ${If} $0 <> 0
            System::Call '*$R4(i, i, i, i, i, i, i, i .r5, i, i, i, i .r6, i .r7)'
            System::Call 'kernel32::GetFileInformationByHandle(p r4, p R4) i .r0'
            ${If} $0 <> 0
              System::Call '*$R4(i, i, i, i, i, i, i, i .r8, i, i, i, i .r9, i .r1)'
              ${If} $5 = $8
              ${AndIf} $6 = $9
              ${AndIf} $7 = $1
                StrCpy ${OUT} 1
              ${Else}
                StrCpy ${OUT} 0
              ${EndIf}
            ${EndIf}
          ${EndIf}
          ${If} $0 = 0
            !insertmacro BS_HOOK_NOTE "Butterfly Speak: could not compare $R2 with $R3, so this counts as a reinstall"
          ${EndIf}
          System::Free $R4
          System::Call 'kernel32::CloseHandle(p r4)'
        ${EndIf}
        System::Call 'kernel32::CloseHandle(p r3)'
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Pop $9
  Pop $8
  Pop $7
  Pop $6
  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; Runs at the start of Tauri's Section Uninstall, before its "is the app
; running?" check, which the user can cancel, and while the app (and its web
; view) may still be changing its folders. So it changes no file and no key
; and judges no data folder: it keeps the user's choice in $BsDeleteAppData
; and clears $DeleteAppDataCheckboxState, which only Tauri's "Delete app
; data" block reads, so that block does not run. NSIS_HOOK_POSTUNINSTALL does
; its work instead, after the app check, and checks each folder then.
;
; It also answers "is this a reinstall?" into $BsInPlace, here because
; Tauri's RMDir "$INSTDIR" comes before NSIS_HOOK_POSTUNINSTALL (see
; BS_RUNS_IN_PLACE), and only when POST will act on the answer: ticked, not
; an update and not passive. BS_RUNS_IN_PLACE opens the two folders with no
; access asked for and closes them, so a cancel at the app check still
; leaves everything as it was. Otherwise $BsInPlace stays 1, which keeps the
; data.
!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro BS_TAKE_BUNDLEID
  StrCpy $BsDeleteAppData $DeleteAppDataCheckboxState
  StrCpy $BsInPlace 1
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    StrCpy $DeleteAppDataCheckboxState 0
    ${If} $PassiveMode <> 1
      !insertmacro BS_RUNS_IN_PLACE $BsInPlace
    ${EndIf}
  ${EndIf}
!macroend

; Runs at the end of Tauri's Section Uninstall, after the app check and after
; the place where Tauri's own "Delete app data" block would have run. It
; reads the user's choice from $BsDeleteAppData, and whether this is a
; reinstall from $BsInPlace.
!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro BS_TAKE_BUNDLEID
  !ifndef MANUKEY
    !error "MANUKEY must be defined (Tauri's installer.nsi, or the harness)"
  !endif
  !ifndef MANUPRODUCTKEY
    !error "MANUPRODUCTKEY must be defined (Tauri's installer.nsi, or the harness)"
  !endif
  !if "${MANUKEY}" == ""
    !error "MANUKEY must name a key below Software"
  !endif
  !if "${MANUKEY}" == "Software"
    !error "MANUKEY must name a key below Software"
  !endif
  !if "${MANUKEY}" == "Software\"
    !error "MANUKEY must name a key below Software"
  !endif
  ${If} $BsDeleteAppData = 1
    ${If} $UpdateMode = 1
      !insertmacro BS_HOOK_NOTE "Butterfly Speak: updating, so your data is kept"
    ${Else}
      ; Tauri's block, taken over (see NSIS_HOOK_PREUNINSTALL). Its registry
      ; lines, as in the 2.11.4 template and in the shell context Tauri's
      ; block would have used, clear the install location and language.
      DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
      DeleteRegKey /ifempty SHCTX "${MANUKEY}"
      DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
      DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
      DeleteRegKey /ifempty HKCU "${MANUKEY}"
      SetShellVarContext current
      ; Then its two folders, each checked now, after the app check, and
      ; each on its own: one kept does not keep the other.
      !insertmacro BS_REMOVE_FOLDER "${BS_ROAMING_BASE}" "${BS_BUNDLE_FOLDER}"
      !insertmacro BS_REMOVE_FOLDER "${BS_LOCAL_BASE}" "${BS_BUNDLE_FOLDER}"

      ; The hook's own removals, which also stay out of passive mode and a
      ; reinstall.
      ${If} $PassiveMode = 1
        !insertmacro BS_HOOK_NOTE "Butterfly Speak: passive uninstall, so your data is kept"
      ${ElseIf} $BsInPlace != "0" ; anything but a plain 0 keeps the data
        !insertmacro BS_HOOK_NOTE "Butterfly Speak: reinstalling, so your data is kept"
      ${Else}
        !insertmacro BS_REMOVE_FOLDER "${BS_ROAMING_BASE}" "${BS_DATA_FOLDER}"
        !insertmacro BS_REMOVE_FOLDER "${BS_LOCAL_BASE}" "${BS_DATA_FOLDER}"
        !insertmacro BS_DELETE_SCRATCH_IMPORTS "${BS_TEMP_BASE}"
        !insertmacro BS_DELETE_CREDENTIAL "sarvam-api-key"
        !insertmacro BS_DELETE_CREDENTIAL "custom-endpoint-key"
        !insertmacro BS_DELETE_CREDENTIAL "cloud-refresh-token"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
