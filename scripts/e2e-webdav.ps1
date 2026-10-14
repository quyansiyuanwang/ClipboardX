# 端到端验收（WebDAV）：两台守护进程经真实 WebDAV 服务端互相同步
#
# 用法：
#   pwsh scripts/e2e-webdav.ps1              # 需要 python + pip（会自动装 wsgidav 到临时 venv）
#   pwsh scripts/e2e-webdav.ps1 -KeepTemp
#
# 与 scripts/e2e-local.ps1 的区别：那个只验本机守护进程与 CLI；这个把"跨设备同步"
# 整条链路跑通：A 的剪贴板 → A 的引擎 → WebDAV → B 的 worker → B 的引擎 → B 的历史，
# 以及反向（B 发布 → A 应用到自己剪贴板）。
#
# 过程中会改写系统剪贴板，结束时恢复。
[CmdletBinding()]
param(
    [switch]$KeepTemp,
    [int]$WaitSeconds = 6
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-dav-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$davRoot = Join-Path $root 'dav'
$venv = Join-Path $root 'venv'
$port = 18080 + (Get-Random -Maximum 200)
$baseUrl = "http://127.0.0.1:$port/"

$failures = [System.Collections.Generic.List[string]]::new()
$originalClipboard = $null
$davProcess = $null

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) { Write-Host "    [fail] $message" -ForegroundColor Red; $failures.Add($message) }
function Assert-True([bool]$condition, [string]$message) { if ($condition) { Ok $message } else { Fail $message } }

function New-DeviceEnv([string]$name, [string]$baseUrl, [switch]$NoClipboard) {
    $deviceRoot = Join-Path $root $name
    $env:CX_CONFIG_DIR = Join-Path $deviceRoot 'config'
    $env:CX_DATA_DIR = Join-Path $deviceRoot 'data'
    $env:CX_CACHE_DIR = Join-Path $deviceRoot 'cache'
    New-Item -ItemType Directory -Force -Path $env:CX_CONFIG_DIR, $env:CX_DATA_DIR, $env:CX_CACHE_DIR | Out-Null
    # 缩短轮询间隔，让验收不必等太久
    @"
[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 300
debounce_ms = 100

[[backend]]
name = "dav"
kind = "webdav"
url = "$baseUrl"
"@ | Set-Content (Join-Path $env:CX_CONFIG_DIR 'config.toml')
    return @{
        Config = $env:CX_CONFIG_DIR
        Data   = $env:CX_DATA_DIR
        Cache  = $env:CX_CACHE_DIR
    }
}

function Invoke-Device([hashtable]$device, [string[]]$Arguments) {
    $env:CX_CONFIG_DIR = $device.Config
    $env:CX_DATA_DIR = $device.Data
    $env:CX_CACHE_DIR = $device.Cache
    $output = & $script:cx @Arguments 2>&1
    return @{ Output = ($output -join "`n"); Code = $LASTEXITCODE }
}

Write-Host "临时目录：$root" -ForegroundColor DarkGray
try {
    Step '构建 release 二进制'
    Push-Location $repo
    & cargo build --release --bin cx 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    $script:cx = if ($IsWindows -or $PSVersionTable.PSEdition -eq 'Desktop') {
        Join-Path $repo 'target\release\cx.exe'
    } else { Join-Path $repo 'target/release/cx' }
    Assert-True (Test-Path $script:cx) '二进制就绪'

    $originalClipboard = try { Get-Clipboard -Raw } catch { $null }

    Step '准备真实 WebDAV 服务端（wsgidav）'
    New-Item -ItemType Directory -Force -Path $davRoot | Out-Null
    & python -m venv $venv 2>&1 | Select-Object -Last 1
    $pip = Join-Path $venv 'Scripts\pip.exe'
    if (-not (Test-Path $pip)) { $pip = Join-Path $venv 'bin/pip' }
    & $pip install --quiet wsgidav cheroot 2>&1 | Select-Object -Last 2
    $wsgidav = Join-Path $venv 'Scripts\wsgidav.exe'
    if (-not (Test-Path $wsgidav)) { $wsgidav = Join-Path $venv 'bin/wsgidav' }

    $davArgs = @("--host=127.0.0.1", "--port=$port", "--root=$davRoot", "--auth=anonymous")
    $startParams = @{ FilePath = $wsgidav; ArgumentList = $davArgs; PassThru = $true }
    if ($IsWindows -or $PSVersionTable.PSEdition -eq 'Desktop') { $startParams['WindowStyle'] = 'Hidden' }
    $davProcess = Start-Process @startParams
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        try {
            $response = Invoke-WebRequest -Uri $baseUrl -Method Options -TimeoutSec 2 -UseBasicParsing
            if ($response.StatusCode -ge 200) { $ready = $true; break }
        }
        catch { }
    }
    Assert-True $ready "WebDAV 已就绪：$baseUrl"
    if (-not $ready) { throw 'WebDAV 服务端未就绪' }

    Step '准备两台设备的隔离目录'
    $deviceA = New-DeviceEnv 'device-a' $baseUrl
    $deviceB = New-DeviceEnv 'device-b' $baseUrl -NoClipboard
    Assert-True (Test-Path (Join-Path $deviceA.Config 'config.toml')) 'A 的配置已写入'
    Assert-True (Test-Path (Join-Path $deviceB.Config 'config.toml')) 'B 的配置已写入'

    Step '探测本机诊断（守护进程未运行 → 本地检查）'
    $result = Invoke-Device $deviceA @('doctor', '--json')
    $lines = $result.Output -split "`n"
    $jsonStart = ($lines | Select-String -Pattern '^\s*\{' | Select-Object -First 1).LineNumber
    $report = ($lines[($jsonStart - 1)..($lines.Count - 1)] -join "`n") | ConvertFrom-Json
    $checkNames = ($report.checks | ForEach-Object { $_.name }) -join ','
    Assert-True ($checkNames -match '配置') "本地诊断包含配置检查（$checkNames）"

    Step '启动两台守护进程'
    $procA = Start-Process -FilePath $script:cx -ArgumentList @('daemon', '--foreground', '--quiet') `
        -PassThru -WindowStyle Hidden -Environment @{ CX_CONFIG_DIR = $deviceA.Config; CX_DATA_DIR = $deviceA.Data; CX_CACHE_DIR = $deviceA.Cache; CX_LOG = 'debug' }
    $procB = Start-Process -FilePath $script:cx -ArgumentList @('daemon', '--foreground', '--quiet', '--no-clipboard') `
        -PassThru -WindowStyle Hidden -Environment @{ CX_CONFIG_DIR = $deviceB.Config; CX_DATA_DIR = $deviceB.Data; CX_CACHE_DIR = $deviceB.Cache; CX_LOG = 'debug' }
    $bothUp = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        if ((Test-Path (Join-Path $deviceA.Data 'daemon.json')) -and (Test-Path (Join-Path $deviceB.Data 'daemon.json'))) {
            $bothUp = $true; break
        }
    }
    Assert-True $bothUp '两台守护进程都已就绪'

    # ---------- A → B ----------
    $textA = "来自 A 的剪贴板内容 🎉 " + (Get-Random)
    Step "A 发布内容（写剪贴板，等守护进程采集）：$textA"
    Set-Clipboard -Value $textA
    Start-Sleep -Seconds $WaitSeconds

    Step 'B 应通过 WebDAV 收到该内容'
    $received = $false
    for ($i = 0; $i -lt 20; $i++) {
        $history = Invoke-Device $deviceB @('history', '--json')
        if ($history.Code -eq 0) {
            $items = $history.Output | ConvertFrom-Json
            if ($items.Count -ge 1) {
                $preview = Invoke-Device $deviceB @('show', '0', '--json')
                $shown = $preview.Output | ConvertFrom-Json
                if ($shown.text -eq $textA) { $received = $true; break }
            }
        }
        Start-Sleep -Seconds 2
    }
    Assert-True $received "B 的历史里出现了 A 的内容（实际等待 $($i * 2) 秒）"

    # ---------- B → A ----------
    $textB = "来自 B 的回复 🙂 " + (Get-Random)
    Step "B 反向发布：$textB"
    $push = Invoke-Device $deviceB @('push', '--text', $textB)
    Assert-True ($push.Code -eq 0) "B 的 push 成功（$($push.Output)）"

    Step 'A 应把该内容写入自己的剪贴板'
    $applied = $false
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Seconds 2
        $current = Get-Clipboard -Raw
        if ($current -eq $textB) { $applied = $true; break }
    }
    Assert-True $applied "A 的剪贴板已变成本地 B 的内容（实际等待 $($i * 2) 秒）"

    Step '两边都不产生重复上传（防回环）'
    Start-Sleep -Seconds $WaitSeconds
    $historyA = (Invoke-Device $deviceA @('history', '--json')).Output | ConvertFrom-Json
    $historyB = (Invoke-Device $deviceB @('history', '--json')).Output | ConvertFrom-Json
    Assert-True ($historyA.Count -le 3) "A 的历史没有膨胀（$($historyA.Count) 条）"
    Assert-True ($historyB.Count -le 3) "B 的历史没有膨胀（$($historyB.Count) 条）"
    Write-Host "    A 历史 $($historyA.Count) 条；B 历史 $($historyB.Count) 条"

    Step '守护进程运行中的诊断应报告后端状态'
    $result = Invoke-Device $deviceB @('doctor', '--json')
    $lines = $result.Output -split "`n"
    $jsonStart = ($lines | Select-String -Pattern '^\s*\{' | Select-Object -First 1).LineNumber
    $daemonReport = ($lines[($jsonStart - 1)..($lines.Count - 1)] -join "`n") | ConvertFrom-Json
    $backendCheck = $daemonReport.checks | Where-Object { $_.name -eq '后端' } | Select-Object -First 1
    Assert-True ($null -ne $backendCheck -and $backendCheck.ok) "诊断报告后端已启用：$($backendCheck.detail)"

    Step '停止两台守护进程'
    Assert-True ((Invoke-Device $deviceA @('stop')).Code -eq 0) 'A 已停止'
    Assert-True ((Invoke-Device $deviceB @('stop')).Code -eq 0) 'B 已停止'
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    if ($null -ne $originalClipboard) {
        try { Set-Clipboard -Value $originalClipboard; Write-Host '已恢复原剪贴板内容' -ForegroundColor DarkGray } catch { }
    }
    foreach ($proc in @($procA, $procB, $davProcess)) {
        if ($null -ne $proc) {
            $alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
            if ($null -ne $alive) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
    foreach ($device in @($deviceA, $deviceB)) {
        if ($null -ne $device) {
            $log = Join-Path $device.Cache 'logs/daemon.log'
            if (Test-Path $log) {
                Write-Host "`n--- $($device.Config) 日志（末尾 12 行）---" -ForegroundColor DarkGray
                Get-Content $log -Tail 12 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
            }
        }
    }
    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'WebDAV 双设备端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "WebDAV 端到端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
