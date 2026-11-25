; Lets peers in the same network reach Mycel directly (P2P over UDP, the anchor's relay over TCP).
; Without it Windows drops their packets on a "Public" network and only relays work.
; The installer runs without admin rights, so adding the rule asks for them once (UAC).
!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec 'netsh advfirewall firewall show rule name=Mycel'
  Pop $0
  ${If} $0 != 0
    ExecShellWait "runas" "netsh" 'advfirewall firewall add rule name=Mycel dir=in action=allow profile=any program="$INSTDIR\mycel-app.exe" enable=yes' SW_HIDE
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'netsh advfirewall firewall show rule name=Mycel'
  Pop $0
  ${If} $0 == 0
    ExecShellWait "runas" "netsh" 'advfirewall firewall delete rule name=Mycel' SW_HIDE
  ${EndIf}
!macroend
