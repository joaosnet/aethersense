# Relay UDP broadcast -> unicast para o backend AetherSense em Docker (Windows).
#
# O Docker Desktop no Windows nao entrega UDP broadcast ao container; este
# relay nativo escuta o broadcast dos ESP32 em :5005 e reinjeta cada datagrama
# como unicast em 127.0.0.1:5006 (mapeado pelo docker-compose para o backend).
#
# PowerShell puro — sem dependencia de Python — para rodar em qualquer Windows.
# Instale o auto-start com scripts/install-relay-autostart.ps1.
#
# Em Linux/Home Assistant use docker-compose.linux.yml (network_mode: host):
# o broadcast chega direto ao container e este relay e desnecessario.

param(
    [int]$ListenPort = 5005,
    [string]$DestHost = "127.0.0.1",
    [int]$DestPort = 5006,
    [int]$TestTapPort = 5007,
    [string]$HealthPath = (Join-Path $env:LOCALAPPDATA "AetherSense\udp-relay-health.json"),
    [int]$HealthIntervalSeconds = 30
)

$ErrorActionPreference = "Stop"

# Tarefas Agendadas nascem com prioridade BelowNormal por padrao. Em uma build
# paralela isso era suficiente para deixar o loop sem CPU por dezenas de
# segundos e estourar o buffer UDP. O relay e leve e deve rodar em prioridade
# normal, sem elevar todo o restante do ambiente.
try {
    [Diagnostics.Process]::GetCurrentProcess().PriorityClass =
        [Diagnostics.ProcessPriorityClass]::Normal
} catch {
    Write-Warning "[relay] nao foi possivel ajustar a prioridade para Normal: $($_.Exception.Message)"
}

$rx = New-Object System.Net.Sockets.UdpClient
# A porta de entrada pertence exclusivamente a este relay. ReuseAddress fazia
# cargo tests e um segundo relay receberem uma parcela indeterminada dos
# broadcasts no Windows.
$rx.ExclusiveAddressUse = $true
$rx.Client.SetSocketOption(
    [Net.Sockets.SocketOptionLevel]::Socket,
    [Net.Sockets.SocketOptionName]::ReuseAddress,
    $false
)
$rx.Client.ReceiveBufferSize = 4MB
$rx.Client.Bind([Net.IPEndPoint]::new([Net.IPAddress]::Any, $ListenPort))

# Backend e tap usam sockets diferentes. Assim um ICMP/erro no tap sem
# consumidor nunca contamina o caminho principal do backend.
$backendTx = New-Object System.Net.Sockets.UdpClient
$backendTx.Client.SendBufferSize = 1MB
$tapTx = if ($TestTapPort -gt 0) {
    $socket = New-Object System.Net.Sockets.UdpClient
    $socket.Client.SendBufferSize = 1MB
    $socket
} else {
    $null
}
$destination = [Net.IPEndPoint]::new([Net.IPAddress]::Parse($DestHost), $DestPort)
$testTap = if ($TestTapPort -gt 0) {
    [Net.IPEndPoint]::new([Net.IPAddress]::Parse($DestHost), $TestTapPort)
} else {
    $null
}

$healthDirectory = Split-Path -Parent $HealthPath
if ($healthDirectory) {
    New-Item -ItemType Directory -Force -Path $healthDirectory | Out-Null
}

Write-Host "[relay] broadcast 0.0.0.0:$ListenPort -> ${DestHost}:$DestPort | tap de testes :$TestTapPort"
$receivedDatagrams = 0
$backendForwarded = 0
$backendSendErrors = 0
$tapForwarded = 0
$tapSendErrors = 0
$receiveErrors = 0
$healthWriteErrors = 0
$lastPacketAt = $null
$lastErrorAt = $null
$lastErrorStage = $null
$lastErrorMessage = $null
$lastHealthWrite = [DateTime]::UtcNow.AddSeconds(-[Math]::Max(1, $HealthIntervalSeconds))
$remote = [Net.IPEndPoint]::new([Net.IPAddress]::Any, 0)
while ($true) {
    try {
        $data = $rx.Receive([ref]$remote)
    } catch {
        # Somente erro de RECEPCAO recebe backoff. Erros de backend/tap abaixo
        # nunca pausam a drenagem do socket de entrada.
        $receiveErrors++
        $lastErrorAt = [DateTime]::UtcNow
        $lastErrorStage = "receive"
        $lastErrorMessage = $_.Exception.Message
        Start-Sleep -Milliseconds 50
        continue
    }

    $receivedDatagrams++
    $lastPacketAt = [DateTime]::UtcNow

    try {
        [void]$backendTx.Send($data, $data.Length, $destination)
        $backendForwarded++
    } catch {
        $backendSendErrors++
        $lastErrorAt = [DateTime]::UtcNow
        $lastErrorStage = "backend_send"
        $lastErrorMessage = $_.Exception.Message
    }

    if ($null -ne $testTap) {
        try {
            [void]$tapTx.Send($data, $data.Length, $testTap)
            $tapForwarded++
        } catch {
            $tapSendErrors++
            $lastErrorAt = [DateTime]::UtcNow
            $lastErrorStage = "tap_send"
            $lastErrorMessage = $_.Exception.Message
        }
    }

    # Persistencia agregada e amortizada: uma escrita curta a cada 30 s por
    # padrao, fora dos catches de rede e sem armazenar payload CSI.
    if (($lastPacketAt - $lastHealthWrite).TotalSeconds -ge [Math]::Max(1, $HealthIntervalSeconds)) {
        try {
            $health = [ordered]@{
                schema = "aethersense.udp_relay_health.v2"
                process_id = $PID
                priority_class = [Diagnostics.Process]::GetCurrentProcess().PriorityClass.ToString()
                listen_port = $ListenPort
                backend_port = $DestPort
                test_tap_port = $TestTapPort
                receive_buffer_bytes = $rx.Client.ReceiveBufferSize
                received_datagrams = $receivedDatagrams
                backend_forwarded_datagrams = $backendForwarded
                backend_send_errors = $backendSendErrors
                tap_forwarded_datagrams = $tapForwarded
                tap_send_errors = $tapSendErrors
                receive_errors = $receiveErrors
                health_write_errors = $healthWriteErrors
                last_packet_at_utc = $lastPacketAt.ToString("o")
                last_source_ip = $remote.Address.ToString()
                last_error_at_utc = if ($null -ne $lastErrorAt) { $lastErrorAt.ToString("o") } else { $null }
                last_error_stage = $lastErrorStage
                last_error_message = $lastErrorMessage
                updated_at_utc = [DateTime]::UtcNow.ToString("o")
            }
            $temporaryHealthPath = "$HealthPath.tmp"
            $healthJson = $health | ConvertTo-Json -Compress
            [IO.File]::WriteAllText(
                $temporaryHealthPath,
                $healthJson,
                [Text.UTF8Encoding]::new($false)
            )
            Move-Item -LiteralPath $temporaryHealthPath -Destination $HealthPath -Force
        } catch {
            $healthWriteErrors++
            $lastErrorAt = [DateTime]::UtcNow
            $lastErrorStage = "health_write"
            $lastErrorMessage = $_.Exception.Message
            Remove-Item -LiteralPath "$HealthPath.tmp" -Force -ErrorAction SilentlyContinue
        } finally {
            $lastHealthWrite = $lastPacketAt
        }
    }

    if ($receivedDatagrams % 10000 -eq 0) {
        Write-Host (
            "[relay] recebidos=$receivedDatagrams backend=$backendForwarded " +
            "tap=$tapForwarded erros_rx=$receiveErrors erros_backend=$backendSendErrors erros_tap=$tapSendErrors"
        )
    }
}
