param(
    [string]$AgentDesktop = "C:\agentdesktop\agentdesktop.exe",
    [string]$AgentGateway = "C:\agentdesktop\agentgateway.exe",
    [string]$GatewayConfig = "C:\agentdesktop\agentgateway-native.yaml"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

function Test-Listener([int]$Port) {
    try {
        $client = [Net.Sockets.TcpClient]::new()
        $client.Connect("127.0.0.1", $Port)
        $client.Dispose()
        return $true
    } catch {
        return $false
    }
}

function Wait-For([scriptblock]$Condition, [int]$Seconds, [string]$Failure) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    do {
        if (& $Condition) {
            return
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw $Failure
}

function Convert-ContentToText($Content) {
    if ($Content -is [byte[]]) {
        return [Text.Encoding]::UTF8.GetString($Content).Trim()
    }
    return ([string]$Content).Trim()
}

foreach ($path in $AgentDesktop, $AgentGateway, $GatewayConfig) {
    if (-not (Test-Path $path -PathType Leaf)) {
        throw "required smoke-test input not found: $path"
    }
}

$work = Split-Path $AgentDesktop
$stdout = Join-Path $work "native-smoke.stdout.log"
$stderr = Join-Path $work "native-smoke.stderr.log"
Remove-Item $stdout, $stderr -Force -ErrorAction SilentlyContinue

$arguments = @(
    "serve",
    "--mode", "standalone",
    "--listen", "127.0.0.1:8080",
    "--status-listen", "127.0.0.1:8081",
    "--upstream", "http://127.0.0.1:15008",
    "--native-target", "native.agentdesktop.internal:4000",
    "--gateway-binary", $AgentGateway,
    "--gateway-config", $GatewayConfig
)

$connector = $null
try {
    $connector = Start-Process $AgentDesktop -ArgumentList $arguments -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr

    Wait-For {
        (Test-Listener 8080) -and (Test-Listener 8081) -and (Test-Listener 15008)
    } 15 "native stack did not become ready"

    $response = Invoke-WebRequest -UseBasicParsing -Method Post `
        -Uri "http://127.0.0.1:8080/v1/messages" -Body "opaque-request"
    $body = Convert-ContentToText $response.Content
    if ($response.StatusCode -ne 200 -or $body -ne "WINDOWS_NATIVE_OK") {
        throw "unexpected native response: status=$($response.StatusCode) body=$body"
    }

    $health = Invoke-WebRequest -UseBasicParsing `
        -Uri "http://127.0.0.1:8081/_agentdesktop/healthz"
    if ($health.StatusCode -ne 200) {
        throw "unexpected health status: $($health.StatusCode)"
    }

    Get-Process agentgateway -ErrorAction Stop | Stop-Process -Force
    if (-not $connector.WaitForExit(10000)) {
        throw "connector remained running after its Agent Gateway exited"
    }
    if ((Test-Listener 8080) -or (Test-Listener 8081) -or (Test-Listener 15008)) {
        throw "a native stack listener remained open after Agent Gateway exit"
    }

    Write-Output "native_status=200 native_body=$body"
    Write-Output "health_status=200"
    Write-Output "gateway_exit_closed_all_listeners=true"
} catch {
    Get-Content $stdout, $stderr -ErrorAction SilentlyContinue
    throw
} finally {
    Get-Process agentdesktop, agentgateway -ErrorAction SilentlyContinue | Stop-Process -Force
}