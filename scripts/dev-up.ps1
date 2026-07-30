# Sobe o ambiente de DEV (Docker) junto com o relay UDP de broadcast.
#
# O ESP32 envia o CSI em broadcast; o Docker Desktop no Windows não entrega
# broadcast ao container, então este script inicia um relay nativo que
# reinjeta os pacotes no backend em Docker. Em Home Assistant/Linux o relay
# não é necessário (o add-on usa host_network).
#
# Uso:  .\scripts\dev-up.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$relayScript = [IO.Path]::GetFullPath((Join-Path $root "scripts\udp-relay.ps1"))
$pythonRelay = [IO.Path]::GetFullPath((Join-Path $root "tools\dev_udp_relay.py"))

function Get-Udp5005Owners {
    @(Get-NetUDPEndpoint -LocalPort 5005 -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty OwningProcess -Unique)
}

function Test-IsProjectRelayProcess([int]$ProcessId) {
    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if (-not $process -or -not $process.CommandLine) {
        return $false
    }

    $commandLine = [string]$process.CommandLine
    return (
        $commandLine.IndexOf($relayScript, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
        $commandLine.IndexOf($pythonRelay, [StringComparison]::OrdinalIgnoreCase) -ge 0
    )
}

Write-Host "[dev-up] Iniciando relay UDP (broadcast :5005 -> 127.0.0.1:5006)..." -ForegroundColor Cyan
$owners = @(Get-Udp5005Owners)
$relay = $null
if ($owners.Count -gt 1) {
    throw "Mais de um processo escuta UDP :5005 (PIDs: $($owners -join ', ')). Corrija a duplicidade antes de subir o DEV."
} elseif ($owners.Count -eq 1) {
    $owner = [int]$owners[0]
    if (-not (Test-IsProjectRelayProcess $owner)) {
        throw "UDP :5005 já pertence ao PID $owner, que não é o relay deste projeto. O DEV não reutilizará um listener desconhecido."
    }
    Write-Host "[dev-up] Relay AetherSense existente em :5005 (PID $owner); reutilizando." -ForegroundColor Green
} else {
    # Use o relay PowerShell canônico. O PID retornado é também o proprietário
    # real do socket, o que permite encerramento e validação sem deixar um
    # processo Python órfão atrás do wrapper `uv run`.
    $relay = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @(
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle", "Hidden",
            "-ExecutionPolicy", "Bypass",
            "-File", "`"$relayScript`""
        ) `
        -PassThru -WindowStyle Hidden

    $boundOwner = $null
    foreach ($attempt in 1..25) {
        Start-Sleep -Milliseconds 200
        if ($relay.HasExited) {
            throw "O relay AetherSense encerrou antes de assumir UDP :5005 (exit code $($relay.ExitCode))."
        }
        $currentOwners = @(Get-Udp5005Owners)
        if ($currentOwners.Count -gt 0) {
            $boundOwner = [int]$currentOwners[0]
            break
        }
    }

    if ($boundOwner -ne $relay.Id -or -not (Test-IsProjectRelayProcess $boundOwner)) {
        Stop-Process -Id $relay.Id -ErrorAction SilentlyContinue
        throw "O relay iniciado (PID $($relay.Id)) não assumiu UDP :5005 de forma exclusiva. Proprietário observado: $boundOwner."
    }
    Write-Host "[dev-up] Relay AetherSense rodando e validado (PID $($relay.Id))." -ForegroundColor Green
}

try {
    docker compose -f "$root\docker-compose.dev.yml" up --build
}
finally {
    if ($relay) {
        Write-Host "`n[dev-up] Parando relay UDP (PID $($relay.Id))..." -ForegroundColor Cyan
        Stop-Process -Id $relay.Id -ErrorAction SilentlyContinue
    }
}
