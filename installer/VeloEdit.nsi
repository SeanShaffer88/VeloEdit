; VeloEdit — NSIS installer script (SPEC §6.1)
;
; Produces: VeloEdit-v<VERSION>-x64-Setup.exe
;
; Per-user install (no administrator prompt): the default and only install
; location root is $LOCALAPPDATA\Programs\VeloEdit, and the Add/Remove
; Programs entry is written under HKCU, so installing never needs elevation.
;
; Build:
;   makensis /DEXE_PATH="path\to\VeloEdit.exe" /DOUT_DIR="path\to\out" VeloEdit.nsi
;
; EXE_PATH points at the VeloEdit.exe to embed (default: the repo-relative
; MSVC release build). CI points it at the static-CRT MSVC build; release
; assets are never built from another target.
; OUT_DIR is where the Setup exe is written (default: this script's folder).

!ifndef EXE_PATH
  !define EXE_PATH "..\target\x86_64-pc-windows-msvc\release\VeloEdit.exe"
!endif
!ifndef OUT_DIR
  !define OUT_DIR "."
!endif

!define VERSION "1.0.1"
!define APPNAME "VeloEdit"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\VeloEdit"

Unicode true
Name "${APPNAME} ${VERSION}"
OutFile "${OUT_DIR}\VeloEdit-v${VERSION}-x64-Setup.exe"
InstallDir "$LOCALAPPDATA\Programs\VeloEdit"
RequestExecutionLevel user
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APPNAME} Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT License"

; ---------------------------------------------------------------------------
; Pages
; ---------------------------------------------------------------------------
!include "MUI2.nsh"

!define MUI_ABORTWARNING
; The program icon for the installer and uninstaller windows (and the
; Setup exe itself). These defines must precede the MUI page macros.
!define MUI_ICON "..\assets\veloedit.ico"
!define MUI_UNICON "..\assets\veloedit.ico"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_CONFIRM
; Uninstall options page: one checkbox — also remove the portable data folder
; (settings, recent files, recovery snapshot) which lives next to the program.
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH

!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------------------
; Install
; ---------------------------------------------------------------------------
Section "VeloEdit" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "/oname=VeloEdit.exe" "${EXE_PATH}"
  File "/oname=LICENSE" "..\LICENSE"
  ; The portable data folder belongs to the user, not to the program: create
  ; it empty so first run finds its home; never overwrite its contents.
  CreateDirectory "$INSTDIR\data"

  ; Start Menu shortcut
  CreateDirectory "$SMPROGRAMS\VeloEdit"
  CreateShortcut "$SMPROGRAMS\VeloEdit\VeloEdit.lnk" "$INSTDIR\VeloEdit.exe"

  ; Uninstaller + Add/Remove Programs entry (per-user, HKCU — no admin)
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Sean Shaffer"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\VeloEdit.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

; ---------------------------------------------------------------------------
; Uninstall
; ---------------------------------------------------------------------------
Section "un.VeloEdit program files" UnSecMain
  SectionIn RO
  Delete "$INSTDIR\VeloEdit.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\VeloEdit\VeloEdit.lnk"
  RMDir "$SMPROGRAMS\VeloEdit"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  ; Removes the install folder itself only when it is already empty (the
  ; data folder below can keep it non-empty when left behind).
  RMDir "$INSTDIR"
SectionEnd

Section /o "un.Also remove the data folder (settings and recovery)" UnSecData
  ; $INSTDIR\data holds settings.json and any recovery snapshot. Removing it
  ; is opt-in only — the checkbox above is off by default, so uninstalling
  ; the program never silently deletes the user's settings.
  RMDir /r "$INSTDIR\data"
  RMDir "$INSTDIR"
SectionEnd

Function un.onInit
  ; The data-folder option starts unchecked; "un." sections named with /o
  ; above handle that. Nothing else to prepare.
FunctionEnd
