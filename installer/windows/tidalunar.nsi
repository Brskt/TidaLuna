; TidaLunar Windows installer.
;
; Variability injected via -D defines from `cargo xtask package`:
;   ${VERSION}    e.g. 0.0.4-alpha - DisplayVersion + filename
;   ${ARCH}       amd64 | arm64    - output filename
;   ${DIST_DIR}   absolute path to dist/, source for File /r
;   ${OUT_DIR}    absolute path to target/installer/, OutFile destination

Unicode true

; Solid LZMA: compresses all files together so cross-file redundancy in CEF
; DLLs and locales is exploited. Cuts installer size by ~60% vs default zlib
; on this codebase. Trade-off: slower makensis build, slower install-time
; decompression (single-threaded).
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

Name           "TidaLunar"
RequestExecutionLevel user
InstallDir     "$LOCALAPPDATA\Programs\TidaLunar"
OutFile        "${OUT_DIR}\tidalunar-windows-${ARCH}-${VERSION}.exe"
Icon           "..\..\tidaluna.ico"
UninstallIcon  "..\..\tidaluna.ico"

; SetShellVarContext default is `current` for per-user installs (matches our
; RequestExecutionLevel user). It is NOT a global directive - placing it at
; top level fails makensis. No code path here needs `all`, so we never call it.

VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey  "ProductName"     "TidaLunar"
VIAddVersionKey  "FileDescription" "TidaLunar Windows Installer"
VIAddVersionKey  "FileVersion"     "${VERSION}"
VIAddVersionKey  "ProductVersion"  "${VERSION}"
VIAddVersionKey  "CompanyName"     "Inrixia"
VIAddVersionKey  "LegalCopyright"  "TidaLunar contributors. Licensed under Ms-PL."

; --- MUI2 pages ----------------------------------------------------------
!define MUI_ICON   "..\..\tidaluna.ico"
!define MUI_UNICON "..\..\tidaluna.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "LICENSE.txt"
!insertmacro MUI_PAGE_INSTFILES

!define MUI_FINISHPAGE_RUN "$INSTDIR\tidalunar.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Run TidaLunar"
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Create Desktop shortcut"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateDesktopShortcut
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; -------------------------------------------------------------------------
; SID retrieval (fail-closed). Sets $9 = current user's SID string.
; Single source of truth for both install and uninstall via macro generation.
; -------------------------------------------------------------------------
!macro DefineGetCurrentUserSid un
Function ${un}GetCurrentUserSid
  Push $0   ; process token / scratch
  Push $1   ; allocated TOKEN_USER buffer pointer
  Push $2   ; ConvertSidToStringSidW out pointer
  Push $4   ; required token-info length (kept separate from $1)
  Push $5   ; scratch return values

  System::Call 'kernel32::GetCurrentProcess() p .r0'
  System::Call 'advapi32::OpenProcessToken(p r0, i 8, *p .r0) i .r5'
  IntCmp $5 0 sid_fail

  ; First call: size the buffer. Required length lands in $4 (kept distinct
  ; from $1 - System::Alloc clobbers its size argument with the result).
  System::Call 'advapi32::GetTokenInformation(p r0, i 1, p 0, i 0, *i .r4)'
  System::Alloc $4
  Pop $1

  System::Call 'advapi32::GetTokenInformation(p r0, i 1, p r1, i r4, *i .r5) i .r5'
  System::Call 'kernel32::CloseHandle(p r0)'
  IntCmp $5 0 sid_fail_freebuf

  ; TOKEN_USER's first field is SID_AND_ATTRIBUTES whose first field is PSID.
  System::Call '*$1(p .r0)'

  System::Call 'advapi32::ConvertSidToStringSidW(p r0, *p .r2) i .r5'
  IntCmp $5 0 sid_fail_freebuf
  System::Call '*$2(&w${NSIS_MAX_STRLEN} .r9)'
  System::Call 'kernel32::LocalFree(p r2)'
  System::Free $1
  Pop $5
  Pop $4
  Pop $2
  Pop $1
  Pop $0
  Return

sid_fail_freebuf:
  System::Free $1
sid_fail:
  Pop $5
  Pop $4
  Pop $2
  Pop $1
  Pop $0
  MessageBox MB_OK|MB_ICONSTOP "Could not determine user identity for install lock. Aborting."
  Abort
FunctionEnd
!macroend

!insertmacro DefineGetCurrentUserSid ""
!insertmacro DefineGetCurrentUserSid "un."

; -------------------------------------------------------------------------
; ProbeExclusive - verifies a path is not held write-exclusive by another
; process. Missing path = success (nothing to lock - relevant for fresh
; installs and for upgrades that drop legacy files).
; In:  $R0 = path. Out: $0 = 1 if writable-or-missing, 0 if locked.
; -------------------------------------------------------------------------
!macro DefineProbeExclusive un
Function ${un}ProbeExclusive
  IfFileExists "$R0" probe_open probe_missing
  probe_missing:
    StrCpy $0 1
    Return
  probe_open:
    System::Call 'kernel32::CreateFileW(w R0, i 0x40000000, i 0, p 0, i 3, i 0x80, p 0) p .r1'
    IntCmp $1 -1 probe_locked probe_ok probe_ok
  probe_ok:
    System::Call 'kernel32::CloseHandle(p r1)'
    StrCpy $0 1
    Return
  probe_locked:
    StrCpy $0 0
FunctionEnd
!macroend

!insertmacro DefineProbeExclusive ""
!insertmacro DefineProbeExclusive "un."

; -------------------------------------------------------------------------
; ProbeOrRetry - macro that calls ProbeExclusive (with the right namespace
; prefix) and jumps to `retry` if the path is locked. Must live inside a
; function that has a `retry:` label and uses $R0 as the probe-target slot.
; -------------------------------------------------------------------------
!macro ProbeOrRetry un path
  StrCpy $R0 "${path}"
  Call ${un}ProbeExclusive
  StrCmp $0 0 retry
!macroend

; -------------------------------------------------------------------------
; WaitForHandleRelease - probe-and-wait loop covering the full file set
; that updater/src/main.rs:513-570 probes (six binaries × three layout
; variants for legacy + current install layouts).
; Aborts after 20 attempts × 500ms = ~10s if any probe is still locked.
; -------------------------------------------------------------------------
!macro DefineWaitForHandleRelease un
Function ${un}WaitForHandleRelease
  StrCpy $R3 0
  loop:
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\tidalunar.exe"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\updater.exe"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\libcef.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\chrome_elf.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\libEGL.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\libGLESv2.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bun.exe"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\cef\libcef.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\cef\chrome_elf.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\cef\libEGL.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\cef\libGLESv2.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bin\cef\libcef.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bin\cef\chrome_elf.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bin\cef\libEGL.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bin\cef\libGLESv2.dll"
    !insertmacro ProbeOrRetry "${un}" "$INSTDIR\bin\bun.exe"
    Goto done
  retry:
    Sleep 500
    IntOp $R3 $R3 + 1
    IntCmp $R3 20 give_up loop loop
  give_up:
    MessageBox MB_OK|MB_ICONSTOP "TidaLunar processes are still holding files open. Close TidaLunar and retry."
    Abort
  done:
FunctionEnd
!macroend

!insertmacro DefineWaitForHandleRelease ""
!insertmacro DefineWaitForHandleRelease "un."

; -------------------------------------------------------------------------
; AcquireInstallMutex - acquire by ownership via WaitForSingleObject.
; Caller must have called Get(un.)CurrentUserSid first ($9 = SID).
; Labels are local to the calling function/section scope, so the same macro
; can be inserted in both .onInit and Section "Uninstall" without conflict.
; Contention message is parameterised since install/uninstall phrase it
; differently.
; -------------------------------------------------------------------------
!macro AcquireInstallMutex contention_message
  mutex_acquire:
    System::Call 'kernel32::CreateMutexW(p 0, i 0, w "Global\TidaLunarInstallLock-$9") p .r2'
    IntCmp $2 0 mutex_create_failed mutex_try_wait mutex_try_wait
  mutex_create_failed:
    MessageBox MB_OK|MB_ICONSTOP "Failed to create install lock. Aborting."
    Abort
  mutex_try_wait:
    System::Call 'kernel32::WaitForSingleObject(p r2, i 0) i .r3'
    IntCmp $3 0   mutex_proceed                  ; WAIT_OBJECT_0
    IntCmp $3 128 mutex_proceed                  ; WAIT_ABANDONED
    System::Call 'kernel32::CloseHandle(p r2)'
    StrCpy $2 0
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION \
      "${contention_message}" \
      /SD IDCANCEL IDRETRY mutex_acquire
    Abort
  mutex_proceed:
!macroend

; =========================================================================
; .onInit - runs before any UI. Acquire mutex (fail-closed on SID lookup),
; verify and discard any pending updater journal that would otherwise
; interfere with this install.
; =========================================================================
Function .onInit
  Call GetCurrentUserSid                         ; sets $9 = SID, aborts on failure
  !insertmacro AcquireInstallMutex "Another TidaLunar installer or updater is running.$\nClick Retry once it finishes."
  ; $2 holds the owned mutex handle; OS releases on process exit.

  ; Discard any incomplete updater transaction so post-install recovery
  ; doesn't undo this install. Verified delete: Delete failure (AV held,
  ; ACL block) leaves the file on disk, so we abort if the file persists.
  IfFileExists "$INSTDIR\.update-journal.json" 0 journal_clear
    MessageBox MB_YESNO|MB_ICONEXCLAMATION \
      "TidaLunar has a pending update transaction.$\nContinue and discard it?$\n$\n(Recommended: cancel, launch TidaLunar once to complete recovery, then re-run installer.)" \
      /SD IDNO IDYES journal_discard
    Abort
  journal_discard:
    ClearErrors
    Delete "$INSTDIR\.update-journal.json"
    RMDir /r "$INSTDIR\.update-staging"
    IfFileExists "$INSTDIR\.update-journal.json" journal_discard_failed journal_clear
  journal_discard_failed:
    MessageBox MB_OK|MB_ICONSTOP \
      "Failed to discard pending update journal at $INSTDIR\.update-journal.json. Close TidaLunar and any antivirus blocker, then retry."
    Abort
  journal_clear:
FunctionEnd

Function CreateDesktopShortcut
  CreateShortCut "$DESKTOP\TidaLunar.lnk" "$INSTDIR\tidalunar.exe" "" "$INSTDIR\tidalunar.exe" 0
FunctionEnd

; =========================================================================
; Install Section
; =========================================================================
Section "TidaLunar" SecMain
  SectionIn RO
  SetOutPath $INSTDIR

  ; 1. Stop running TidaLunar + its updater. Path-scoped via tracked .ps1
  ;    extracted to $PLUGINSDIR. taskkill /IM by image name would terminate
  ;    foreign Adobe/Office/Brave updater.exe processes. The .ps1 uses
  ;    StartsWith with a trailing-backslash boundary so sibling installs
  ;    like ...\TidaLunar-beta\ are untouched.
  SetOutPath $PLUGINSDIR
  File "kill-installdir-procs.ps1"
  SetOutPath $INSTDIR
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden \
    -File "$PLUGINSDIR\kill-installdir-procs.ps1" -InstallDir "$INSTDIR"'

  ; 2. Wait for kernel to release write handles. Probe set includes
  ;    updater.exe (we're about to overwrite + execute it).
  Call WaitForHandleRelease

  ; 3. Re-check journal - legacy updaters not yet honoring the install
  ;    mutex could have started AFTER .onInit and been killed mid-write
  ;    by step 1. Verified delete; abort on persistence.
  IfFileExists "$INSTDIR\.update-journal.json" 0 journal_recheck_clear
    DetailPrint "Discarding journal written after .onInit (legacy updater transition guard)."
    Delete "$INSTDIR\.update-journal.json"
    RMDir /r "$INSTDIR\.update-staging"
    IfFileExists "$INSTDIR\.update-journal.json" journal_recheck_failed journal_recheck_clear
  journal_recheck_failed:
    DetailPrint "Failed to discard journal at recheck; aborting install."
    FileOpen $1 "$INSTDIR\install-cleanup-warning.log" w
    FileWrite $1 "journal recheck failed: file remains at $INSTDIR\.update-journal.json$\r$\n"
    FileClose $1
    MessageBox MB_OK|MB_ICONSTOP \
      "Could not discard pending update journal. Close TidaLunar processes and antivirus blockers, then retry."
    Abort
  journal_recheck_clear:

  ; 4. Save old manifest aside so cleanup can compare old vs new.
  ;
  ; Idempotency guard: if manifest.old.json from a previous failed cleanup
  ; still exists, run cleanup against it FIRST, using the still-current
  ; manifest.json. Catches files removed between v1->v2 (preserved when v2's
  ; cleanup failed) so the chained v1->v2->v3 install doesn't strand them.
  ; Best-effort: drop the stale backup either way.
  IfFileExists "$INSTDIR\manifest.old.json" 0 manifest_save
  IfFileExists "$INSTDIR\manifest.json" 0 manifest_old_drop
    DetailPrint "Found leftover manifest.old.json from failed cleanup; retrying."
    nsExec::Exec '"$INSTDIR\updater.exe" --cleanup-stale --app-dir "$INSTDIR" \
                  --old-manifest "$INSTDIR\manifest.old.json" \
                  --new-manifest "$INSTDIR\manifest.json"'
    Pop $0
    ; Both error-string and any non-zero exit fall through to the unconditional drop below.
  manifest_old_drop:
    Delete "$INSTDIR\manifest.old.json"
  manifest_save:
    ClearErrors
    IfFileExists "$INSTDIR\manifest.json" 0 manifest_rename_done
      Rename "$INSTDIR\manifest.json" "$INSTDIR\manifest.old.json"
      IfErrors manifest_rename_failed manifest_rename_done
  manifest_rename_failed:
    DetailPrint "Could not preserve old manifest; cleanup will be skipped this run."
  manifest_rename_done:

  ; 5. Copy new bundle. File /r overlays files; orphans (files removed in
  ;    new version) survive this step and are cleaned in step 7.
  File /r "${DIST_DIR}\*"

  ; 6. Post-copy write-handle probe: confirms File /r's write handles have
  ;    been released. NOT a proof of CreateProcess readiness - Defender
  ;    quarantine, AppLocker, or WDAC can still reject the launch
  ;    independently of file handles. The cleanup invocation below treats
  ;    nsExec failure as non-fatal (preserves manifest.old.json for retry);
  ;    that's the actual safety net for those cases.
  Call WaitForHandleRelease

  ; 7. Manifest-diff cleanup. Transactional: keep manifest.old.json if the
  ;    helper fails (launch failure, transient AV scan, version skew,
  ;    missing --cleanup-stale in older builds). Only delete on numeric
  ;    exit code 0. Guard against the literal string "error" pushed by
  ;    nsExec::Exec on launch failure (NSIS IntCmp coerces non-numeric
  ;    strings to 0 and would otherwise mask launch failures as success).
  IfFileExists "$INSTDIR\manifest.old.json" 0 skip_cleanup
    nsExec::Exec '"$INSTDIR\updater.exe" --cleanup-stale --app-dir "$INSTDIR" \
                  --old-manifest "$INSTDIR\manifest.old.json" \
                  --new-manifest "$INSTDIR\manifest.json"'
    Pop $0
    StrCmp $0 "error" cleanup_failed
    IntCmp $0 0 cleanup_succeeded cleanup_failed cleanup_failed
  cleanup_succeeded:
    Delete "$INSTDIR\manifest.old.json"
    Goto skip_cleanup
  cleanup_failed:
    DetailPrint "Cleanup helper failed (exit/result: $0); preserving manifest.old.json for next install attempt."
    ; In silent (/S) mode, DetailPrint is invisible - mirror to a log file.
    FileOpen $1 "$INSTDIR\install-cleanup-warning.log" w
    FileWrite $1 "cleanup-stale failed: $0$\r$\n"
    FileClose $1
  skip_cleanup:

  ; 8. Start Menu shortcut (always).
  CreateDirectory "$SMPROGRAMS\TidaLunar"
  CreateShortCut "$SMPROGRAMS\TidaLunar\TidaLunar.lnk" \
    "$INSTDIR\tidalunar.exe" "" "$INSTDIR\tidalunar.exe" 0

  ; 9. Add/Remove Programs registration (HKCU, per-user) + uninstaller.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "DisplayName"          "TidaLunar"
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "DisplayVersion"       "${VERSION}"
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "DisplayIcon"          "$INSTDIR\tidalunar.exe,0"
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "Publisher"            "Inrixia"
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "InstallLocation"      "$INSTDIR"
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "UninstallString"      '"$INSTDIR\Uninstall.exe"'
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegStr   HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "URLInfoAbout"         "https://github.com/Inrixia/TidaLuna"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "EstimatedSize"        $0
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "NoModify"             1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar" "NoRepair"             1

  WriteUninstaller "$INSTDIR\Uninstall.exe"
SectionEnd

; =========================================================================
; Uninstall Section
; =========================================================================
Var RemoveUserData

Function un.onInit
  ; Prompt up front (default No on /S silent uninstalls - preserve user data).
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "Also remove TidaLunar user data (cache, settings, plugins) from %LOCALAPPDATA%\tidalunar?$\n$\nDefault is No (preserve)." \
    /SD IDNO IDYES un_remove_userdata
  StrCpy $RemoveUserData 0
  Return
  un_remove_userdata:
  StrCpy $RemoveUserData 1
FunctionEnd

Section "Uninstall"
  Call un.GetCurrentUserSid                      ; sets $9 = SID, aborts on failure
  !insertmacro AcquireInstallMutex "TidaLunar is running or updating. Close it and click Retry."

  ; Path-scoped kill. taskkill /IM updater.exe would match unrelated apps.
  SetOutPath $PLUGINSDIR
  File "kill-installdir-procs.ps1"
  nsExec::ExecToLog 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden \
    -File "$PLUGINSDIR\kill-installdir-procs.ps1" -InstallDir "$INSTDIR"'
  Call un.WaitForHandleRelease

  RMDir /r "$INSTDIR"

  Delete "$SMPROGRAMS\TidaLunar\TidaLunar.lnk"
  RMDir  "$SMPROGRAMS\TidaLunar"
  Delete "$DESKTOP\TidaLunar.lnk"

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\TidaLunar"

  ${If} $RemoveUserData == 1
    RMDir /r "$LOCALAPPDATA\tidalunar"
  ${EndIf}
SectionEnd
