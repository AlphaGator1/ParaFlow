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
  Delete "$SMPROGRAMS\$AppStartMenuFolder\${PRODUCTNAME} User Guide.lnk"
  Delete "$SMPROGRAMS\${PRODUCTNAME} User Guide.lnk"
!macroend
