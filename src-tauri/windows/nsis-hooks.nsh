; Migrate the legacy NSIS package in place when the product display name changes.
; Keep the old install directory and app data, then let the new installer write
; the same hex-motor-gui.exe under the new hexmeow-gui product registration.
!macro NSIS_HOOK_PREINSTALL
  ReadRegStr $R8 SHCTX "Software\hexmecha\hex-motor-gui" ""
  ReadRegStr $R9 SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\hex-motor-gui" "UninstallString"
  StrCmp $R8 "" hexmeow_gui_migration_done
  StrCmp $R9 "" hexmeow_gui_migration_done

  DetailPrint "Migrating the existing hex-motor-gui installation"
  ExecWait '$R9 /S /UPDATE _?=$R8' $R7
  StrCmp $R7 "0" hexmeow_gui_migration_ok
  Abort "Could not migrate the existing hex-motor-gui installation (exit code $R7)."

  hexmeow_gui_migration_ok:
  StrCpy $INSTDIR $R8
  SetOutPath "$INSTDIR"
  Delete "$SMPROGRAMS\hex-motor-gui.lnk"
  Delete "$DESKTOP\hex-motor-gui.lnk"

  hexmeow_gui_migration_done:
!macroend
