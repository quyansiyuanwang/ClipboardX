
#
# 用法：
#   pwsh scripts/e2e-server.ps1              # 自动探测剪贴板
#   pwsh scripts/e2e-server.ps1 -NoClipboard # 强制无剪贴板路径（无头 CI 用）
#   pwsh scripts/e2e-server.ps1 -KeepTemp
#
# 与 e2e-webdav / e2e-s3 的区别：那两条验的是"第三方存储作为传输层"，
# 这条验的是**整套自建方案**，而且不依赖任何外部服务（cx-server 就是我们自己的二进制）：
#
#   1. cx-server space create      → 拿到空间 id 与注册密钥
#   2. 设备 A 自助注册（POST /v1/enroll）→ 建空间 → 把空间元数据上传到服务端
#   3. 设备 B 用注册密钥自助注册 → cx space join（拉 cx-space.json + 口令解出密钥）
#   4. 两台守护进程互相同步（A 剪贴板 → 服务端 → B 历史；B 发布 → A）
#   5. **直接在服务端数据目录里断言只有密文**：明文不能出现在任何对象里
[CmdletBinding()]
param(
    [switch]$NoClipboard,
    [switch]$KeepTemp,
    [int]$WaitSeconds = 6
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-e2e-srv-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$serverData = Join-Path $root 'server'
$port = 18688 + (Get-Random -Maximum 200)
$baseUrl = "http://127.0.0.1:$port"
$passphrase = 'e2e-passphrase-4f9a'

$failures = [System.Collections.Generic.List[string]]::new()
$originalClipboard = $null
$serverProcess = $null
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

function New-DeviceEnv([string]$name, [string]$url, [string]$spaceId, [string]$enrollSecret) {
    $deviceRoot = Join-Path $root $name
    $config = Join-Path $deviceRoot 'config'
    $data = Join-Path $deviceRoot 'data'
    $cache = Join-Path $deviceRoot 'cache'
    New-Item -ItemType Directory -Force -Path $config, $data, $cache | Out-Null
    # 缩短轮询间隔
    @"
[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 300
debounce_ms = 100
"@ | Set-Content (Join-Path $config 'config.toml')
    return @{
        Config = $config; Data = $data; Cache = $cache; Root = $deviceRoot
        Url = $url; SpaceId = $spaceId; EnrollSecret = $enrollSecret
    }
}

function Invoke-Device([hashtable]$device, [string[]]$Arguments, [string]$Stdin) {
    $env:CX_CONFIG_DIR = $device.Config
    $env:CX_DATA_DIR = $device.Data
    $env:CX_CACHE_DIR = $device.Cache
    if ($null -ne $Stdin) {
        $output = $Stdin | & $script:cx @Arguments 2>&1
    }
    else {
        $output = & $script:cx @Arguments 2>&1
    }
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
    Step '构建 release 二进制（cx 与 cx-server）'
    Push-Location $repo
    & cargo build --release --bin cx --bin cx-server 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "    $_" }
    $buildCode = $LASTEXITCODE
    Pop-Location
    if ($buildCode -ne 0) { throw "cargo build 失败（exit $buildCode）" }
    $suffix = if ($isWindowsHost) { '.exe' } else { '' }
    $script:cx = Join-Path $repo "target\release\cx$suffix"
    $cxServer = Join-Path $repo "target\release\cx-server$suffix"
    Assert-True ((Test-Path $script:cx) -and (Test-Path $cxServer)) '两个二进制就绪'

    Step '创建空间并启动服务端'
    New-Item -ItemType Directory -Force -Path $serverData | Out-Null
    $env:CX_SERVER_DATA = $serverData
    $created = & $cxServer space create '家庭空间' --quota 32MB --data $serverData 2>&1
    $spaceId = ($created | Select-String -Pattern '空间已创建：(.+)').Matches[0].Groups[1].Value.Trim()
    $enrollSecret = ($created | Select-String -Pattern '注册密钥：(.+)').Matches[0].Groups[1].Value.Trim()
    Assert-True ($spaceId.Length -eq 32) "空间 id 已生成：$spaceId"
    Assert-True ($enrollSecret.StartsWith('cxe_')) '注册密钥已生成'

    $serverProcess = Start-Hidden $cxServer @('serve', '--listen', "127.0.0.1:$port", '--quiet', '--data', $serverData) @{ CX_SERVER_DATA = $serverData }
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        try {
            $response = Invoke-WebRequest -Uri "$baseUrl/healthz" -TimeoutSec 2 -UseBasicParsing
            if ($response.StatusCode -eq 200) { $ready = $true; break }
        }
        catch { }
    }
    Assert-True $ready "服务端已就绪：$baseUrl"
    if (-not $ready) {
        $log = Join-Path $serverData 'logs/cx-server.log'
        if (Test-Path $log) {
            Write-Host '    服务端日志尾部：' -ForegroundColor Yellow
            Get-Content $log -Tail 8 | ForEach-Object { Write-Host "      $_" -ForegroundColor Yellow }
        }
        throw '服务端未就绪'
    }

    Step '准备两台设备'
    $deviceA = New-DeviceEnv 'device-a' $baseUrl $spaceId $enrollSecret
    $deviceB = New-DeviceEnv 'device-b' $baseUrl $spaceId $enrollSecret

    Step '设备 A 自助注册并配置后端'
    $add = Invoke-Device $deviceA @('backend', 'add', 'server', 'home', $baseUrl, '--space', $spaceId, '--enroll-secret', $enrollSecret)
    Assert-True ($add.Code -eq 0) "A 配置后端成功（$($add.Output)）"
    Assert-True (Test-Path (Join-Path $deviceA.Config 'secrets/home.token')) 'A 的设备令牌已写入 secrets/'

    Step '设备 A 建空间并上传空间元数据'
    $init = Invoke-Device $deviceA @('space', 'init', '--passphrase-stdin') $passphrase
    Assert-True ($init.Code -eq 0) "A 建空间成功（$($init.Output)）"
    $publish = Invoke-Device $deviceA @('space', 'publish', 'home')
    Assert-True ($publish.Code -eq 0) "空间元数据已上传（$($publish.Output)）"

    Step '设备 B 自助注册并加入空间'
    $addB = Invoke-Device $deviceB @('backend', 'add', 'server', 'home', $baseUrl, '--space', $spaceId, '--enroll-secret', $enrollSecret)
    Assert-True ($addB.Code -eq 0) "B 配置后端成功（$($addB.Output)）"
    $join = Invoke-Device $deviceB @('space', 'join', 'home', '--passphrase-stdin') $passphrase
    Assert-True ($join.Code -eq 0) "B 加入空间成功（$($join.Output)）"
    # 注意两个"空间 id"是不同的东西：
    #   - 服务端空间 id（十六进制）：存储租户，决定令牌与配额；
    #   - 客户端空间 id（base32）：加密空间，写在 cx-space.json 里。
    # 断言要比的是后者（A 与 B 必须是同一个加密空间）。
    $spaceA = Convert-CxJson (Invoke-Device $deviceA @('space', 'show', '--json')).Output
    $spaceB = Convert-CxJson (Invoke-Device $deviceB @('space', 'show', '--json')).Output
    Assert-True ($spaceB.space_id -eq $spaceA.space_id) "B 与 A 属于同一个加密空间（$($spaceB.space_id)）"
    Assert-True ($spaceB.cipher -eq 'xchacha20poly1305') "B 拿到了加密空间（$($spaceB.cipher)）"
    Assert-True ($spaceB.space_id -ne $spaceId) '客户端空间 id 与服务端空间 id（存储租户）本来就是两个标识'

    Step '后端连通性（两端各测一次）'
    Assert-True ((Invoke-Device $deviceA @('backend', 'test', 'home')).Code -eq 0) 'A 的后端可用'
    Assert-True ((Invoke-Device $deviceB @('backend', 'test', 'home')).Code -eq 0) 'B 的后端可用'

    # 剪贴板能力以 cx doctor 的判断为准（无头环境用 --no-clipboard）
    $doctor = Convert-CxJson (Invoke-Device $deviceA @('doctor', '--json')).Output
    $clipCheck = $doctor.checks | Where-Object { $_.name -eq '剪贴板' } | Select-Object -First 1
    $daemonClipboard = (-not $NoClipboard) -and ($null -ne $clipCheck) -and $clipCheck.ok
    $scriptClipboard = $false
    if ($daemonClipboard) {
        try {
            $originalClipboard = Get-Clipboard -Raw
            Set-Clipboard -Value 'cx-e2e-server-probe'
            $scriptClipboard = (Get-Clipboard -Raw) -eq 'cx-e2e-server-probe'
        }
        catch { $scriptClipboard = $false; $originalClipboard = $null }
    }
    Write-Host ("守护进程剪贴板：" + $(if ($daemonClipboard) { '可用' } else { '不可用（用 push/历史代替）' })) -ForegroundColor DarkGray

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
    $textA = "自建服务端第一条明文 " + (Get-Random)
    if ($scriptClipboard) {
        Step "A 复制到剪贴板：$textA"
        Set-Clipboard -Value $textA
    }
    else {
        Step "A 通过 cx push 发布：$textA"
        Assert-True ((Invoke-Device $deviceA @('push', '--text', $textA)).Code -eq 0) 'A 的 push 成功'
    }
    Start-Sleep -Seconds $WaitSeconds

    Step 'B 应通过自建服务端收到该内容'
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
    $textB = "自建服务端第二条明文 " + (Get-Random)
    Step "B 反向发布：$textB"
    Assert-True ((Invoke-Device $deviceB @('push', '--text', $textB)).Code -eq 0) 'B 的 push 成功'

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

    Step '服务端只应看到密文（这是自建方案的核心承诺）'
    $objectsDir = Join-Path $serverData (Join-Path 'spaces' (Join-Path $spaceId 'objects'))
    $leaked = @()
    if (Test-Path $objectsDir) {
        foreach ($file in Get-ChildItem -Recurse -File $objectsDir) {
            $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
            $text = [System.Text.Encoding]::UTF8.GetString($bytes)
            if ($text.Contains($textA) -or $text.Contains($textB)) {
                $leaked += $file.FullName
            }
        }
    }
    Assert-True ($leaked.Count -eq 0) "明文没有出现在任何对象里（检查了 $((Get-ChildItem -Recurse -File $objectsDir -ErrorAction SilentlyContinue).Count) 个文件）"
    if ($leaked.Count -gt 0) { $leaked | ForEach-Object { Write-Host "      泄漏：$_" -ForegroundColor Red } }

    Step '服务端配额统计应反映真实用量'
    $spaceInfo = & $cxServer space show $spaceId --data $serverData 2>&1
    $usedLine = ($spaceInfo | Select-String -Pattern '已用：(\d+)').Matches[0].Groups[1].Value
    Assert-True ([int]$usedLine -gt 0) "服务端统计到已用 $usedLine 字节"
    $devicesOut = & $cxServer devices $spaceId --data $serverData 2>&1
    Assert-True (($devicesOut -join "`n").Contains('设备') -or ($devicesOut | Measure-Object).Count -ge 2) '服务端记录了设备'

    Step '两边都不产生重复上传（防回环）'
    Start-Sleep -Seconds $WaitSeconds
    $historyA = Convert-CxJson (Invoke-Device $deviceA @('history', '--json')).Output
    $historyB = Convert-CxJson (Invoke-Device $deviceB @('history', '--json')).Output
    Assert-True ($historyA.Count -le 3) "A 的历史没有膨胀（$($historyA.Count) 条）"
    Assert-True ($historyB.Count -le 3) "B 的历史没有膨胀（$($historyB.Count) 条）"

    Step '停止两台守护进程与服务端'
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
    foreach ($proc in @($procA, $procB, $serverProcess)) {
        if ($null -ne $proc) {
            $alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
            if ($null -ne $alive) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
        }
    }
    if ($KeepTemp) { Write-Host "临时目录保留在：$root" -ForegroundColor DarkGray }
    else { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host '自建服务端 MVP 端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "自建服务端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
