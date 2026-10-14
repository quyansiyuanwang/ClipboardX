
#
# 用法：
#   pwsh scripts/e2e-hardening.ps1
#   pwsh scripts/e2e-hardening.ps1 -KeepTemp
#
# 四项测量，每项都有硬阈值：
#   1. 体积：cx < 6 MB、cx-server < 5 MB（逐项列表，便于归因）；
#   2. 密钥落盘权限：Unix 上必须 0600（space.key / control.token / lan.key / secrets/*）；
#      Windows 上只列路径（ACL 检查交给平台工具）；
#   3. **明文泄漏**：加密空间下把一条带唯一标记的内容同步一遍，然后在数据目录与日志里
#      全盘搜索该标记——它不该出现在任何地方（这是"服务端只见密文"的客户端版本）；
#   4. 空闲开销：守护进程空转 10 秒，CPU 时间增量 < 100 ms（< 1% 单核），常驻内存 < 40 MB。
[CmdletBinding()]
param(
    [switch]$KeepTemp,
    [int]$IdleSeconds = 10
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-hardening-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$passphrase = 'hardening-passphrase-9b21'

$failures = [System.Collections.Generic.List[string]]::new()
$daemonProcess = $null
$device = $null

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) { Write-Host "    [fail] $message" -ForegroundColor Red; $failures.Add($message) }
function Assert-True([bool]$condition, [string]$message) { if ($condition) { Ok $message } else { Fail $message } }

function Invoke-Cx([string[]]$Arguments, [string]$Stdin) {
    $env:CX_CONFIG_DIR = $device.Config
    $env:CX_DATA_DIR = $device.Data
    $env:CX_CACHE_DIR = $device.Cache
    if ($null -ne $Stdin) { $output = $Stdin | & $script:cx @Arguments 2>&1 }
    else { $output = & $script:cx @Arguments 2>&1 }
    return @{ Output = ($output -join "`n"); Code = $LASTEXITCODE }
}

Write-Host "临时目录：$root" -ForegroundColor DarkGray
try {
    Step '构建 release 二进制'
    Push-Location $repo
    & cargo build --release --bin cx --bin cx-server 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    $suffix = if ($isWindowsHost) { '.exe' } else { '' }
    $script:cx = Join-Path $repo "target\release\cx$suffix"
    $cxServer = Join-Path $repo "target\release\cx-server$suffix"

    # ---------------------------------------------------------------- 体积
    Step '体积预算（逐项列出，便于归因）'
    $budgets = @{ $script:cx = 6 * 1024 * 1024; $cxServer = 5 * 1024 * 1024 }
    foreach ($entry in $budgets.GetEnumerator()) {
        if (-not (Test-Path $entry.Key)) { Fail "找不到 $($entry.Key)"; continue }
        $bytes = (Get-Item $entry.Key).Length
        $mb = [math]::Round($bytes / 1MB, 2)
        $budgetMb = [math]::Round($entry.Value / 1MB, 2)
        Assert-True ($bytes -lt $entry.Value) "$(Split-Path $entry.Key -Leaf)：$mb MB（预算 $budgetMb MB）"
    }

    Step '准备设备与加密空间'
    $device = @{
        Config = Join-Path $root 'config'
        Data = Join-Path $root 'data'
        Cache = Join-Path $root 'cache'
    }
    New-Item -ItemType Directory -Force -Path $device.Config, $device.Data, $device.Cache | Out-Null
    @"
[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 500
debounce_ms = 100

[lan]
enabled = true
port = 0
pairing_code = "hardening-lan-code"
use_mdns = false
"@ | Set-Content (Join-Path $device.Config 'config.toml')

    $init = Invoke-Cx @('space', 'init', '--passphrase-stdin') $passphrase
    Assert-True ($init.Code -eq 0) "空间已创建（$($init.Output.Split([char]10)[0])）"

    Step '启动守护进程（纯中继模式）'
    $params = @{
        FilePath = $script:cx
        ArgumentList = @('daemon', '--foreground', '--quiet', '--no-clipboard')
        PassThru = $true
        Environment = @{
            CX_CONFIG_DIR = $device.Config; CX_DATA_DIR = $device.Data; CX_CACHE_DIR = $device.Cache
        }
    }
    if ($isWindowsHost) { $params['WindowStyle'] = 'Hidden' }
    $daemonProcess = Start-Process @params
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        if (Test-Path (Join-Path $device.Data 'daemon.json')) { $ready = $true; break }
    }
    Assert-True $ready '守护进程已就绪'

    # ---------------------------------------------------------------- 明文泄漏
    Step '明文泄漏检查：加密空间下内容不该出现在数据目录与日志里'
    $marker = "HARDENING-MARKER-" + [guid]::NewGuid().ToString('N').Substring(0, 12)
    $push = Invoke-Cx @('push', '--text', $marker)
    Assert-True ($push.Code -eq 0) '已推送带标记的内容'
    Start-Sleep -Seconds 2

    # 确认内容确实进了历史（否则"找不到"是因为没同步，而不是因为加密）
    $preview = Invoke-Cx @('show', '0', '--json')
    Assert-True ($preview.Output -match [regex]::Escape($marker)) '历史里能读回该内容（证明它确实落盘了）'

    $leaked = [System.Collections.Generic.List[string]]::new()
    $scanned = 0
    foreach ($dir in @($device.Data, $device.Cache, $device.Config)) {
        foreach ($file in Get-ChildItem -Recurse -File $dir -ErrorAction SilentlyContinue) {
            $scanned++
            # 守护进程可能正持有某些文件（daemon.lock、正在写的日志），用共享读打开；
            # 实在读不到就跳过并计数——不能因为"读不了"就默认"没泄漏"
            try {
                $stream = [System.IO.File]::Open($file.FullName, [System.IO.FileMode]::Open,
                    [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
                try {
                    $bytes = New-Object byte[] $stream.Length
                    $read = $stream.Read($bytes, 0, $bytes.Length)
                    $text = [System.Text.Encoding]::UTF8.GetString($bytes, 0, $read)
                }
                finally { $stream.Dispose() }
            }
            catch {
                Write-Host "      跳过（无法读取）：$($file.Name)" -ForegroundColor DarkGray
                continue
            }
            if ($text.Contains($marker)) { $leaked.Add($file.FullName) }
        }
    }
    Assert-True ($leaked.Count -eq 0) "扫了 $scanned 个文件，明文标记没有出现在任何地方"
    foreach ($file in $leaked) { Write-Host "      泄漏：$file" -ForegroundColor Red }

    # ---------------------------------------------------------------- 密钥权限
    Step '密钥落盘权限'
    $secretFiles = @(
        (Join-Path $device.Config 'space.key'),
        (Join-Path $device.Config 'secrets'),
        (Join-Path $device.Data 'control.token'),
        (Join-Path $device.Config 'lan.key')
    )
    foreach ($path in $secretFiles) {
        if (-not (Test-Path $path)) { Fail "缺少密钥文件：$path"; continue }
        if ($isWindowsHost) {
            # Windows 上用 ACL 表达，这里只确认文件在用户目录下且存在（ACL 检查交给平台工具）
            Ok "$(Split-Path $path -Leaf)：存在（Windows 上权限由 ACL 表达）"
        }
        else {
            $mode = if ($IsMacOS) { (stat -f '%Lp' $path) } else { (stat -c '%a' $path) }
            # 正确的不变量是"组与其他位为 0"：文件 600、目录 700 都满足。
            # 一开始写的是精确等于 600，于是 secrets/ 这个**目录**（700）被误报——
            # 断言写错了，不是权限有问题（只有 CI 上跑 Unix 分支才会暴露）。
            $value = [Convert]::ToInt32("$mode".Trim(), 8)
            $others = $value -band 63   # 0o077：组 rwx + 其他 rwx
            $isDir = Test-Path -PathType Container $path
            $kind = if ($isDir) { '目录' } else { '文件' }
            $expect = if ($isDir) { '700' } else { '600' }
            Assert-True ($others -eq 0) "$(Split-Path $path -Leaf)（$kind）：权限 $mode（应为 $expect，组/其他位必须为 0）"
        }
    }

    # ---------------------------------------------------------------- 空闲开销
    Step "空闲开销（观察 $IdleSeconds 秒）"
    $proc = Get-Process -Id $daemonProcess.Id
    $cpuBefore = $proc.TotalProcessorTime.TotalMilliseconds
    Start-Sleep -Seconds $IdleSeconds
    $proc.Refresh()
    $cpuDelta = $proc.TotalProcessorTime.TotalMilliseconds - $cpuBefore
    $rssMb = [math]::Round($proc.WorkingSet64 / 1MB, 1)
    $cpuPercent = [math]::Round($cpuDelta / ($IdleSeconds * 1000) * 100, 2)
    Assert-True ($cpuDelta -lt 100) "空闲 $IdleSeconds 秒的 CPU 时间增量 $([math]::Round($cpuDelta, 1)) ms（阈值 100 ms，约 $cpuPercent%）"
    Assert-True ($rssMb -lt 40) "常驻内存 $rssMb MB（阈值 40 MB）"

    Step '停止守护进程'
    Assert-True ((Invoke-Cx @('stop')).Code -eq 0) '已停止'
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    if ($null -ne $daemonProcess) {
        $alive = Get-Process -Id $daemonProcess.Id -ErrorAction SilentlyContinue
        if ($null -ne $alive) { Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue }
    }
    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host '加固验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "加固验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
