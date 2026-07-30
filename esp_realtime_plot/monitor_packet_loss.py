import argparse
import socket
import time


def main():
    parser = argparse.ArgumentParser(
        description="Real-Time Packet Loss Monitor for ESP32 CSI"
    )
    parser.add_argument("--port", type=int, default=5005, help="UDP port to listen to")
    parser.add_argument(
        "--report-interval", type=int, default=1, help="Report interval in seconds"
    )
    parser.add_argument(
        "--duration",
        type=int,
        default=0,
        help="Run duration in seconds (0 for infinite)",
    )
    args = parser.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    # Try to set reuse address/port so it can run alongside the server or dashboard
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if hasattr(socket, "SO_REUSEPORT"):
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        except Exception:
            pass

    sock.bind(("0.0.0.0", args.port))
    sock.settimeout(0.5)

    print(f"Listening for ESP32 packets on UDP port {args.port}...")
    print(f"Reporting every {args.report_interval} seconds. Press Ctrl+C to stop.\n")

    last_seq = None
    drops = 0
    received = 0
    total_drops = 0
    total_received = 0

    last_report_time = time.time()
    start_time = time.time()

    try:
        while True:
            if args.duration > 0 and time.time() - start_time >= args.duration:
                break

            try:
                data, _ = sock.recvfrom(8192)
                line = data.decode("utf-8", errors="ignore").strip()
                if not line.startswith("CSI_DATA"):
                    continue

                parts = line.split(",")
                if len(parts) > 1:
                    try:
                        seq = int(parts[1])
                        if last_seq is not None:
                            if seq > last_seq + 1:
                                dropped_now = seq - last_seq - 1
                                drops += dropped_now
                                total_drops += dropped_now

                        last_seq = seq
                        received += 1
                        total_received += 1
                    except ValueError:
                        pass

            except TimeoutError:
                pass

            now = time.time()
            if now - last_report_time >= args.report_interval:
                if received > 0 or drops > 0:
                    loss_rate = (
                        (drops / (received + drops)) * 100
                        if (received + drops) > 0
                        else 0.0
                    )
                    print(
                        f"[{time.strftime('%H:%M:%S')}] Rate: {received} Hz | Drops: {drops} ({loss_rate:.2f}%) | Total Rx: {total_received} | Total Drops: {total_drops}"
                    )
                else:
                    print(f"[{time.strftime('%H:%M:%S')}] Waiting for data...")

                received = 0
                drops = 0
                last_report_time = now

    except KeyboardInterrupt:
        print("\nMonitoring stopped.")
        if total_received > 0 or total_drops > 0:
            total_loss_rate = (
                (total_drops / (total_received + total_drops)) * 100
                if (total_received + total_drops) > 0
                else 0.0
            )
            print(
                f"Final Stats -> Total Rx: {total_received} | Total Drops: {total_drops} | Overall Loss Rate: {total_loss_rate:.2f}%"
            )


if __name__ == "__main__":
    main()
