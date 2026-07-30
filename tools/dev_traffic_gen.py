"""Gerador de tráfego Wi-Fi para estabilizar a taxa de CSI dos ESP32.

O CSI é PASSIVO: o ESP32 só produz um frame CSI quando ouve um frame Wi-Fi
no canal. Em rede ociosa a taxa cai e oscila (~3-6 Hz). Este gerador envia
pequenos datagramas UDP ao roteador num ritmo constante — cada envio gera
frames PC→AP (e ACKs do AP) que os ESP32 ouvem, estabilizando o stream perto
do teto de pacing do firmware (AETHERSENSE_SEND_FREQUENCY_HZ, 10 Hz).

IMPORTANTE (dual band): se o PC estiver no 5 GHz e os ESP32 em 2,4 GHz,
tráfego PC→roteador NÃO passa pelo rádio que os ESPs escutam. Mire nos
PRÓPRIOS ESP32s (TRAFFIC_TARGET_HOST com os IPs deles): o AP transmite o
frame até cada ESP em 2,4 GHz — e esse frame vira CSI.

Uso:
    python tools/dev_traffic_gen.py
Variáveis de ambiente (opcionais):
    TRAFFIC_TARGET_HOST (um ou mais IPs separados por vírgula;
                         default: gateway IPv4 detectado)
    TRAFFIC_TARGET_PORT (default 9 — porta discard; o conteúdo é irrelevante,
                         o que importa é o frame no ar)
    TRAFFIC_RATE_HZ     (default 25, por alvo)
"""
import os
import socket
import time


def default_gateway() -> str:
    # Truque padrão: conectar um socket UDP "para fora" revela o IP/rota local;
    # o gateway costuma ser .1 na mesma subnet. Sem dependências externas.
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("8.8.8.8", 80))
        local_ip = probe.getsockname()[0]
    finally:
        probe.close()
    return ".".join(local_ip.split(".")[:3]) + ".1"


HOSTS = [
    host.strip()
    for host in (os.environ.get("TRAFFIC_TARGET_HOST") or default_gateway()).split(",")
    if host.strip()
]
PORT = int(os.environ.get("TRAFFIC_TARGET_PORT", "9"))
RATE_HZ = float(os.environ.get("TRAFFIC_RATE_HZ", "25"))

interval = 1.0 / max(RATE_HZ, 1.0)
payload = b"aethersense-csi-traffic"
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

print(f"[traffic] {RATE_HZ:.0f} Hz/alvo -> {HOSTS} porta {PORT} (Ctrl+C para parar)")
sent = 0
last_log = time.monotonic()
try:
    while True:
        for host in HOSTS:
            sock.sendto(payload, (host, PORT))
            sent += 1
        now = time.monotonic()
        if now - last_log >= 30.0:
            print(f"[traffic] {sent} datagramas enviados")
            last_log = now
        time.sleep(interval)
except KeyboardInterrupt:
    print(f"\n[traffic] encerrado. Total: {sent}")
finally:
    sock.close()
