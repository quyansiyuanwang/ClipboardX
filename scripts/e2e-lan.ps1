# 端到端验收（局域网直连）：两台守护进程不经任何服务端互相同步
#
# 用法：
#   pwsh scripts/e2e-lan.ps1                # 自动探测剪贴板
#   pwsh scripts/e2e-lan.ps1 -NoClipboard   # 强制无剪贴板路径（无头 CI 用）
#   pwsh scripts/e2e-lan.ps1 -KeepTemp
#
# 与其它 e2e 脚本的区别：**完全不依赖任何后端**。两台守护进程只用局域网直连
# （mDNS 关掉、显式给对端地址——CI 里多播常被禁，显式地址也更确定），
# 验证的是"同一个网段里不经服务器就能同步"这条路径。
#
# 顺带验一个安全属性：配对码不同的第三台设备**连不上也收不到**任何内容。
[CmdletBinding()]
param(
    [switch]$NoClipboard,
    [switch]$KeepTemp,
    [int]$WaitSeconds = 8
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-lan-e2e-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$pairingCode = 'lan-e2e-code-7c31'
$wrongCode = 'lan-e2e-code-WRONG'

$failures = [System.Collections.Generic.List[string]]::new()
$originalClipboard = $null
$procs = @{}
$devices = @{}

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) { Write-Host "    [fail] $message" -ForegroundColor Red; $failures.Add($message) }
function Assert-True([bool]$condition, [string]$message) { if ($condition) { Ok $message } else { Fail $message } }

function Start-Hidden([string]$file, [string[]]$arguments, [hashtable]$environment) {
    $params = @{ FilePath = $file; ArgumentList = $arguments; PassThru = $true }
    if ($null -ne $environment) { $params['Environment'] = $environment }
    if ($isWindowsHost) { $params['WindowStyle'] = 'Hidden' }
    return Start-Process @params
}

function New-Device([string]$name, [int]$port, [string[]]$peers, [string]$code) {
    $deviceRoot = Join-Path $root $name
    $config = Join-Path $deviceRoot 'config'
    $data = Join-Path $deviceRoot 'data'
    $cache = Join-Path $deviceRoot 'cache'
    New-Item -ItemType Directory -Force -Path $config, $data, $cache | Out-Null
    $peerList = ($peers | ForEach-Object { "`"$_`"" }) -join ', '
    @"
[space]
encrypt = false

[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 300
debounce_ms = 100

[lan]
enabled = true
port = $port
pairing_code = "$code"
peers = [$peerList]
use_mdns = false
"@ | Set-Content (Join-Path $config 'config.toml')
    return @{ Config = $config; Data = $data; Cache = $cache; Port = $port }
}

function Invoke-Device([hashtable]$device, [string[]]$Arguments) {
    $env:CX_CONFIG_DIR = $device.Config
    $env:CX_DATA_DIR = $device.Data
    $env:CX_CACHE_DIR = $device.Cache
    $output = & $script:cx @Arguments 2>&1
    return @{ Output = ($output -join "`n"); Code = $LASTEXITCODE }
}

function Convert-CxJson([string]$text) {
    $lines = $text -split "`n"
    $start = ($lines | Select-String -Pattern '^\s*[\[\{]' | Select-Object -First 1).LineNumber
    if ($null -eq $start) { return $null }
    return ($lines[($start - 1)..($lines.Count - 1)] -join "`n") | ConvertFrom-Json
}

function Wait-Item([hashtable]$device, [string]$expected, [int]$seconds) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        $history = Invoke-Device $device @('history', '--json')
        if ($history.Code -eq 0) {
            $items = Convert-CxJson $history.Output
            if ($items.Count -ge 1) {
                $preview = Convert-CxJson (Invoke-Device $device @('show', '0', '--json')).Output
                if ($preview.text -eq $expected) { return $true }
            }
        }
        Start-Sleep -Seconds 1
    }
    return $false
}

Write-Host "临时目录：$root" -ForegroundColor DarkGray
try {
    Step '构建 release 二进制'
    Push-Location $repo
    & cargo build --release --bin cx 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    $suffix = if ($isWindowsHost) { '.exe' } else { '' }
    $script:cx = Join-Path $repo "target\release\cx$suffix"
    Assert-True (Test-Path $script:cx) '二进制就绪'

    Step '准备三台设备的隔离目录'
    # 选两个空闲端口
    $listenerA = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listenerA.Start(); $portA = $listenerA.LocalEndpoint.Port; $listenerA.Stop()
    $listenerB = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listenerB.Start(); $portB = $listenerB.LocalEndpoint.Port; $listenerB.Stop()
    $listenerC = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listenerC.Start(); $portC = $listenerC.LocalEndpoint.Port; $listenerC.Stop()

    # A 与 B 用同一个配对码；C 用错误的配对码（用来验证"连不上也收不到"）
    $devices['A'] = New-Device 'device-a' $portA @() $pairingCode
    $devices['B'] = New-Device 'device-b' $portB @("127.0.0.1:$portA") $pairingCode
    $devices['C'] = New-Device 'device-c' $portC @("127.0.0.1:$portA") $wrongCode
    Assert-True (Test-Path (Join-Path $devices['A'].Config 'config.toml')) '三台设备的配置已写入'

    Step '启动三台守护进程'
    foreach ($tag in @('A', 'B', 'C')) {
        $device = $devices[$tag]
        $procs[$tag] = Start-Hidden $script:cx @('daemon', '--foreground', '--quiet', '--no-clipboard') @{
            CX_CONFIG_DIR = $device.Config; CX_DATA_DIR = $device.Data; CX_CACHE_DIR = $device.Cache; CX_LOG = 'debug'
        }
    }
    $bothUp = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        $up = @($devices.Values | Where-Object { Test-Path (Join-Path $_.Data 'daemon.json') }).Count
        if ($up -eq 3) { $bothUp = $true; break }
    }
    Assert-True $bothUp '三台守护进程都已就绪'

    Step '等待局域网握手完成（A 应当看到 B 连上来，C 应当连不上）'
    # 局域网不是"后端"（不走对象存储），因此它出现在 cx doctor 的检查里而不是 status.backends
    $connected = $false
    for ($i = 0; $i -lt 30; $i++) {
        $doctorA = Convert-CxJson (Invoke-Device $devices['A'] @('doctor', '--json')).Output
        $lanCheck = $doctorA.checks | Where-Object { $_.name -eq '局域网' } | Select-Object -First 1
        if ($null -ne $lanCheck -and $lanCheck.ok) { $connected = $true; break }
        Start-Sleep -Seconds 1
    }
    Assert-True $connected "A 的 doctor 报告局域网已有对端（等待 $i 秒）"
    $lanDetail = ($doctorA.checks | Where-Object { $_.name -eq '局域网' } | Select-Object -First 1).detail
    Write-Host "    局域网：$lanDetail" -ForegroundColor DarkGray
    # 日志里应当有对端连接的记录（C 用错配对码，不该出现它的名字）
    $logA = Join-Path $devices['A'].Cache 'logs/daemon.log'
    $logText = if (Test-Path $logA) { Get-Content $logA -Raw } else { '' }
    Assert-True ($logText -match '局域网对端已连接') 'A 的日志记录了局域网对端连接'
    Assert-True (-not ($logText -match 'device-c')) 'A 的日志里没有配对码错误的设备 C'

    # ---------- A → B（不经任何服务端） ----------
    $textA = "局域网直连内容 " + (Get-Random)
    Step "A 发布：$textA"
    Assert-True ((Invoke-Device $devices['A'] @('push', '--text', $textA)).Code -eq 0) 'A 的 push 成功'
    Assert-True (Wait-Item $devices['B'] $textA $WaitSeconds) 'B 通过局域网收到了 A 的内容'

    # ---------- B → A ----------
    $textB = "局域网反向内容 " + (Get-Random)
    Step "B 发布：$textB"
    Assert-True ((Invoke-Device $devices['B'] @('push', '--text', $textB)).Code -eq 0) 'B 的 push 成功'
    Assert-True (Wait-Item $devices['A'] $textB $WaitSeconds) 'A 通过局域网收到了 B 的内容'

    Step '安全断言：配对码错误的设备收不到任何内容'
    Start-Sleep -Seconds 3
    $historyC = Convert-CxJson (Invoke-Device $devices['C'] @('history', '--json')).Output
    $countC = if ($null -eq $historyC) { 0 } else { $historyC.Count }
    Assert-True ($countC -eq 0) "设备 C 的历史为空（实际 $countC 条）"

    Step '停止三台守护进程'
    foreach ($tag in @('A', 'B', 'C')) {
        Assert-True ((Invoke-Device $devices[$tag] @('stop')).Code -eq 0) "$tag 已停止"
    }
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    if ($null -ne $originalClipboard) {
        try { Set-Clipboard -Value $originalClipboard } catch { }
    }
    foreach ($proc in $procs.Values) {
        if ($null -ne $proc) {
            $alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
            if ($null -ne $alive) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
    foreach ($device in $devices.Values) {
        $log = Join-Path $device.Cache 'logs/daemon.log'
        if (Test-Path $log) {
            Write-Host "`n--- $($device.Config) 日志（末尾 8 行）---" -ForegroundColor DarkGray
            Get-Content $log -Tail 8 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
        }
    }
    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host '局域网直连端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "局域网端到端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
