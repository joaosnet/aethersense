#!/usr/bin/env bash
# setup_thunder.sh — prepara uma instância Thunder Compute (ou qualquer Linux
# com GPU NVIDIA) para o pipeline ML do AetherSense.
#
# Uso (na raiz do repositório clonado):
#   bash setup_thunder.sh
#
# O script é idempotente: pode ser re-executado após falhas.

set -euo pipefail

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m ok \033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merro\033[0m %s\n' "$*" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ML_DIR="$REPO_ROOT/ml"
[ -d "$ML_DIR" ] || die "ml/ não encontrado — rode na raiz do repositório aethersense."

# ── 1. Pré-requisitos básicos ────────────────────────────────────────────────
say "Verificando pré-requisitos"
for cmd in git curl; do
    command -v "$cmd" >/dev/null || die "'$cmd' não instalado (apt install $cmd)"
done
ok "git e curl presentes"

if command -v nvidia-smi >/dev/null; then
    ok "GPU: $(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader | head -1)"
else
    warn "nvidia-smi não encontrado — o treino vai rodar em CPU (lento)."
fi

# ── 2. uv ─────────────────────────────────────────────────────────────────────
if command -v uv >/dev/null; then
    ok "uv já instalado: $(uv --version)"
else
    say "Instalando uv"
    curl -LsSf https://astral.sh/uv/install.sh | sh
    export PATH="$HOME/.local/bin:$PATH"
    command -v uv >/dev/null || die "uv não entrou no PATH — abra um novo shell e re-execute."
    ok "uv instalado: $(uv --version)"
fi

# ── 3. Dependências Python ───────────────────────────────────────────────────
say "Instalando dependências (uv sync)"
cd "$ML_DIR"
uv sync
ok "dependências sincronizadas"

# ── 4. PyTorch com CUDA ──────────────────────────────────────────────────────
if command -v nvidia-smi >/dev/null; then
    CUDA_INDEX="${TORCH_CUDA_INDEX:-cu124}"
    say "Instalando PyTorch CUDA ($CUDA_INDEX — defina TORCH_CUDA_INDEX para mudar)"
    uv pip install torch --index-url "https://download.pytorch.org/whl/$CUDA_INDEX"
    say "Sanity check CUDA"
    if uv run python -c "import torch; assert torch.cuda.is_available()"; then
        ok "torch.cuda.is_available() == True"
    else
        warn "CUDA indisponível no torch — confira o driver (nvidia-smi) e o índice $CUDA_INDEX."
    fi
fi

# ── 5. Próximos passos ───────────────────────────────────────────────────────
cat <<'EOF'

Pronto! Próximos passos (em ml/):

  # Smoke test ponta a ponta (amostras pequenas, ~5 min):
  uv run main.py            # opção [1] Smoke Test
  # ou, sem TUI:
  uv run pipeline.py --fast-dev

  # Pipeline completo em background (sobrevive a SSH drop):
  nohup uv run pipeline.py > pipeline.out 2>&1 &
  tail -f pipeline.out

Artefatos em ml/artifacts/runs/<run_id>/ (report.html, *.onnx, benchmark.json).
EOF
