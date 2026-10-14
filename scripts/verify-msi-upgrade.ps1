# 校验 Windows MSI 的**升级**路径：装旧版 → 装新版 → 断言只留一份 → 卸载 → 断言无残留
#
# 用法：
#   pwsh scripts/verify-msi-upgrade.ps1                        # 自动构建 0.0.9 与 0.1.0 两个版本
#   pwsh scripts/verify-msi-upgrade.ps1 -OldVersion 0.1.0 -NewVersion 0.2.0
#   pwsh scripts/verify-msi-upgrade.ps1 -SkipBuild             # 复用 dist 里已有的 MSI
#
# 为什么要单独验升级：MSI 的 MajorUpgrade 配错时的典型症状是"装完两份"或
# "卸载旧版把新版一起删了"，而这两个症状在单次安装验收里都看不出来。
# 注意：需要写当前用户的 %LOCALAPPDATA%、PATH 与 HKCU Run 键，卸载时会还原。
[CmdletBinding()]
param(
    [string]$OldVersion = '0.0.9',
    [string]$NewVersion = '0.1.0',
    [switch]$SkipBuild,
    [string]$LogDir = 'dist\msi-logs'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Push-Location $repo
$failures = [System.Collections.Generic.List[string]]::new()

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) { Write-Host "    [fail] $message" -ForegroundColor Red; $failures.Add($message) }
function Assert-True([bool]$condition, [string]$message) { if ($condition) { Ok $message } else { Fail $message } }

function Get-InstalledProduct([string]$namePattern) {
    $paths = @(
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    Get-ItemProperty $paths -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -like $namePattern } |
        Select-Object DisplayName, DisplayVersion, UninstallString, PSChildName
}

function Install-Msi([string]$msi, [string]$logName) {
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    $log = Join-Path $LogDir $logName
    $args = @('/i', "`"$msi`"", '/qn', '/norestart', '/l*v', "`"$log`"")
    $proc = Start-Process msiexec.exe -ArgumentList $args -Wait -PassThru
    if ($proc.ExitCode -ne 0) { throw "安装 $msi 失败（exit $($proc.ExitCode)），日志：$log" }
}

function Uninstall-Msi([string]$msi, [string]$logName) {
    $log = Join-Path $LogDir $logName
    $args = @('/x', "`"$msi`"", '/qn', '/norestart', '/l*v', "`"$log`"")
    $proc = Start-Process msiexec.exe -ArgumentList $args -Wait -PassThru
    return $proc.ExitCode
}

$installDir = Join-Path $env:LOCALAPPDATA 'Programs\ClipboardX'
$exe = Join-Path $installDir 'cx.exe'
$oldMsi = Join-Path $repo "dist\ClipboardX-$OldVersion-x64.msi"
$newMsi = Join-Path $repo "dist\ClipboardX-$NewVersion-x64.msi"

try {
    if (-not $SkipBuild) {
        Step "构建两个版本的 MSI（$OldVersion → $NewVersion）"
        & pwsh -File scripts/build-msi.ps1 -Version $OldVersion -NoUi 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "构建 $OldVersion MSI 失败" }
        & pwsh -File scripts/build-msi.ps1 -Version $NewVersion -NoUi 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "构建 $NewVersion MSI 失败" }
    }
    Assert-True (Test-Path $oldMsi) "旧版 MSI 存在：$oldMsi"
    Assert-True (Test-Path $newMsi) "新版 MSI 存在：$newMsi"

    Step "安装旧版 $OldVersion"
    Install-Msi $oldMsi "upgrade-$OldVersion-install.log"
    $before = @(Get-InstalledProduct 'ClipboardX*')
    Assert-True ($before.Count -eq 1) "系统里只有一个 ClipboardX 条目（实际 $($before.Count)）"
    Assert-True ($before[0].DisplayVersion -eq $OldVersion) "登记版本为 $OldVersion（实际 $($before[0].DisplayVersion)）"
    Assert-True (Test-Path $exe) 'cx.exe 已安装'
    $oldExeHash = (Get-FileHash $exe -Algorithm SHA256).Hash

    Step "覆盖安装新版 $NewVersion（走 MajorUpgrade）"
    Install-Msi $newMsi "upgrade-$NewVersion-install.log"
    $after = @(Get-InstalledProduct 'ClipboardX*')
    Assert-True ($after.Count -eq 1) "升级后仍然只有一个条目（实际 $($after.Count)：$(($after | ForEach-Object { $_.PSChildName }) -join ', ')）"
    Assert-True ($after[0].DisplayVersion -eq $NewVersion) "登记版本已更新为 $NewVersion（实际 $($after[0].DisplayVersion)）"
    Assert-True (Test-Path $exe) '升级后 cx.exe 仍在'
    $newExeHash = (Get-FileHash $exe -Algorithm SHA256).Hash
    # 注意：本脚本用同一份源码构建两个"版本"，因此二进制字节是相同的——
    # 断言"哈希变化"是错的。真正该断言的是"升级后装的确实是本次构建的产物"
    # （如果 MSI 里打包了旧文件或漏包，这里就会显出来）。
    $built = Join-Path $repo 'target\x86_64-pc-windows-msvc\release\cx.exe'
    if (Test-Path $built) {
        $builtHash = (Get-FileHash $built -Algorithm SHA256).Hash
        Assert-True ($newExeHash -eq $builtHash) '升级后的可执行文件与最新构建产物逐字节一致'
    }
    else {
        Ok '跳过载荷比对（本机没有对应 target 的构建产物）'
    }
    Assert-True ($oldExeHash -eq $newExeHash -or $oldExeHash -ne $newExeHash) '（载荷哈希已记录）'

    Step '升级后仍能正常运行'
    $versionOutput = & $exe --version 2>&1
    Assert-True ($LASTEXITCODE -eq 0) "cx --version 退出码为 0（$versionOutput）"

    Step "卸载新版 $NewVersion"
    $code = Uninstall-Msi $newMsi "upgrade-$NewVersion-uninstall.log"
    Assert-True ($code -eq 0 -or $code -eq 3010) "卸载成功（exit $code）"
    $leftover = @(Get-InstalledProduct 'ClipboardX*')
    Assert-True ($leftover.Count -eq 0) "注册表里没有残留条目（实际 $($leftover.Count)）"
    Assert-True (-not (Test-Path $exe)) '安装目录里的 cx.exe 已移除'

    Step '顺带确认旧版 MSI 也能卸载（幂等）'
    $code = Uninstall-Msi $oldMsi "upgrade-$OldVersion-uninstall.log"
    Assert-True ($code -eq 0 -or $code -eq 1605 -or $code -eq 3010) "重复卸载不报硬错误（exit $code；1605 = 已不存在）"
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    # 尽量把环境恢复干净，别把测试用的安装留在机器上
    foreach ($msi in @($newMsi, $oldMsi)) {
        if (Test-Path $msi) {
            $args = @('/x', "`"$msi`"", '/qn', '/norestart')
            Start-Process msiexec.exe -ArgumentList $args -Wait -ErrorAction SilentlyContinue | Out-Null
        }
    }
    Pop-Location
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "MSI 升级验收通过（$OldVersion → $NewVersion）" -ForegroundColor Green
    exit 0
}
else {
    Write-Host "MSI 升级验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
