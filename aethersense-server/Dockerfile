FROM rust:1.94.1-slim-trixie AS builder
WORKDIR /app

# Truque para cache do Cargo: Compila apenas as dependências primeiro
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src

# Copia e compila o código real
COPY src ./src
COPY tests ./tests
RUN touch src/main.rs && cargo build --release

# Imagem final de execução
FROM debian:trixie-slim
WORKDIR /app

RUN apt-get update && apt-get install -y ca-certificates curl jq && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/aethersense-server /usr/local/bin/
# Modelos ONNX (occupancy + vitals) — carregados em runtime de /app/models
COPY models ./models
COPY run.sh /run.sh
RUN chmod +x /run.sh

EXPOSE 8000
EXPOSE 5005/udp

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 CMD curl -fsS --max-time 2 http://127.0.0.1:8000/api/health

# O entrypoint lê opções do Home Assistant quando presente e então inicia o servidor
CMD ["/run.sh"]
