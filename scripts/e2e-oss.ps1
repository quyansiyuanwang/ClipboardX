# 端到端验收（**真实** S3 / 阿里云 OSS）：两台设备经真实对象存储同步，端到端加密
#
# 与 e2e-s3.ps1 的分工：
# - e2e-s3.ps1 用 moto（本地假服务端）验证线格式与完整同步链路，CI 里跑；
# - 本脚本打**真实 OSS**，验证真证书链 / 真 OSS 签名校验 / 真对象布局 / 真加密空间，
#   因为它是唯一能证明"用户实际配置能通"的验收。需要真实的桶与凭据。
#
# 用法（凭据只从环境变量或参数进来，不落盘、不进日志）：
#   $env:CX_OSS_ENDPOINT = 'https://oss-cn-beijing.aliyuncs.com'
#   $env:CX_OSS_BUCKET   = 'clipboard-sync'
#   $env:CX_OSS_PREFIX   = 'clip/_diag/sync'      # 必须是一个专用前缀，脚本会在结束时清空
#   $env:CX_OSS_REGION   = 'cn-beijing'
#   $env:CX_OSS_ACCESS_KEY / CX_OSS_SECRET_KEY
#   pwsh scripts/e2e-oss.ps1
#
#   pwsh scripts/e2e-oss.ps1 -KeepTemp          # 保留临时目录（排障）
#   pwsh scripts/e2e-oss.ps1 -NoCleanup         # 结束后不清理云端对象（留证据用）
#
# 安全：只写这个前缀；结束时（默认）把该前缀下的对象全部删除，并断言没有残留。
[CmdletBinding()]
param(
    [string]$Endpoint = $env:CX_OSS_ENDPOINT,
    [string]$Bucket = $env:CX_OSS_BUCKET,
    [string]$Prefix = $env:CX_OSS_PREFIX,
    [string]$Region = $(if ($env:CX_OSS_REGION) { $env:CX_OSS_REGION } else { 'cn-beijing' }),
    [string]$AccessKey = $env:CX_OSS_ACCESS_KEY,
    [string]$SecretKey = $env:CX_OSS_SECRET_KEY,
    [switch]$PathStyle,
    [switch]$KeepTemp,
    [switch]$NoCleanup,
    [int]$WaitSeconds = 8
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$isWindowsHost = $IsWindows -or ($PSVersionTable.PSEdition -eq 'Desktop')
$root = Join-Path ([IO.Path]::GetTempPath()) ("cx-oss-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$passphrase = 'e2e-oss-passphrase-7c31'

$failures = [System.Collections.Generic.List[string]]::new()
$procA = $null
$procB = $null
$deviceA = $null
$deviceB = $null

function Step([string]$name) { Write-Host "==> $name" -ForegroundColor Cyan }
function Ok([string]$message) { Write-Host "    [ok] $message" -ForegroundColor Green }
function Fail([string]$message) { Write-Host "    [fail] $message" -ForegroundColor Red; $failures.Add($message) }
function Assert-True([bool]$condition, [string]$message) { if ($condition) { Ok $message } else { Fail $message } }
function Skip([string]$message) { Write-Host "    [skip] $message" -ForegroundColor Yellow }

# ---------------------------------------------------------------- 最小 S3 客户端
# 只做本脚本需要的事：ListObjectsV2 / DeleteObject。自己算 SigV4 是有意的——
# 我们用真实 OSS 校验**产品自己的**签名实现，清理动作用独立的一份代码写，
# 两边不共享逻辑（共享的话签名 bug 会互相掩盖）。

function Get-Sha256Hex([byte[]]$bytes) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

function Get-Hmac([byte[]]$key, [string]$data) {
    $hmac = [System.Security.Cryptography.HMACSHA256]::new($key)
    try { return $hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes($data)) }
    finally { $hmac.Dispose() }
}

function Get-UriEncoded([string]$text) {
    # RFC3986：除 A-Za-z0-9-._~ 外全部百分号编码（空格是 %20，斜杠是 %2F）
    $sb = [System.Text.StringBuilder]::new()
    foreach ($byte in [Text.Encoding]::UTF8.GetBytes($text)) {
        $ch = [char]$byte
        if (($ch -ge 'A' -and $ch -le 'Z') -or ($ch -ge 'a' -and $ch -le 'z') -or
            ($ch -ge '0' -and $ch -le '9') -or $ch -eq '-' -or $ch -eq '.' -or
            $ch -eq '_' -or $ch -eq '~') {
            [void]$sb.Append($ch)
        }
        else { [void]$sb.Append(('%{0:X2}' -f $byte)) }
    }
    return $sb.ToString()
}

$script:hostName = if ($PathStyle) { $Endpoint -replace '^https?://', '' } else { "$Bucket.$($Endpoint -replace '^https?://', '')" }
$script:scheme = if ($Endpoint -match '^http://') { 'http' } else { 'https' }

function Invoke-S3([string]$method, [string]$key, [hashtable]$query = @{}) {
    $uri = if ($PathStyle) { "/$Bucket" + $(if ($key) { "/$key" } else { '' }) } else { "/" + $(if ($key) { $key } else { '' }) }
    $amzDate = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
    $dateStamp = $amzDate.Substring(0, 8)
    $payloadHash = Get-Sha256Hex ([byte[]]@())

    $canonicalQuery = ($query.GetEnumerator() | Sort-Object Key | ForEach-Object {
            "$(Get-UriEncoded $_.Key)=$(Get-UriEncoded $_.Value)"
        }) -join '&'
    $canonicalUri = (($uri -split '/' | ForEach-Object { Get-UriEncoded $_ }) -join '/')
    $canonicalHeaders = "host:$($script:hostName)`nx-amz-content-sha256:$payloadHash`nx-amz-date:$amzDate`n"
    $signedHeaders = 'host;x-amz-content-sha256;x-amz-date'
    $canonicalRequest = "$method`n$canonicalUri`n$canonicalQuery`n$canonicalHeaders`n$signedHeaders`n$payloadHash"
    $scope = "$dateStamp/$Region/s3/aws4_request"
    $stringToSign = "AWS4-HMAC-SHA256`n$amzDate`n$scope`n$(Get-Sha256Hex ([Text.Encoding]::UTF8.GetBytes($canonicalRequest)))"
    $kDate = Get-Hmac ([Text.Encoding]::UTF8.GetBytes("AWS4$SecretKey")) $dateStamp
    $kRegion = Get-Hmac $kDate $Region
    $kService = Get-Hmac $kRegion 's3'
    $kSigning = Get-Hmac $kService 'aws4_request'
    $signature = ([BitConverter]::ToString((Get-Hmac $kSigning $stringToSign)) -replace '-', '').ToLowerInvariant()
    $authorization = "AWS4-HMAC-SHA256 Credential=$AccessKey/$scope, SignedHeaders=$signedHeaders, Signature=$signature"

    $url = "$($script:scheme)://$($script:hostName)$canonicalUri"
    if ($canonicalQuery) { $url += "?$canonicalQuery" }

    # 用 curl 而不是 Invoke-WebRequest：PowerShell 会校验 Authorization 头的格式
    # （"AWS4-HMAC-SHA256 Credential=…, SignedHeaders=…" 被它判为非法值），curl 不会。
    $bodyFile = [IO.Path]::GetTempFileName()
    $outFile = "$bodyFile.out"
    try {
        $curlArgs = @(
            '-s', '-o', $outFile, '-w', '%{http_code}',
            '-X', $method, $url,
            '-H', "x-amz-content-sha256: $payloadHash",
            '-H', "x-amz-date: $amzDate",
            '-H', "Authorization: $authorization",
            '--max-time', '30'
        )
        $status = & curl.exe @curlArgs
        $content = if (Test-Path $outFile) { Get-Content $outFile -Raw } else { '' }
        return @{ StatusCode = [int]$status; Content = [string]$content }
    }
    finally {
        Remove-Item $bodyFile, $outFile -Force -ErrorAction SilentlyContinue
    }
}

function Get-ObjectsUnderPrefix() {
    $keys = [System.Collections.Generic.List[string]]::new()
    $token = $null
    for ($page = 0; $page -lt 50; $page++) {
        $query = @{ 'list-type' = '2'; 'prefix' = $Prefix }
        if ($token) { $query['continuation-token'] = $token }
        $response = Invoke-S3 'GET' '' $query
        if ($response.StatusCode -ne 200) {
            throw "列举对象失败：HTTP $($response.StatusCode) $($response.Content.Substring(0, [Math]::Min(200, $response.Content.Length)))"
        }
        [xml]$xml = $response.Content
        foreach ($node in $xml.ListBucketResult.Contents) { if ($node.Key) { $keys.Add([string]$node.Key) } }
        if ($xml.ListBucketResult.IsTruncated -ne 'true') { break }
        $token = [string]$xml.ListBucketResult.NextContinuationToken
        if (-not $token) { break }
    }
    return $keys
}

function Clear-Prefix() {
    $keys = @(Get-ObjectsUnderPrefix)
    foreach ($key in $keys) {
        $response = Invoke-S3 'DELETE' $key
        if ($response.StatusCode -notin @(204, 200)) {
            Fail "删除对象 $key 失败：HTTP $($response.StatusCode)"
        }
    }
    $left = @(Get-ObjectsUnderPrefix)
    Assert-True ($left.Count -eq 0) "云端前缀已清空（删了 $($keys.Count) 个，残留 $($left.Count) 个）"
    foreach ($key in $left) { Write-Host "    残留：$key" -ForegroundColor DarkYellow }
}

# ---------------------------------------------------------------- 设备环境
function New-DeviceEnv([string]$name) {
    $deviceRoot = Join-Path $root $name
    $config = Join-Path $deviceRoot 'config'
    $data = Join-Path $deviceRoot 'data'
    $cache = Join-Path $deviceRoot 'cache'
    New-Item -ItemType Directory -Force -Path $config, $data, $cache, (Join-Path $config 'secrets') | Out-Null
    $pathStyleLine = if ($PathStyle) { 'path_style = true' } else { 'path_style = false' }
    @"
[space]
encrypt = true

[sync]
poll_active_ms = 1000
poll_idle_ms = 2000
watch_interval_ms = 300
debounce_ms = 100

[[backend]]
name = "oss"
kind = "s3"
endpoint = "$Endpoint"
bucket = "$Bucket"
region = "$Region"
prefix = "$Prefix"
access_key_id = "$AccessKey"
secret_access_key_file = "oss.key"
$pathStyleLine
"@ | Set-Content (Join-Path $config 'config.toml')
    Set-Content (Join-Path $config 'secrets\oss.key') $SecretKey -NoNewline
    return @{ Config = $config; Data = $data; Cache = $cache }
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
    $start = ($lines | Select-String -Pattern '^\s*[\[\{]' | Select-Object -First 1)
    if ($null -eq $start) { return $null }
    return ($lines[($start.LineNumber - 1)..($lines.Count - 1)] -join "`n") | ConvertFrom-Json
}

# ---------------------------------------------------------------- 前置检查
Step '检查参数与凭据'
if (-not $Endpoint) { throw '缺少端点（-Endpoint 或 CX_OSS_ENDPOINT）' }
if (-not $Bucket) { throw '缺少桶名（-Bucket 或 CX_OSS_BUCKET）' }
if (-not $Prefix) { throw '缺少前缀（-Prefix 或 CX_OSS_PREFIX）：本脚本会删空这个前缀，必须显式指定' }
if (-not $AccessKey -or -not $SecretKey) { throw '缺少凭据（CX_OSS_ACCESS_KEY / CX_OSS_SECRET_KEY）' }
Assert-True ($Prefix.Trim() -ne '' -and $Prefix.Trim() -ne '/') "前缀是专用的非根路径：$Prefix"
Write-Host "    端点 $Endpoint；桶 $Bucket；区域 $Region；前缀 $Prefix" -ForegroundColor DarkGray

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

    Step '准备两台设备的隔离目录（加密空间）'
    $deviceA = New-DeviceEnv 'device-a'
    $deviceB = New-DeviceEnv 'device-b'
    Assert-True (Test-Path (Join-Path $deviceA.Config 'config.toml')) 'A 的配置已写入'
    Assert-True (Test-Path (Join-Path $deviceB.Config 'secrets/oss.key')) 'B 的凭据已写入'

    Step '直连真实 OSS（只读：列举前缀 + 空间元数据）'
    $probe = Invoke-Device $deviceA @('backend', 'test', 'oss')
    Assert-True ($probe.Code -eq 0) "backend test 通过：$($probe.Output)"
    if ($probe.Code -ne 0) { throw '真实存储连不上，后续步骤没有意义' }

    Step '设备 A 在云端建加密空间'
    $init = Invoke-Device $deviceA @('space', 'init', '--passphrase-stdin') $passphrase
    Assert-True ($init.Code -eq 0) "A 建空间成功（$($init.Output)）"
    $publish = Invoke-Device $deviceA @('space', 'publish', 'oss')
    Assert-True ($publish.Code -eq 0) "空间元数据已上传（$($publish.Output)）"

    Step '设备 B 用口令加入同一个空间'
    $join = Invoke-Device $deviceB @('space', 'join', 'oss', '--passphrase-stdin') $passphrase
    Assert-True ($join.Code -eq 0) "B 加入空间成功（$($join.Output)）"
    $spaceA = Convert-CxJson (Invoke-Device $deviceA @('space', 'show', '--json')).Output
    $spaceB = Convert-CxJson (Invoke-Device $deviceB @('space', 'show', '--json')).Output
    if ($null -eq $spaceA -or $null -eq $spaceB) {
        Fail '读不到空间信息（space show --json 没有输出）'
        throw '空间未建立，后续步骤没有意义'
    }
    Assert-True ($spaceA.space_id -eq $spaceB.space_id) "两端是同一个加密空间（$($spaceA.space_id)）"
    # 加密与否看人读的输出（JSON 里只有元数据的原始字段，就别猜字段名了）
    $showA = Invoke-Device $deviceA @('space', 'show')
    Assert-True ($showA.Output -match 'XChaCha20-Poly1305') "空间在 A 上是加密的：$($showA.Output -replace "`n", ' ')"

    Step '启动两台守护进程'
    $daemonEnvA = @{ CX_CONFIG_DIR = $deviceA.Config; CX_DATA_DIR = $deviceA.Data; CX_CACHE_DIR = $deviceA.Cache; CX_LOG = 'debug' }
    $daemonEnvB = @{ CX_CONFIG_DIR = $deviceB.Config; CX_DATA_DIR = $deviceB.Data; CX_CACHE_DIR = $deviceB.Cache; CX_LOG = 'debug' }
    $startParamsA = @{ FilePath = $script:cx; ArgumentList = @('daemon', '--foreground', '--quiet', '--no-clipboard'); PassThru = $true; Environment = $daemonEnvA }
    $startParamsB = @{ FilePath = $script:cx; ArgumentList = @('daemon', '--foreground', '--quiet', '--no-clipboard'); PassThru = $true; Environment = $daemonEnvB }
    if ($isWindowsHost) { $startParamsA['WindowStyle'] = 'Hidden'; $startParamsB['WindowStyle'] = 'Hidden' }
    $procA = Start-Process @startParamsA
    $procB = Start-Process @startParamsB
    $bothUp = $false
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        if ((Test-Path (Join-Path $deviceA.Data 'daemon.json')) -and (Test-Path (Join-Path $deviceB.Data 'daemon.json'))) { $bothUp = $true; break }
    }
    Assert-True $bothUp '两台守护进程都已就绪'

    # ---------- A → B ----------
    $textA = "real-oss-A - " + (Get-Random)
    Step "A 发布：$textA"
    $pushA = Invoke-Device $deviceA @('push', '--text', $textA)
    Assert-True ($pushA.Code -eq 0) "A 的 push 成功（$($pushA.Output)）"

    Step 'B 应通过真实 OSS 收到该内容'
    $received = $false
    for ($i = 0; $i -lt 25; $i++) {
        Start-Sleep -Seconds 2
        $history = Invoke-Device $deviceB @('history', '--json')
        if ($history.Code -eq 0) {
            $items = Convert-CxJson $history.Output
            if ($items.Count -ge 1) {
                $preview = Convert-CxJson (Invoke-Device $deviceB @('show', '0', '--json')).Output
                if ($preview.text -eq $textA) { $received = $true; break }
            }
        }
    }
    Assert-True $received "B 的历史里出现了 A 的内容（等待 $($i * 2) 秒）"

    # ---------- B → A ----------
    $textB = "real-oss-B - " + (Get-Random)
    Step "B 反向发布：$textB"
    $pushB = Invoke-Device $deviceB @('push', '--text', $textB)
    Assert-True ($pushB.Code -eq 0) "B 的 push 成功（$($pushB.Output)）"

    Step 'A 应收到 B 的内容'
    $applied = $false
    for ($i = 0; $i -lt 25; $i++) {
        Start-Sleep -Seconds 2
        $preview = Convert-CxJson (Invoke-Device $deviceA @('show', '0', '--json')).Output
        if ($preview.text -eq $textB) { $applied = $true; break }
    }
    Assert-True $applied "A 的历史已变成本地 B 的内容（等待 $($i * 2) 秒）"

    Step '没有重复上传（防回环）'
    Start-Sleep -Seconds $WaitSeconds
    $historyA = Convert-CxJson (Invoke-Device $deviceA @('history', '--json')).Output
    $historyB = Convert-CxJson (Invoke-Device $deviceB @('history', '--json')).Output
    Assert-True ($historyA.Count -le 3) "A 的历史没有膨胀（$($historyA.Count) 条）"
    Assert-True ($historyB.Count -le 3) "B 的历史没有膨胀（$($historyB.Count) 条）"

    Step '云端对象布局：前缀下只有加密对象'
    $keys = @(Get-ObjectsUnderPrefix)
    Assert-True ($keys.Count -gt 0) "前缀下有对象（$($keys.Count) 个）"
    $hasManifest = ($keys | Where-Object { $_ -match 'v1/manifests/[0-9a-f]{32}\.json$' }).Count -ge 1
    Assert-True $hasManifest '设备清单按 v1/manifests/<设备>.json 落位'
    $hasItem = ($keys | Where-Object { $_ -match 'v1/items/' }).Count -ge 1
    Assert-True $hasItem '剪贴板条目按 v1/items/… 落位'
    # 对象名本身若含明文就是泄漏；内容加密由 e2e-server 的"服务端只有密文"断言覆盖
    $leak = $keys | Where-Object { $_ -match 'real-oss' }
    Assert-True ($null -eq $leak) '对象名里没有明文内容'

    Step '停止两台守护进程'
    Assert-True ((Invoke-Device $deviceA @('stop')).Code -eq 0) 'A 已停止'
    Assert-True ((Invoke-Device $deviceB @('stop')).Code -eq 0) 'B 已停止'

    if ($NoCleanup) {
        Skip "保留云端对象（$Prefix 下 $($keys.Count) 个）：$($keys -join ', ')"
    }
    else {
        Step '清空测试前缀'
        Clear-Prefix
    }
}
catch {
    Fail "未预期的异常：$($_.Exception.Message)"
}
finally {
    foreach ($proc in @($procA, $procB)) {
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
    Write-Host '真实 OSS 双设备端到端验收全部通过' -ForegroundColor Green
    exit 0
}
else {
    Write-Host "真实 OSS 端到端验收有 $($failures.Count) 项失败：" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}
