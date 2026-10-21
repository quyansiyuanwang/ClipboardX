# 校验 Windows MSI 安装包：真实安装 → 断言 → 卸载 → 断言无残留
#
# 用法：
#   pwsh scripts/verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi
#   pwsh scripts/verify-msi.ps1 -Msi dist\ClipboardX-0.1.0-x64.msi -Mode Extract   # 仅解包，不改动系统
#
# 注意：Install 模式会写入当前用户的 %LOCALAPPDATA%、PATH 与 HKCU Run 键，并在卸载时还原。
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Msi,
    [ValidateSet('Install', 'Extract')][string]$Mode = 'Install',
    [string]$LogDir = 'dist\msi-logs',
    # 无 UI 构建（build-msi.ps1 -NoUi）时跳过「安装向导存在性」检查
    [switch]$NoUiBuild
)

$ErrorActionPreference = 'Stop'

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "断言失败：$Message" }
    Write-Host "    [ok] $Message"
}

$Msi = (Resolve-Path $Msi).Path
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\ClipboardX'
$exe = Join-Path $installDir 'cx.exe'
$serverExe = Join-Path $installDir 'cx-server.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

if ($Mode -eq 'Extract') {
    $extractDir = Join-Path ([IO.Path]::GetTempPath()) ("cx-msi-extract-" + [guid]::NewGuid().ToString('N'))
    Write-Host "==> 管理员解包到 $extractDir"
    $p = Start-Process msiexec.exe -ArgumentList @('/a', "`"$Msi`"", '/qn', "TARGETDIR=`"$extractDir`"", '/l*v', "`"$LogDir\extract.log`"") -Wait -PassThru
    Assert-True ($p.ExitCode -eq 0) "msiexec /a 退出码为 0（实际 $($p.ExitCode)）"
    $found = Get-ChildItem -Path $extractDir -Recurse -Filter 'cx.exe' -ErrorAction SilentlyContinue
    Assert-True ($null -ne $found) ' 解包产物中包含 cx.exe'
    Write-Host "==> Extract 模式完成（未改动系统）"
    exit 0
}

# ---- 前置：确保没有旧版本干扰 ----
if (Test-Path $exe) {
    Write-Host '==> 检测到已安装版本，先卸载'
    Start-Process msiexec.exe -ArgumentList @('/x', "`"$Msi`"", '/qn') -Wait | Out-Null
}

# ---- 静态检查：UI 序列里不能有"从目录启动可执行"的自定义动作 ----
# 这类动作（Type 含 34，例如 cx stop / taskkill）依赖 Directory Manager；
# 如果排在 CostInitialize(800) 之前，双击安装会失败并报「错误代码 2732」，
# 而 /qn 与 /qb 都不跑 UI 序列（日志里是 "Running entire install on the server"），
# 所以静默安装永远发现不了。必须静态查表。
Write-Host '==> 静态检查：UI 序列里没有依赖目录的自定义动作'
try {
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @($Msi, 0))
    $customActions = @{}
    $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @('SELECT Action,Type FROM CustomAction'))
    $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
    while ($true) {
        $rec = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
        if (-not $rec) { break }
        $name = [string]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @(1))
        $type = [int]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @(2))
        $customActions[$name] = $type
    }
    # 1 = 从已安装文件启动、17 = 从目录启动、18/34 等；34 = 目录 + 命令行（我们用的这种）
    $offenders = @()
    $uiRows = @()
    try {
        $uiView = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @('SELECT Action,Sequence FROM InstallUISequence'))
        $uiView.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $uiView, $null)
        while ($true) {
            $rec = $uiView.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $uiView, $null)
            if (-not $rec) { break }
            $action = [string]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @(1))
            $sequence = [int][string]$rec.GetType().InvokeMember('StringData', 'GetProperty', $null, $rec, @(2))
            $uiRows += , @($action, $sequence)
        }
    }
    catch {
        # 没有 UI 序列（-NoUi 构建）：本身就不会有 2732
    }
    foreach ($row in $uiRows) {
        $action = $row[0]
        $sequence = $row[1]
        if ($action -eq 'CostInitialize') { $costInit = $sequence }
        if ($customActions.ContainsKey($action)) {
            $type = $customActions[$action]
            $base = $type % 64
            if ($base -eq 34 -and ($null -eq $costInit -or $sequence -lt $costInit)) {
                $offenders += "$action (Type=$type, Sequence=$sequence)"
            }
        }
    }
    Assert-True ($offenders.Count -eq 0) "UI 序列里没有排在 CostInitialize 之前的目录型自定义动作（问题项：$($offenders -join ', ')）"
}
catch {
    Write-Host "    [skip] 无法读取 MSI 表做静态检查：$($_.Exception.Message)" -ForegroundColor Yellow
}

# ---- 静态检查：带 UI 构建时，对话框表不能是空的 ----
# 出现过这种情况：CI 打出来的 MSI 只有 1 个对话框（没有 Welcome / License / Custom Setup），
# 而本机同样的脚本打出来有 22 个。安装照样能过，但用户双击时看不到任何向导，
# 也没法勾选 PATH / 自启 / 桌面快捷方式。这里让它显形（先只告警，不拦）。
if (-not $NoUiBuild) {
    $dialogs = 0
    try {
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $db = $installer.GetType().InvokeMember('OpenDatabase', 'InvokeMethod', $null, $installer, @($Msi, 0))
        $view = $db.GetType().InvokeMember('OpenView', 'InvokeMethod', $null, $db, @('SELECT Dialog FROM Dialog'))
        $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null)
        while ($view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)) { $dialogs++ }
    }
    catch {
        # 连 Dialog 表都没有 = 完全没编 UI（比"对话框少"更严重），按 0 处理
        $dialogs = 0
    }
    if ($dialogs -lt 5) {
        Write-Host "    [warn] 这份 MSI 只有 $dialogs 个对话框：没有安装向导（UI 扩展没生效或用了 -NoUi）" -ForegroundColor Yellow
    }
    else {
        Assert-True $true "MSI 含安装向导（$dialogs 个对话框）"
    }
}

Write-Host '==> 静默安装（per-user，无需提权）'
$p = Start-Process msiexec.exe -ArgumentList @('/i', "`"$Msi`"", '/qn', 'ADDLOCAL=Main,PathEnv,Autostart', '/l*v', "`"$LogDir\install.log`"") -Wait -PassThru
Assert-True ($p.ExitCode -eq 0) "msiexec /i 退出码为 0（实际 $($p.ExitCode)）"

try {
    Assert-True (Test-Path $exe) "已安装到 $exe"

    # PATH（用户级环境变量）
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    Assert-True ($userPath -like "*$installDir*") '用户 PATH 已包含安装目录'

    # 自启
    $run = (Get-ItemProperty -Path $runKey -Name 'ClipboardX' -ErrorAction SilentlyContinue).ClipboardX
    Assert-True ($null -ne $run) 'HKCU Run 已注册 ClipboardX 自启项'
    Assert-True ($run -like "*cx.exe*start --silent*") "自启命令形如 cx.exe start --silent（实际：$run）"

    # 二进制可运行
    $json = & $exe version --json
    Assert-True ($LASTEXITCODE -eq 0) 'cx version 退出码为 0'
    Assert-True (Test-Path $serverExe) '自建服务端 cx-server.exe 也已安装'
    $serverVersion = & $serverExe version
    Assert-True ($LASTEXITCODE -eq 0) "cx-server version 退出码为 0（$serverVersion）"
    Assert-True (($serverVersion -join ' ') -match '^cx-server ') 'cx-server version 输出形如 cx-server X.Y.Z'
    $info = ($json -join [Environment]::NewLine) | ConvertFrom-Json
    Assert-True ($info.name -eq 'cx') 'cx version --json 输出 name=cx'
    Assert-True ($info.store_layout -ge 1) 'cx version --json 报告了 store_layout'
    Write-Host "    版本：$($info.version) / $($info.os) $($info.arch)"

    # 版本一致性（安装包版本 vs 二进制版本）
    $fileVersion = (Get-Item $exe).VersionInfo.FileVersion
    Write-Host "    二进制 FileVersion：$fileVersion"
}
finally {
    Write-Host '==> 静默卸载'
    $p = Start-Process msiexec.exe -ArgumentList @('/x', "`"$Msi`"", '/qn', '/l*v', "`"$LogDir\uninstall.log`"") -Wait -PassThru
    Assert-True ($p.ExitCode -eq 0) "msiexec /x 退出码为 0（实际 $($p.ExitCode)）"

    Assert-True (-not (Test-Path $exe)) '安装目录中的 cx.exe 已移除'
    $run = (Get-ItemProperty -Path $runKey -Name 'ClipboardX' -ErrorAction SilentlyContinue).ClipboardX
    Assert-True ($null -eq $run) 'HKCU Run 自启项已移除'
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    Assert-True ($userPath -notlike "*$installDir*") '用户 PATH 已还原'
}

Write-Host '==> MSI 校验全部通过'
