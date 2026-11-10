<#
  Installer upgrade/uninstall test (Windows, elevated; changes the machine, meant for CI runners).

  1. Installs -OldMsi, seeds the runtime data the service keeps outside the package
     (config, battery history, icon, shortcuts, logs, downloaded framework_tool.exe).
  2. Upgrades to -NewMsi the way the in-app updater does (ADDLOCAL=Binaries) and checks that
     none of that data was deleted, that the new exe replaced the old one, and that exactly one
     product is registered with the service running.
  3. Uninstalls and checks that the data is cleaned up.
  4. Fresh install of -NewMsi, then uninstall, and the same cleanup checks.

  The data checks read the verbose MSI logs: the service recreates some of these files on its
  own, so their presence after an upgrade proves little. Use -CheckUpgradeLog alone to run the
  upgrade-log assertions against an existing log.
#>
param(
    [string]$OldMsi,
    [string]$NewMsi,
    [string]$NewExe,
    [string]$LogDir = "$PWD\msi-logs",
    [string]$CheckUpgradeLog
)
$ErrorActionPreference = 'Stop'

$CleanupComponent = '{B6F3C2A1-8D45-4E9C-A3B7-1F0E5D2C7A84}'
$DataNames = 'config\.json|battery-history\.json|battery-history\.json\.tmp|framework-control\.ico|Framework Control\.(lnk|url)|framework_tool\.exe|[^,]*\.log|[^,]*\.log\.old'
$DataRemoveOp = "Executing op: FileRemove\(,FileName=($DataNames),"

$script:failures = 0
function Check([bool]$ok, [string]$what) {
    if ($ok) { Write-Host "  ok    $what" } else { $script:failures++; Write-Host "  FAIL  $what" }
}

# Assertions on the verbose log of an upgrade (new package installed over an old one).
function Test-UpgradeLog([string]$log) {
    $removed = @(Select-String -Path $log -Pattern $DataRemoveOp | ForEach-Object { $_.Matches[0].Groups[1].Value } | Sort-Object -Unique)
    Check ($removed.Count -eq 0) "upgrade deleted no user data$(if ($removed) { ': deleted ' + ($removed -join ', ') })"
    $kept = Select-String -Path $log -SimpleMatch "Disallowing uninstallation of component: $CleanupComponent"
    Check ($null -ne $kept) "old product left the RuntimeDataCleanup component in place (shared with the new product)"
}

if ($CheckUpgradeLog) {
    Write-Host "[upgrade log] $CheckUpgradeLog"
    Test-UpgradeLog $CheckUpgradeLog
    if ($script:failures) { Write-Host "RESULT: $($script:failures) failure(s)"; exit 1 }
    Write-Host 'RESULT: ok'; exit 0
}

$InstallDir = Join-Path $env:ProgramFiles 'FrameworkControl'
$DataDir = Join-Path $env:ProgramData 'FrameworkControl'
$StartMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'
$Desktop = Join-Path $env:PUBLIC 'Desktop'
$ServiceName = 'FrameworkControlService'
New-Item -ItemType Directory -Force $LogDir | Out-Null

function Invoke-Msi([string[]]$msiArgs, [string]$log) {
    $p = Start-Process msiexec.exe -ArgumentList ($msiArgs + @('/qn', '/norestart', '/l*v', "`"$log`"")) -Wait -PassThru
    return $p.ExitCode
}

function Get-Products {
    @(Get-ItemProperty 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -eq 'Framework Laptop Control Service' })
}

function Wait-ServiceRunning {
    for ($i = 0; $i -lt 60; $i++) {
        $s = Get-Service $ServiceName -ErrorAction SilentlyContinue
        if ($s -and $s.Status -eq 'Running') { return $true }
        Start-Sleep 1
    }
    return $false
}

function Seed-Data {
    Stop-Service $ServiceName -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force "$DataDir\assets" | Out-Null
    '{"telemetry":{"poll_ms":4321}}' | Set-Content -Encoding utf8 "$DataDir\config.json"
    'seed' | Set-Content "$DataDir\battery-history.json"
    'seed' | Set-Content "$DataDir\assets\framework-control.ico"
    'seed' | Set-Content "$StartMenu\Framework Control.lnk"
    'seed' | Set-Content "$Desktop\Framework Control.url"
    if (-not (Test-Path "$InstallDir\framework_tool.exe")) { 'seed' | Set-Content "$InstallDir\framework_tool.exe" }
    'seed' | Set-Content "$InstallDir\FrameworkControlService.out.log.old"
    Start-Service $ServiceName
}

function Test-DataPresent {
    Check ((Test-Path "$DataDir\config.json") -and (Select-String -Path "$DataDir\config.json" -SimpleMatch '4321' -Quiet)) 'config.json kept (custom poll_ms intact)'
    Check (Test-Path "$DataDir\assets\framework-control.ico") 'shortcut icon kept'
    Check (Test-Path "$StartMenu\Framework Control.lnk") 'Start Menu shortcut kept'
    Check (Test-Path "$Desktop\Framework Control.url") 'Desktop shortcut kept'
    Check (Test-Path "$InstallDir\FrameworkControlService.out.log.old") 'rolled service log kept'
}

function Test-Uninstall([string]$label) {
    $products = Get-Products
    Check ($products.Count -eq 1) "$label`: exactly one product registered before uninstall ($($products.Count))"
    if ($products.Count -lt 1) { return }
    $log = Join-Path $LogDir "$label-uninstall.log"
    $code = Invoke-Msi @('/x', $products[0].PSChildName) $log
    Check ($code -eq 0) "$label`: uninstall exit code $code"
    Check ((Get-Products).Count -eq 0) "$label`: no product registered after uninstall"
    Check ($null -eq (Get-Service $ServiceName -ErrorAction SilentlyContinue)) "$label`: service removed"
    foreach ($p in @("$DataDir\config.json", "$DataDir\battery-history.json", "$DataDir\assets\framework-control.ico",
            "$StartMenu\Framework Control.lnk", "$Desktop\Framework Control.url", "$InstallDir\framework_tool.exe",
            "$InstallDir\FrameworkControlService.out.log", "$InstallDir\FrameworkControlService.out.log.old")) {
        Check (-not (Test-Path $p)) "$label`: removed $p"
    }
    Check ($null -ne (Select-String -Path $log -Pattern 'Executing op: FileRemove\(,FileName=config\.json,')) "$label`: uninstall log removes config.json"
}

$newHash = (Get-FileHash $NewExe).Hash

Write-Host '[1] install old package'
$code = Invoke-Msi @('/i', "`"$OldMsi`"") (Join-Path $LogDir '1-install-old.log')
Check ($code -eq 0) "install exit code $code"
Check (Wait-ServiceRunning) 'old service running'
Start-Sleep 5   # let it create its logs
Seed-Data

Write-Host '[2] upgrade to new package (as the in-app updater does)'
$upgradeLog = Join-Path $LogDir '2-upgrade.log'
$code = Invoke-Msi @('/i', "`"$NewMsi`"", 'ADDLOCAL=Binaries') $upgradeLog
Check ($code -eq 0) "upgrade exit code $code (3010 would mean a file was in use and not replaced)"
Check ((Get-FileHash "$InstallDir\framework-control-service.exe").Hash -eq $newHash) 'installed exe is the new build'
Check ((Get-Products).Count -eq 1) "exactly one product registered ($((Get-Products).Count))"
Check (Wait-ServiceRunning) 'service running after upgrade'
Test-UpgradeLog $upgradeLog
Test-DataPresent

Write-Host '[3] uninstall upgraded product'
Test-Uninstall '3'

Write-Host '[4] fresh install of new package, then uninstall'
$code = Invoke-Msi @('/i', "`"$NewMsi`"") (Join-Path $LogDir '4-install-new.log')
Check ($code -eq 0) "fresh install exit code $code"
Check (Wait-ServiceRunning) 'service running after fresh install'
Seed-Data
Test-Uninstall '4'

if ($script:failures) { Write-Host "RESULT: $($script:failures) failure(s)"; exit 1 }
Write-Host 'RESULT: ok'
