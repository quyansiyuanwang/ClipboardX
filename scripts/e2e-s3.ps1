# 端到端验收（S3）：两台守护进程经 S3 兼容存储互相同步
#
# 用法：
#   pwsh scripts/e2e-s3.ps1                # 自动用 python 装 moto 起一个真实 S3 兼容服务端
#   pwsh scripts/e2e-s3.ps1 -NoClipboard   # 强制无剪贴板路径（无头 CI 用）
#   pwsh scripts/e2e-s3.ps1 -KeepTemp
#
# 与 e2e-webdav.ps1 的分工：
# - 两者都验"跨设备同步"整条链路，只是传输层不同（WebDAV vs S3 + SigV4 签名）；
# - **签名正确性**由仓库内的测试服务端保证（它会从原始字节重算 SigV4 并比对）；
#   本脚本另外跑一遍**独立实现**（moto）来证明线格式互通。
#   注意：moto 不校验签名（错误密钥也会通过），所以它不能替代前者。
[CmdletBinding()]
param(
    [switch]$NoClipboard,
    [switch]$KeepTemp,
    [int]$WaitSeconds = 6
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-s3-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$venv = Join-Path $root 'venv'
$port = 19021 + (Get-Random -Maximum 200)
$endpoint = "http://127.0.0.1:$port"
$bucket = 'clipboardx'
$accessKey = 'clipboardx-access'
$secretKey = 'clipboardx-secret-0123456789'

$script:skipped = $false
$failures = [System.Collections.Generic.List[string]]::new()
$originalClipboard = $null
$s3Process = $null
$procA = $null
$procB = $null
$deviceA = $null
$deviceB = $null

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

function New-DeviceEnv([string]$name, [string]$endpoint, [string]$bucket, [string]$keyFile) {
    $deviceRoot = Join-Path $root $name
    $config = Join-Path $deviceRoot 'config'
    $data = Join-Path $deviceRoot 'data'
    $cache = Join-Path $deviceRoot 'cache'
    New-Item -ItemType Directory -Force -Path $config, $data, $cache, (Join-Path $config 'secrets') | Out-Null
    @"
[space]
encrypt = false

[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 300
debounce_ms = 100

[[backend]]
name = "oss"
kind = "s3"
endpoint = "$endpoint"
bucket = "$bucket"
region = "us-east-1"
prefix = "space-e2e"
access_key_id = "$accessKey"
secret_access_key_file = "$keyFile"
path_style = true
"@ | Set-Content (Join-Path $config 'config.toml')
    # 凭据文件：注意不要有尾随换行以外的内容
    Set-Content (Join-Path $config "secrets\$keyFile") $secretKey -NoNewline
    return @{ Config = $config; Data = $data; Cache = $cache }
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

Write-Host "临时目录：$root" -ForegroundColor DarkGray
try {
    Step '构建 release 二进制'
    Push-Location $repo
    & cargo build --release --bin cx 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    $script:cx = if ($isWindowsHost) {
        Join-Path $repo 'target\release\cx.exe'
    }
    else { Join-Path $repo 'target/release/cx' }
    Assert-True (Test-Path $script:cx) '二进制就绪'

    Step '准备 S3 兼容服务端（moto）'
    & python -m venv $venv 2>&1 | Select-Object -Last 1
    $pip = Join-Path $venv 'Scripts\pip.exe'
    if (-not (Test-Path $pip)) { $pip = Join-Path $venv 'bin/pip' }
    & $pip install --quiet 'moto[server]' 2>&1 | Select-Object -Last 2
    $moto = Join-Path $venv 'Scripts\moto_server.exe'
    if (-not (Test-Path $moto)) { $moto = Join-Path $venv 'bin/moto_server' }
    if (-not (Test-Path $moto)) {
        # 装不到就说清楚是环境问题，别让下一步抛"找不到 bin/moto_server"这种怪错
        throw ("装不到 moto（需要联网执行 pip install 'moto[server]'）。" +
            "S3 的线格式与签名由仓库内测试服务端覆盖（cargo test -p cx-backend --test s3_server），" +
            "这里只是额外用 moto 这份独立实现验证互通；联网后重跑本脚本即可。")
    }

    $s3Process = Start-Hidden $moto @('-p', "$port", '-H', '127.0.0.1') $null
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 500
        try {
            Invoke-WebRequest -Uri "$endpoint/" -TimeoutSec 2 -UseBasicParsing | Out-Null
            $ready = $true; break
        }
        catch {
            if ($null -ne $_.Exception.Response) { $ready = $true; break }
        }
    }
    Assert-True $ready "S3 服务端已就绪：$endpoint"
    if (-not $ready) { throw 'S3 服务端未就绪' }

    Step '显式创建桶（连接检查与对象写入都不再隐式建桶）'
    $python = Join-Path $venv 'Scripts/python.exe'
    if (-not (Test-Path $python)) { $python = Join-Path $venv 'bin/python' }
    $createScript = Join-Path $root 'create-bucket.py'
    @"
import boto3
boto3.client(
    "s3",
    endpoint_url="$endpoint",
    region_name="us-east-1",
    aws_access_key_id="$accessKey",
    aws_secret_access_key="$secretKey",
).create_bucket(Bucket="$bucket")
print("bucket ready")
"@ | Set-Content $createScript
    $created = & $python $createScript 2>&1
    Assert-True ($LASTEXITCODE -eq 0) "桶已创建：$($created | Select-Object -Last 1)"
    if ($LASTEXITCODE -ne 0) { throw '创建桶失败' }

    Step '准备两台设备的隔离目录'
    $deviceA = New-DeviceEnv 'device-a' $endpoint $bucket 'oss.key'
    $deviceB = New-DeviceEnv 'device-b' $endpoint $bucket 'oss.key'
    Assert-True (Test-Path (Join-Path $deviceA.Config 'config.toml')) 'A 的配置已写入'
    Assert-True (Test-Path (Join-Path $deviceB.Config 'secrets/oss.key')) 'A 的凭据已写入'

    Step '直连测试后端（只读：列举前缀 + 空间元数据，不建桶）'
    $probe = Invoke-Device $deviceA @('backend', 'test', 'oss')
    Assert-True ($probe.Code -eq 0) "backend test 通过：$($probe.Output)"

    # ---- 剪贴板能力以 cx doctor 的判断为准 ----
    $doctor = Convert-CxJson (Invoke-Device $deviceA @('doctor', '--json')).Output
    $clipCheck = $doctor.checks | Where-Object { $_.name -eq '剪贴板' } | Select-Object -First 1
    $daemonClipboard = (-not $NoClipboard) -and ($null -ne $clipCheck) -and $clipCheck.ok
    $scriptClipboard = $false
    if ($daemonClipboard) {
        try {
            $originalClipboard = Get-Clipboard -Raw
            Set-Clipboard -Value 'cx-s3-probe'
            $scriptClipboard = (Get-Clipboard -Raw) -eq 'cx-s3-probe'
        }
        catch { $scriptClipboard = $false; $originalClipboard = $null }
    }
    Write-Host ("守护进程剪贴板：" + $(if ($daemonClipboard) { '可用' } else { '不可用' })) -ForegroundColor DarkGray

    Step '启动两台守护进程'
    $relayArgs = if ($daemonClipboard) { @() } else { @('--no-clipboard') }
    $procA = Start-Hidden $script:cx (@('daemon', '--foreground', '--quiet') + $relayArgs) @{
        CX_CONFIG_DIR = $deviceA.Config; CX_DATA_DIR = $deviceA.Data; CX_CACHE_DIR = $deviceA.Cache; CX_LOG = 'debug'
    }
    $procB = Start-Hidden $script:cx (@('daemon', '--foreground', '--quiet') + $relayArgs) @{
        CX_CONFIG_DIR = $deviceB.Config; CX_DATA_DIR = $deviceB.Data; CX_CACHE_DIR = $deviceB.Cache; CX_LOG = 'debug'
    }
    $bothUp = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        if ((Test-Path (Join-Path $deviceA.Data 'daemon.json')) -and (Test-Path (Join-Path $deviceB.Data 'daemon.json'))) {
            $bothUp = $true; break
        }
    }
    Assert-True $bothUp '两台守护进程都已就绪'

    # ---------- A → B ----------
    $textA = "来自 S3 上 A 的内容 " + (Get-Random)
    if ($scriptClipboard) {
        Step "A 复制到剪贴板：$textA"
        Set-Clipboard -Value $textA
    }
    else {
        Step "A 通过 cx push 发布：$textA"
        $push = Invoke-Device $deviceA @('push', '--text', $textA)
        Assert-True ($push.Code -eq 0) "A 的 push 成功（$($push.Output)）"
    }
    Start-Sleep -Seconds $WaitSeconds

    Step 'B 应通过 S3 收到该内容'
    $received = $false
    for ($i = 0; $i -lt 20; $i++) {
        $history = Invoke-Device $deviceB @('history', '--json')
        if ($history.Code -eq 0) {
            $items = Convert-CxJson $history.Output
            if ($items.Count -ge 1) {
                $preview = Convert-CxJson (Invoke-Device $deviceB @('show', '0', '--json')).Output
                if ($preview.text -eq $textA) { $received = $true; break }
            }
        }
        Start-Sleep -Seconds 2
    }
    Assert-True $received "B 的历史里出现了 A 的内容（等待 $($i * 2) 秒）"

    # ---------- B → A ----------
    $textB = "来自 S3 上 B 的回复 " + (Get-Random)
    Step "B 反向发布：$textB"
    $push = Invoke-Device $deviceB @('push', '--text', $textB)
    Assert-True ($push.Code -eq 0) "B 的 push 成功（$($push.Output)）"

    Step 'A 应收到 B 的内容'
    $applied = $false
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Seconds 2
        if ($scriptClipboard) {
            if ((Get-Clipboard -Raw) -eq $textB) { $applied = $true; break }
        }
        else {
            $preview = Convert-CxJson (Invoke-Device $deviceA @('show', '0', '--json')).Output
            if ($preview.text -eq $textB) { $applied = $true; break }
        }
    }
    $where = if ($scriptClipboard) { 'A 的剪贴板' } else { 'A 的历史' }
    Assert-True $applied "$where 已变成本地 B 的内容（等待 $($i * 2) 秒）"

    Step '两边都不产生重复上传（防回环）'
    Start-Sleep -Seconds $WaitSeconds
    $historyA = Convert-CxJson (Invoke-Device $deviceA @('history', '--json')).Output
    $historyB = Convert-CxJson (Invoke-Device $deviceB @('history', '--json')).Output
    Assert-True ($historyA.Count -le 3) "A 的历史没有膨胀（$($historyA.Count) 条）"
    Assert-True ($historyB.Count -le 3) "B 的历史没有膨胀（$($historyB.Count) 条）"

    Step '运行中的诊断应报告后端状态'
    $daemonReport = Convert-CxJson (Invoke-Device $deviceB @('doctor', '--json')).Output
    $backendCheck = $daemonReport.checks | Where-Object { $_.name -eq '后端' } | Select-Object -First 1
    Assert-True ($null -ne $backendCheck -and $backendCheck.ok) "诊断报告后端已启用：$($backendCheck.detail)"

    Step '停止两台守护进程'
    Assert-True ((Invoke-Device $deviceA @('stop')).Code -eq 0) 'A 已停止'
    Assert-True ((Invoke-Device $deviceB @('stop')).Code -eq 0) 'B 已停止'
}
catch {
    # 环境不满足（例如离线装不到 moto/wsgidav）时单独归类：本地跳过，CI 里仍算失败，
    # 免得"依赖装不上"被当成"实现有问题"，也免得在 CI 上被静默跳过。
    if ($_.Exception.Message -like '装不到*') {
        Write-Host ''
        Write-Host "跳过：$($_.Exception.Message)" -ForegroundColor Yellow
        if ($env:CI) {
            Fail "环境不满足：$($_.Exception.Message)"
        }
        $script:skipped = $true
    }
    else {
        Fail "未预期的异常：$($_.Exception.Message)"
    }
}
finally {
    if ($null -ne $originalClipboard) {
        try { Set-Clipboard -Value $originalClipboard; Write-Host '已恢复原剪贴板内容' -ForegroundColor DarkGray } catch { }
    }
    foreach ($proc in @($procA, $procB, $s3Process)) {
        if ($null -ne $proc) {
            $alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
            if ($null -ne $alive) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
    foreach ($device in @($deviceA, $deviceB)) {
        if ($null -ne $device) {
            $log = Join-Path $device.Cache 'logs/daemon.log'
            if (Test-Path $log) {
                Write-Host "`n--- $($device.Config) 日志（末尾 10 行）---" -ForegroundColor DarkGray
                Get-Content $log -Tail 10 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
            }
        }
    }
    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0 -and $script:skipped) {
    Write-Host '本次跳过（环境不满足，见上面原因）' -ForegroundColor Yellow
    exit 0
}
if ($failures.Count -eq 0) {
    Write-Host 'S3 双设备端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "S3 端到端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}