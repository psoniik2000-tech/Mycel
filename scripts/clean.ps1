# Keeps build junk (cargo/gradle outputs) under a limit: deletes the least recently used dirs first.
# Kept: target\release (installer), top-level files in target\ (APK). Skips while a build runs.
# Run by the "MycelClean" scheduled task (scripts\install-clean-task.ps1); safe to run by hand.
param([double]$LimitGB = 5, [string]$Root = (Split-Path $PSScriptRoot -Parent))

if (Get-Process cargo, rustc -ErrorAction SilentlyContinue) { exit }

function Size($p) {
    $s = 0
    try { foreach ($f in [IO.Directory]::EnumerateFiles($p, '*', 'AllDirectories')) { $s += ([IO.FileInfo]$f).Length } } catch {}
    $s
}

# Worktree folders git no longer knows about (finished sessions) are junk as a whole.
$known = git -C $Root worktree list --porcelain | ? { $_ -like 'worktree *' } | % { (Resolve-Path $_.Substring(9)).Path }
$wts = Get-ChildItem "$Root\.claude\worktrees" -Directory -ErrorAction SilentlyContinue
$orphans = $wts | ? { $known -notcontains $_.FullName }

$dirs = @()
$targets = @("$Root\target") + ($wts | ? { $known -contains $_.FullName } | % { "$($_.FullName)\target" })
foreach ($t in $targets) {
    $dirs += Get-ChildItem $t -Directory -ErrorAction SilentlyContinue | ? { -not ($_.Name -eq 'release' -and $t -eq "$Root\target") }
}
$dirs += Get-ChildItem "$Root\apps\mycel-app\src-tauri\gen\android" -Recurse -Directory -Filter build -Depth 2 -ErrorAction SilentlyContinue

$items = @($orphans) + @($dirs) | ? { $_ } | % { [pscustomobject]@{ Path = $_.FullName; Size = (Size $_.FullName); Used = $_.LastWriteTime } }
$total = ($items | Measure-Object Size -Sum).Sum
$limit = $LimitGB * 1GB
$log = "$Root\target\clean.log"
foreach ($i in $items | Sort-Object { $orphans.FullName -notcontains $_.Path }, Used) {
    if ($total -le $limit) { break }
    cmd /c rd /s /q "\\?\$($i.Path)" 2>$null
    $total -= $i.Size
    Add-Content $log ("{0:yyyy-MM-dd HH:mm} removed {1:N1} GB {2}" -f (Get-Date), ($i.Size / 1GB), $i.Path)
}
