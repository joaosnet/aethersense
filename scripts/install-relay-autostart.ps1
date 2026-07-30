# Instala o relay UDP do AetherSense como Tarefa Agendada do usuário atual:
# inicia no logon, roda oculto e reinicia sozinho em caso de falha. O relay
# também duplica os datagramas para o tap de testes :5007; nunca é necessário
# abrir uma segunda escuta em :5005.
# Execução única por máquina Windows (não precisa de administrador):
#     powershell -ExecutionPolicy Bypass -File scripts\install-relay-autostart.ps1
#
# Em Linux/Home Assistant isto é DESNECESSÁRIO — use docker-compose.linux.yml
# (network_mode: host) e o broadcast chega direto ao container.

$ErrorActionPreference = "Stop"
$taskName = "AetherSense UDP Relay"
$relayScript = Join-Path (Split-Path $PSScriptRoot -Parent) "scripts\udp-relay.ps1"

if (-not (Test-Path $relayScript)) {
    throw "Relay não encontrado em $relayScript"
}

$action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$relayScript`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero)
# O Task Scheduler usa prioridade 7 (BelowNormal) quando este campo fica
# implícito. Builds Rust/Node já demonstraram starvation do relay nessa classe.
# O valor 4 corresponde a Normal; o próprio script também reafirma a classe.
$settings.Priority = 4

$existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($existingTask -and $existingTask.State -eq "Running") {
    Stop-ScheduledTask -TaskName $taskName
    $deadline = (Get-Date).AddSeconds(15)
    do {
        Start-Sleep -Milliseconds 250
        $existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    } while ($existingTask -and $existingTask.State -eq "Running" -and (Get-Date) -lt $deadline)

    if ($existingTask -and $existingTask.State -eq "Running") {
        throw "A tarefa '$taskName' não encerrou a instância anterior dentro de 15 s."
    }
}

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Settings $settings -Force | Out-Null

Start-ScheduledTask -TaskName $taskName
Write-Host "Tarefa '$taskName' instalada (inicia no logon, reinicia em falha) e iniciada agora."
