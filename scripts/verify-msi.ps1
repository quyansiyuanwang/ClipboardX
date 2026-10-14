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
    [string]$LogDir = 'dist\msi-logs'
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
