; Installer hooks for ParaFlow.
;
; Tauri's NSIS template creates one Start-menu shortcut, for the executable.
; The user guide ships beside it as a bundled resource, and a file sitting in
; C:\Program Files that nothing links to is a file nobody finds - so give it
; its own entry, right next to ParaFlow's, and take it away again on uninstall.
;
; POSTINSTALL runs after CreateOrUpdateStartMenuShortcut, so $AppStartMenuFolder
; is already resolved by the time these macros fire.

!macro NSIS_HOOK_POSTINSTALL
  ; $UpdateMode is 1 when an existing install is being upgraded in place, in
  ; which case the shortcut is already there and Tauri deliberately leaves
  ; shortcuts alone. $NoShortcutMode is the /NS switch. Honour both, so this
  ; hook cannot resurrect a shortcut the user asked not to have.
  ${If} $UpdateMode <> 1
  ${AndIf} $NoShortcutMode <> 1
    ; Match the install mode explicitly rather than inheriting whatever context
    ; happens to be set - see the note in POSTUNINSTALL for why inheriting it
    ; is not safe.
    !if "${INSTALLMODE}" == "perMachine"
      SetShellVarContext all
    !else
      SetShellVarContext current
    !endif
    !if "${STARTMENUFOLDER}" != ""
      CreateShortcut "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME} User Guide.lnk" \
        "$INSTDIR\${PRODUCTNAME} User Guide.html"
    !else
      CreateShortcut "$SMPROGRAMS\${PRODUCTNAME} User Guide.lnk" \
        "$INSTDIR\${PRODUCTNAME} User Guide.html"
    !endif
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Not gated on $UpdateMode: an update replaces the shortcut in POSTINSTALL
  ; anyway, and a stale link pointing into a directory that no longer exists is
  ; worse than a missing one.
  ;
  ; Deleted from BOTH shell contexts, explicitly, because the context here is
  ; not ours and cannot be assumed. Tauri runs `SetShellVarContext current`
  ; inside its delete-app-data branch, which executes *before* this hook is
  ; inserted - so with that checkbox ticked $SMPROGRAMS quietly means the
  ; per-user Start Menu while a perMachine install put the shortcut in the
  ; machine-wide one. That left the shortcut behind on exactly one path, which
  ; is the kind of conditional miss that survives casual testing. Deleting from
  ; both contexts cannot be wrong under any install mode.
  SetShellVarContext all
  Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME} User Guide.lnk"
  Delete "$SMPROGRAMS\${PRODUCTNAME} User Guide.lnk"
  SetShellVarContext current
  Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME} User Guide.lnk"
  Delete "$SMPROGRAMS\${PRODUCTNAME} User Guide.lnk"

  ; --- make "delete application data" actually mean something ---------------
  ;
  ; Tauri's template honours that checkbox by removing $LOCALAPPDATA\<bundle
  ; id>, which for us is `com.paraflow.dictation` - a directory that has never
  ; existed. Everything real lives in $LOCALAPPDATA\ParaFlow, so ticking the box
  ; removed nothing at all and left several gigabytes behind while reporting
  ; success.
  ;
  ; Keeping the models across an uninstall is genuinely what most people want,
  ; since a reinstall would otherwise re-download them. The problem was only
  ; ever that the checkbox promised otherwise. So: ask plainly, name the
  ; directory and what is in it, and default to keeping.
  ;
  ; Tauri has already called SetShellVarContext current by the time this runs,
  ; but only on the ticked path - set it here too so $LOCALAPPDATA cannot
  ; resolve against the wrong profile.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    ${If} ${FileExists} "$LOCALAPPDATA\${PRODUCTNAME}\*.*"
      MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 \
        "Also delete ${PRODUCTNAME}'s downloaded data?$\n$\n\
         This permanently removes, from$\n\
         $LOCALAPPDATA\${PRODUCTNAME}$\n$\n\
         $\t- the speech recognition and cleanup models (several GB)$\n\
         $\t- your settings$\n\
         $\t- your personal dictionary$\n$\n\
         Installing ${PRODUCTNAME} again later would download the models again.$\n$\n\
         Choose No to keep them." \
        IDNO ParaFlowKeepData
        DetailPrint "Removing $LOCALAPPDATA\${PRODUCTNAME}"
        RMDir /r "$LOCALAPPDATA\${PRODUCTNAME}"
      ParaFlowKeepData:
    ${EndIf}
  ${EndIf}
!macroend
