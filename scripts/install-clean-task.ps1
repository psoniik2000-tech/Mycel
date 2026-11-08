# Registers the "MycelClean" task: runs clean.ps1 hidden every 15 minutes while you are signed in. Remove: Unregister-ScheduledTask MycelClean
$script = Join-Path $PSScriptRoot 'clean.ps1'
$a = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$script`""
$t = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 15)
$p = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
$s = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries -AllowStartIfOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 10)
Register-ScheduledTask MycelClean -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null
"MycelClean registered"
