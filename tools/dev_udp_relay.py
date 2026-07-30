"""Relay UDP broadcast -> unicast para o ambiente de DEV em Docker (Windows).

O Docker Desktop no Windows NÃO entrega pacotes UDP broadcast ao container
(o proxy de porta só repassa unicast). O ESP32, por outro lado, envia o CSI em
broadcast (para ser portátil entre redes/máquinas, sem IP fixo).

Este relay roda NATIVO no host Windows: escuta o broadcast do ESP32 em
0.0.0.0:<LISTEN_PORT> (o host recebe o broadcast normalmente) e reinjeta cada
datagrama como unicast em <DEST_HOST>:<DEST_PORT>, que o docker-compose.dev
mapeia para a porta UDP do container. Assim o backend em Docker recebe o CSI.

Em produção (Home Assistant / Linux com host_network) este relay NÃO é
necessário: o broadcast chega direto ao add-on.

Uso:
    uv run tools/dev_udp_relay.py
Variáveis de ambiente (opcionais):
    RELAY_LISTEN_PORT (default 5005)  -> porta do broadcast do ESP32
    RELAY_DEST_HOST   (default 127.0.0.1)
    RELAY_DEST_PORT   (default 5006)  -> porta host mapeada para o container
    RELAY_TEST_TAP_PORT (default 5007) -> copia somente para testes ao vivo
"""
import os
import json
import socket
import time
from pathlib import Path

LISTEN_PORT = int(os.environ.get("RELAY_LISTEN_PORT", "5005"))
DEST_HOST = os.environ.get("RELAY_DEST_HOST", "127.0.0.1")
DEST_PORT = int(os.environ.get("RELAY_DEST_PORT", "5006"))
TEST_TAP_PORT = int(os.environ.get("RELAY_TEST_TAP_PORT", "5007"))
HEALTH_INTERVAL_SECONDS = max(
    1.0, float(os.environ.get("RELAY_HEALTH_INTERVAL_SECONDS", "30"))
)
HEALTH_PATH = Path(
    os.environ.get(
        "RELAY_HEALTH_PATH",
        str(
            Path(os.environ.get("LOCALAPPDATA", os.environ.get("TEMP", ".")))
            / "AetherSense"
            / "udp-relay-health.json"
        ),
    )
)


def set_normal_priority() -> str:
    """Undo Task Scheduler's BelowNormal default on Windows."""
    if os.name != "nt":
        return "os_default"
    try:
        import ctypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.GetCurrentProcess.restype = ctypes.c_void_p
        kernel32.SetPriorityClass.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        kernel32.SetPriorityClass.restype = ctypes.c_int
        normal_priority_class = 0x00000020
        if not kernel32.SetPriorityClass(
            kernel32.GetCurrentProcess(), normal_priority_class
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return "Normal"
    except OSError as exc:
        print(f"[relay] aviso: prioridade Normal nao aplicada: {exc}", flush=True)
        return "unknown"


PRIORITY_CLASS = set_normal_priority()

rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
# O listener :5005 deve ter um unico dono. SO_REUSEADDR no Windows torna
# indeterminada a entrega quando cargo/dev-up abre outro socket na mesma porta.
if os.name == "nt":
    rx.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
rx.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
rx.bind(("0.0.0.0", LISTEN_PORT))
backend_tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
backend_tx.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1024 * 1024)
tap_tx = None
if TEST_TAP_PORT > 0:
    # Socket separado: um erro/ICMP no tap sem consumidor nao pode afetar o
    # encaminhamento principal ao backend.
    tap_tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tap_tx.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1024 * 1024)

HEALTH_PATH.parent.mkdir(parents=True, exist_ok=True)

print(
    f"[relay] broadcast 0.0.0.0:{LISTEN_PORT} -> {DEST_HOST}:{DEST_PORT}"
    f" | tap de testes :{TEST_TAP_PORT}"
)
print("[relay] Ctrl+C para parar.")

received = 0
backend_forwarded = 0
backend_send_errors = 0
tap_forwarded = 0
tap_send_errors = 0
receive_errors = 0
health_write_errors = 0
sources: set[str] = set()
last_log = time.monotonic()
last_health = last_log - HEALTH_INTERVAL_SECONDS
last_packet_at: float | None = None
last_error_at: float | None = None
last_error_stage: str | None = None
last_error_message: str | None = None
try:
    while True:
        try:
            data, addr = rx.recvfrom(8192)
        except OSError as exc:
            receive_errors += 1
            last_error_at = time.time()
            last_error_stage = "receive"
            last_error_message = str(exc)
            # Backoff existe somente para erro de recepcao persistente.
            time.sleep(0.05)
            continue

        received += 1
        last_packet_at = time.time()
        sources.add(addr[0])

        try:
            backend_tx.sendto(data, (DEST_HOST, DEST_PORT))
            backend_forwarded += 1
        except OSError as exc:
            backend_send_errors += 1
            last_error_at = time.time()
            last_error_stage = "backend_send"
            last_error_message = str(exc)

        if tap_tx is not None:
            try:
                tap_tx.sendto(data, (DEST_HOST, TEST_TAP_PORT))
                tap_forwarded += 1
            except OSError as exc:
                tap_send_errors += 1
                last_error_at = time.time()
                last_error_stage = "tap_send"
                last_error_message = str(exc)

        now = time.monotonic()
        if now - last_health >= HEALTH_INTERVAL_SECONDS:
            # Apenas contadores agregados; nenhum payload CSI e gravado.
            health = {
                "schema": "aethersense.udp_relay_health.v2",
                "process_id": os.getpid(),
                "priority_class": PRIORITY_CLASS,
                "listen_port": LISTEN_PORT,
                "backend_port": DEST_PORT,
                "test_tap_port": TEST_TAP_PORT,
                "receive_buffer_bytes": rx.getsockopt(
                    socket.SOL_SOCKET, socket.SO_RCVBUF
                ),
                "received_datagrams": received,
                "backend_forwarded_datagrams": backend_forwarded,
                "backend_send_errors": backend_send_errors,
                "tap_forwarded_datagrams": tap_forwarded,
                "tap_send_errors": tap_send_errors,
                "receive_errors": receive_errors,
                "health_write_errors": health_write_errors,
                "last_packet_at_unix_s": last_packet_at,
                "last_source_ip": addr[0],
                "last_error_at_unix_s": last_error_at,
                "last_error_stage": last_error_stage,
                "last_error_message": last_error_message,
                "updated_at_unix_s": time.time(),
            }
            temporary_path = HEALTH_PATH.with_suffix(HEALTH_PATH.suffix + ".tmp")
            try:
                temporary_path.write_text(
                    json.dumps(health, ensure_ascii=True, separators=(",", ":")),
                    encoding="utf-8",
                )
                temporary_path.replace(HEALTH_PATH)
            except OSError as exc:
                health_write_errors += 1
                last_error_at = time.time()
                last_error_stage = "health_write"
                last_error_message = str(exc)
                try:
                    temporary_path.unlink(missing_ok=True)
                except OSError:
                    pass
            last_health = now

        if now - last_log >= 30.0:
            print(
                "[relay] "
                f"recebidos={received} backend={backend_forwarded} tap={tap_forwarded} "
                f"erros_rx={receive_errors} erros_backend={backend_send_errors} "
                f"erros_tap={tap_send_errors} origens={sorted(sources)}",
                flush=True,
            )
            last_log = now
except KeyboardInterrupt:
    print(f"\n[relay] encerrado. Total recebido: {received}")
finally:
    rx.close()
    backend_tx.close()
    if tap_tx is not None:
        tap_tx.close()
