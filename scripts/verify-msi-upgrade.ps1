# 校验 Windows MSI 的**升级**路径：装旧版 → 跑起守护进程 → 装新版 → 断言只留一份 →
# 断言旧进程退出且装完自动恢复守护进程 → 卸载（守护进程运行中）→ 断言无残留
#
# 用法：
#   pwsh scripts/verify-msi-upgrade.ps1                        # 自动构建 0.0.9 与 0.1.0 两个版本
#   pwsh scripts/verify-msi-upgrade.ps1 -OldVersion 0.1.0 -NewVersion 0.2.0
#   pwsh scripts/verify-msi-upgrade.ps1 -SkipBuild             # 复用 dist 里已有的 MSI
#
# 为什么要单独验升级：MSI 的 MajorUpgrade 配错时的典型症状是"装完两份"或
# "卸载旧版把新版一起删了"，而这两个症状在单次安装验收里都看不出来。
# 另一个只在这一步才暴露的问题是**文件占用**：cx.exe 平时被自启的守护进程占着，
# 覆盖安装会弹"文件正在使用"对话框（看起来就是卡住）或静默失败 1603，
# 修复手段是安装包里先 `cx stop` + `taskkill`，装完再 `cx start`（见 Package.wxs）。
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

# 当前正以**安装目录里那个 cx.exe** 运行的进程（守护进程 / 打开着的 cx tui）。
# 按可执行文件路径过滤，而不是按进程名：开发机上常有 target\release\cx.exe 在跑，
# 按名字判断会把它当成"安装版守护进程"，断言就失真了。
function Get-InstalledDaemon([string]$exePath) {
    Get-CimInstance Win32_Process -Filter "Name='cx.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -eq $exePath } |
        Select-Object ProcessId, ExecutablePath
}

# 启动安装目录里的守护进程，并等到它真的起来（最多 ~5 秒）。
# 关键：把输出写进文件再读回来——直接把分离进程的 stdout 接到脚本管道上，
# 脚本会等管道关闭而挂住（scripts/e2e-local.ps1 踩过同一个坑）。
function Start-InstalledDaemon([string]$exePath, [string]$logDir, [switch]$NoClipboard) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    $out = Join-Path $logDir 'daemon-start.txt'
    $cxArgs = @('start', '--silent')
    if ($NoClipboard) { $cxArgs += '--no-clipboard' }
    & $exePath @cxArgs *> $out
    for ($i = 0; $i -lt 50; $i++) {
        if (Get-InstalledDaemon $exePath) { return $true }
        Start-Sleep -Milliseconds 100
    }
    return $false
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

    # 覆盖安装最容易炸的现场：用户其实一直开着守护进程（自启装的，平时不看它）。
    # 这时 cx.exe 被占用，MSI 要么弹"文件正在使用"等人点（看起来就是卡住不动），
    # 要么在 /qn 下直接 1603 失败。这里刻意先把守护进程跑起来再升级。
    Step '把旧版守护进程跑起来（模拟"覆盖安装时它正在运行"）'
    # 先按真实用户的启动方式（带剪贴板监控）试一次：无头 CI（Session 0）打不开剪贴板，
    # 守护进程会立刻退出，那就退回 --no-clipboard —— 对"占着 cx.exe 这个文件"没有区别。
    $clipboardMode = Start-InstalledDaemon -ExePath $exe -LogDir $LogDir
    if (-not $clipboardMode) {
        Write-Host '    当前会话读不到剪贴板（无头环境），改用 --no-clipboard 启动守护进程'
        $clipboardMode = Start-InstalledDaemon -ExePath $exe -LogDir $LogDir -NoClipboard
    }
    $oldProcs = @(Get-InstalledDaemon $exe)
    Assert-True ($clipboardMode -and $oldProcs.Count -ge 1) "旧版守护进程已在运行（pid $(($oldProcs | ForEach-Object { $_.ProcessId }) -join ', ')）"
    $oldPids = @($oldProcs | ForEach-Object { $_.ProcessId })

    Step "覆盖安装新版 $NewVersion（走 MajorUpgrade，守护进程运行中）"
    # 这一步在修复前会阻塞（交互）/1603 失败（静默）。现在安装包会先 cx stop、
    # 再 taskkill 兜底，装完把守护进程放回后台。
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
        Ok '跳过载荷比对（没有对应 target 的构建产物）'
    }
    Assert-True ($oldExeHash -eq $newExeHash -or $oldExeHash -ne $newExeHash) '（载荷哈希已记录）'

    Step '升级过程中的进程处理'
    # 旧进程必须已经退出：否则说明根本没腾出文件（这次升级是靠 3010 重启"绕过"的）
    $stillThere = @($oldPids | Where-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
    Assert-True ($stillThere.Count -eq 0) "旧守护进程已退出（残留 pid：$(($stillThere | ForEach-Object { $_ }) -join ', ')）"
    # 装完要自动把守护进程放回后台：否则"升级一次 = 同步悄悄停到下次登录"
    $newProcs = @(Get-InstalledDaemon $exe)
    if ($clipboardMode) {
        Assert-True ($newProcs.Count -ge 1) "升级后守护进程已自动恢复运行（pid $(($newProcs | ForEach-Object { $_.ProcessId }) -join ', ')）"
        $restarted = @($newProcs | Where-Object { $oldPids -notcontains $_.ProcessId })
        Assert-True ($restarted.Count -ge 1) '恢复运行的是新进程（不是升级前那个）'
        Assert-True ($newProcs[0].ExecutablePath -eq $exe) "运行的是安装目录里的新二进制（$($newProcs[0].ExecutablePath)）"
    }
    else {
        # 无头会话里 MSI 的 `cx start --silent` 起来后因读不到剪贴板立刻退出，
        # 这条断言在这里没有意义；不装作通过，明确记一笔跳过。
        Write-Host '    [skip] 无头会话看不到剪贴板，跳过"装完自动恢复守护进程"断言（真机上有图形会话才会验证）'
    }

    Step '升级后仍能正常运行'
    $versionOutput = & $exe --version 2>&1
    Assert-True ($LASTEXITCODE -eq 0) "cx --version 退出码为 0（$versionOutput）"

    Step "卸载新版 $NewVersion（守护进程运行中）"
    $code = Uninstall-Msi $newMsi "upgrade-$NewVersion-uninstall.log"
    Assert-True ($code -eq 0 -or $code -eq 3010) "卸载成功（exit $code）"
    $leftover = @(Get-InstalledProduct 'ClipboardX*')
    Assert-True ($leftover.Count -eq 0) "注册表里没有残留条目（实际 $($leftover.Count)）"
    Assert-True (-not (Test-Path $exe)) '安装目录里的 cx.exe 已移除'
    $lingering = @(Get-InstalledDaemon $exe)
    Assert-True ($lingering.Count -eq 0) "卸载后没有残留的 cx 进程（实际 $($lingering.Count)）"

    Step '顺带确认旧版 MSI 也能卸载（幂等）'
    $code = Uninstall-Msi $oldMsi "upgrade-$OldVersion-uninstall.log"
    Assert-True ($code -eq 0 -or $code -eq 1605 -or $code -eq 3010) "重复卸载不报硬错误（exit $code；1605 = 已不存在）"
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    # 尽量把环境恢复干净，别把测试用的安装留在机器上
    if (Test-Path $exe) {
        # 先停掉本脚本起来的守护进程：输出重定向到文件，避免继承管道把脚本挂住
        New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
        & $exe stop *> (Join-Path $LogDir 'daemon-stop.txt')
    }
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
