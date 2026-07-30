"""
download_datasets.py — baixa os datasets públicos do AetherSense.

Nenhuma credencial é necessária: o Zenodo serve os zips pela API pública, o
Kaggle serve arquivos individuais do WiMANS pelo endpoint REST anônimo
(barras do caminho codificadas como %2F; binários chegam como zip individual),
o GitHub serve os CSVs do CSI-MiningTool via raw URLs e o NIST serve o zip do
BreatheSmart com suporte a HTTP Range (no smoke, membros individuais são
extraídos do zip remoto sem baixar os 202 MB).

Datasets:
  - Zenodo 8021099  → <data-dir>/zenodo_8021099/{DA_LOS,DA_NLOS,DP_LOS,DP_NLOS}/
  - WiMANS (Kaggle shuokanghuang/wimans)
                    → <data-dir>/WiMANS/annotation.csv + wifi_csi/amp/act_N_M.npy
  - WiFi-CSI-MiningTool (GitHub, GPL-3.0)
                    → <data-dir>/WiFi-CSI-MiningTool/Datasets/Subjects/…
  - NIST BreatheSmart (mds2-2963)
                    → <data-dir>/NIST-BreatheSmart/…/config*/…csv
  - MM-Fi (Google Drive, CC BY-NC 4.0) — anotações sempre; o CSI WiFi só vem
    em zips de ~18-21 GB por ambiente → exige --include-mmfi (modo completo)
  - Wi-Pose (opcional --include-wipose, zip único via gdown)
                    → <data-dir>/Wi-Pose/

Modos:
    # Completo (Thunder Compute): WiMANS amp inteiro (~19 GB) + Zenodo +
    # CSI-MiningTool (~459 MB) + NIST (~202 MB)
    uv run download_datasets.py

    # Smoke test: amostra pequena de cada dataset (~70 MB no total)
    uv run download_datasets.py --smoke

    # Retomar um download completo interrompido: basta rodar de novo —
    # arquivos existentes são pulados.

No modo --smoke o annotation.csv do WiMANS contém SOMENTE as amostras
baixadas (o train.py indexa o CSV linha a linha; um CSV completo com .npy
faltando quebraria o treino). O modo completo sobrescreve com o integral.
"""

from __future__ import annotations

import argparse
import csv
import io
import shutil
import sys
import tarfile
import tempfile
import time
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from urllib.parse import quote

import requests
from rich.console import Console
from rich.progress import (
    BarColumn,
    MofNCompleteColumn,
    Progress,
    TextColumn,
    TimeElapsedColumn,
)

_REPO_ROOT = Path(__file__).resolve().parent.parent

# ── Zenodo 8021099 — presença binária (PNGs 64×64 + CSVs de labels) ──────────
ZENODO_ZONES = ("DA_LOS", "DA_NLOS", "DP_LOS", "DP_NLOS")
ZENODO_SMOKE_ZONES = ("DA_LOS",)
ZENODO_URL = "https://zenodo.org/api/records/8021099/files/{zone}.zip/content"

# ── WiMANS — Kaggle oficial do autor (huangshk/WiMANS → shuokanghuang/wimans)
KAGGLE_DATASET = "shuokanghuang/wimans"
KAGGLE_FILE_URL = (
    "https://www.kaggle.com/api/v1/datasets/download/"
    + KAGGLE_DATASET + "/{path}"
)

# ── Wi-Pose — zip único no Google Drive (gancho opcional, desativado) ─────────
WIPOSE_GDRIVE_URL = (
    "https://drive.google.com/file/d/1WL6bJ-rSVdsclRt9RFc0l5hhtXYmfNg9/view?usp=sharing"
)

# ── WiFi-CSI-MiningTool — GitHub (GPL-3.0), CSVs brutos com GT de vitais ─────
MININGTOOL_RAW = (
    "https://raw.githubusercontent.com/AlbanyArmenta0711/WiFi-CSI-MiningTool/main/{path}"
)
MININGTOOL_ZIPBALL = (
    "https://codeload.github.com/AlbanyArmenta0711/WiFi-CSI-MiningTool/legacy.zip/refs/heads/main"
)
# Amostra p/ smoke: 2 taxas de respiração (9 e 21 RPM) + 1 sujeito de batimento
MININGTOOL_SMOKE_FILES = (
    "Datasets/Subjects/Breathing/S1_AMP_PHASE/s1_9BPM.csv",
    "Datasets/Subjects/Breathing/S1_AMP_PHASE/s1_21BPM.csv",
    "Datasets/Subjects/Heart-Rate/S1/S1_RAW.csv",
    "Datasets/Subjects/Heart-Rate/S1/HRReal.CSV",
)

# ── NIST BreatheSmart (mds2-2963) — zip único com 344 tar.gz internos ────────
NIST_ZIP_URL = (
    "https://data.nist.gov/od/ds/ark:/88434/mds2-2963/BreatheSmartv2.zip"
)
NIST_SMOKE_MAX_MEMBERS = 8
NIST_SMOKE_MAX_BYTES = 4 << 20      # ~4 MB de tar.gz no smoke

# ── MM-Fi — Google Drive (CC BY-NC 4.0) ──────────────────────────────────────
MMFI_SEGMENTS_GDRIVE_ID = "1IhzGDxCwglJPzfX8CSLWWdjxI4J9an4z"   # CSV 148 KB
MMFI_E01_GDRIVE_ID = "1ExV3AQeHstQ3Z1VBFOC0z1BDtTD5nZ1B"        # E01.zip ~21 GB

_RETRIES = 3
_TIMEOUT = (15, 120)   # (connect, read) em segundos


# ---------------------------------------------------------------------------
# HTTP com retry
# ---------------------------------------------------------------------------

def _get(url: str, console: Console) -> requests.Response:
    """GET (em memória) com retry exponencial. Levanta a última exceção."""
    last_exc: Exception | None = None
    for attempt in range(_RETRIES):
        try:
            resp = requests.get(url, timeout=_TIMEOUT, allow_redirects=True)
            if resp.status_code in (429, 500, 502, 503, 504):
                raise requests.HTTPError(f"HTTP {resp.status_code}", response=resp)
            resp.raise_for_status()
            return resp
        except requests.RequestException as exc:
            last_exc = exc
            wait = 2 ** attempt
            console.print(
                f"  [yellow]tentativa {attempt + 1}/{_RETRIES} falhou "
                f"({exc}); aguardando {wait}s[/yellow]"
            )
            time.sleep(wait)
    raise last_exc  # type: ignore[misc]


def _download_to(url: str, dest: Path, console: Console) -> None:
    """
    Baixa *url* para *dest* via arquivo .part (sem parciais corrompidos).

    O retry envolve o streaming inteiro: quedas no meio da transferência
    (ChunkedEncodingError/timeout de leitura) também são retentadas — crítico
    para o download completo do WiMANS (11k arquivos) em rede instável.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_suffix(dest.suffix + ".part")
    last_exc: Exception | None = None
    for attempt in range(_RETRIES):
        try:
            resp = requests.get(url, stream=True, timeout=_TIMEOUT,
                                allow_redirects=True)
            if resp.status_code in (429, 500, 502, 503, 504):
                raise requests.HTTPError(f"HTTP {resp.status_code}", response=resp)
            resp.raise_for_status()
            with part.open("wb") as fh:
                for chunk in resp.iter_content(chunk_size=1 << 20):
                    fh.write(chunk)
            part.replace(dest)
            return
        except requests.RequestException as exc:
            last_exc = exc
            wait = 2 ** attempt
            console.print(
                f"  [yellow]tentativa {attempt + 1}/{_RETRIES} falhou "
                f"({exc}); aguardando {wait}s[/yellow]"
            )
            time.sleep(wait)
    raise last_exc  # type: ignore[misc]


def _download_kaggle_file(relpath: str, dest: Path, console: Console) -> None:
    """
    Baixa um arquivo individual do dataset Kaggle (sem credenciais).

    As barras do caminho DEVEM ir URL-encodadas (%2F) — com barras literais o
    endpoint responde 404. Binários chegam embrulhados num zip de um membro;
    annotation.csv chega raw. Detecta pelo magic number e extrai se preciso.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    url = KAGGLE_FILE_URL.format(path=quote(relpath, safe=""))
    tmp = dest.with_suffix(dest.suffix + ".part")
    _download_to(url, tmp, console)

    with tmp.open("rb") as fh:
        magic = fh.read(2)
    if magic == b"PK":
        with zipfile.ZipFile(tmp) as zf:
            member = next(
                (n for n in zf.namelist() if not n.endswith("/")), None
            )
            if member is None:
                raise RuntimeError(f"Zip vazio recebido do Kaggle para {relpath}")
            with zf.open(member) as src, dest.open("wb") as out:
                shutil.copyfileobj(src, out)
        tmp.unlink()
    else:
        tmp.replace(dest)


# ---------------------------------------------------------------------------
# Zenodo 8021099
# ---------------------------------------------------------------------------

def download_zenodo(zenodo_dir: Path, smoke: bool, console: Console) -> bool:
    zones = ZENODO_SMOKE_ZONES if smoke else ZENODO_ZONES
    console.print(f"[bold]Zenodo 8021099[/bold] — zonas: {', '.join(zones)}")

    for zone in zones:
        zone_dir = zenodo_dir / zone
        if any(zone_dir.rglob("*Labels.csv")):
            console.print(f"  {zone}: já presente — pulando")
            continue

        console.print(f"  {zone}: baixando (~3,6 MB)…")
        zip_path = zenodo_dir / f"{zone}.zip"
        _download_to(ZENODO_URL.format(zone=zone), zip_path, console)

        with zipfile.ZipFile(zip_path) as zf:
            names = zf.namelist()
            # Os zips do registro trazem pasta raiz (DA_LOS/…); por segurança,
            # zips flat são extraídos numa subpasta com o nome da zona para os
            # *Labels.csv de zonas diferentes não se sobrescreverem.
            has_root = all(n.startswith(f"{zone}/") for n in names)
            zf.extractall(zenodo_dir if has_root else zone_dir)
        zip_path.unlink()
        console.print(f"  {zone}: extraído em {zone_dir}")
    return True


# ---------------------------------------------------------------------------
# WiMANS
# ---------------------------------------------------------------------------

def _fetch_annotation(console: Console) -> tuple[list[str], list[dict]]:
    """Baixa o annotation.csv completo (raw, ~700 KB) para a memória."""
    console.print("  annotation.csv: baixando do Kaggle…")
    url = KAGGLE_FILE_URL.format(path="annotation.csv")
    resp = _get(url, console)
    text = resp.content.decode("utf-8-sig")
    reader = csv.DictReader(io.StringIO(text))
    rows = list(reader)
    if not rows or "label" not in (reader.fieldnames or []):
        raise RuntimeError(
            "annotation.csv inesperado (sem coluna 'label') — "
            f"colunas: {reader.fieldnames}"
        )
    return list(reader.fieldnames or []), rows


def _stratify(rows: list[dict], n_total: int) -> list[dict]:
    """
    Seleciona ~n_total amostras cobrindo todas as classes de contagem (0..5+),
    em round-robin para o smoke test exercitar contagem e heatmap em todas as
    classes (inclusive as raras, 0 e 5).
    """
    by_class: dict[int, list[dict]] = {}
    for row in rows:
        try:
            c = int(float(row.get("number_of_users") or 0))
        except ValueError:
            c = 0
        by_class.setdefault(c, []).append(row)

    selected: list[dict] = []
    classes = sorted(by_class)
    i = 0
    while len(selected) < n_total and any(by_class[c] for c in classes):
        c = classes[i % len(classes)]
        if by_class[c]:
            selected.append(by_class[c].pop(0))
        i += 1
    return selected


def download_wimans(
    wimans_dir: Path,
    smoke: bool,
    smoke_samples: int,
    workers: int,
    console: Console,
) -> bool:
    console.print(f"[bold]WiMANS[/bold] (Kaggle {KAGGLE_DATASET}, sem credenciais)")
    fieldnames, all_rows = _fetch_annotation(console)

    if smoke:
        rows = _stratify(all_rows, smoke_samples)
        counts = sorted(int(float(r.get("number_of_users") or 0)) for r in rows)
        console.print(
            f"  amostra estratificada: {len(rows)} de {len(all_rows)} amostras "
            f"(contagens: {counts})"
        )
    else:
        rows = all_rows
        console.print(f"  dataset completo: {len(rows)} amostras "
                      f"(~19 GB transferidos; somente wifi_csi/amp)")

    amp_dir = wimans_dir / "wifi_csi" / "amp"
    todo = [
        r["label"] for r in rows
        if r.get("label") and not (amp_dir / f"{r['label']}.npy").exists()
    ]
    done_already = len(rows) - len(todo)
    if done_already:
        console.print(f"  {done_already} .npy já presentes — pulando")

    errors: list[str] = []
    if todo:
        with Progress(
            TextColumn("  [progress.description]{task.description}"),
            BarColumn(bar_width=30),
            MofNCompleteColumn(),
            TimeElapsedColumn(),
            console=console,
            disable=not (sys.stdout.isatty() or sys.stderr.isatty()),
        ) as prog:
            task = prog.add_task("wifi_csi/amp", total=len(todo))
            with ThreadPoolExecutor(max_workers=workers) as pool:
                futures = {
                    pool.submit(
                        _download_kaggle_file,
                        f"wifi_csi/amp/{label}.npy",
                        amp_dir / f"{label}.npy",
                        console,
                    ): label
                    for label in todo
                }
                for fut in as_completed(futures):
                    label = futures[fut]
                    try:
                        fut.result()
                    except Exception as exc:
                        errors.append(f"{label}: {exc}")
                    prog.advance(task)

    if errors:
        console.print(f"  [red]{len(errors)} downloads falharam:[/red]")
        for e in errors[:10]:
            console.print(f"    [red]{e}[/red]")
        if len(errors) > 10:
            console.print(f"    [red]… e mais {len(errors) - 10}[/red]")

    # Grava o annotation.csv coerente com os .npy em disco. No smoke, apenas
    # as amostras baixadas com sucesso; no completo, o CSV integral.
    failed_labels = {e.split(":", 1)[0] for e in errors}
    keep = [r for r in rows if r.get("label") not in failed_labels]
    ann_path = wimans_dir / "annotation.csv"
    wimans_dir.mkdir(parents=True, exist_ok=True)
    with ann_path.open("w", newline="", encoding="utf-8") as fh:
        writer = csv.DictWriter(fh, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(keep)

    if smoke:
        (wimans_dir / "SMOKE_SAMPLE.txt").write_text(
            "Amostra parcial do WiMANS para smoke test "
            f"({len(keep)} de {len(all_rows)} amostras). "
            "Para o dataset completo: uv run download_datasets.py\n",
            encoding="utf-8",
        )
    else:
        (wimans_dir / "SMOKE_SAMPLE.txt").unlink(missing_ok=True)

    console.print(f"  annotation.csv gravado com {len(keep)} amostras")
    return not errors


# ---------------------------------------------------------------------------
# WiFi-CSI-MiningTool
# ---------------------------------------------------------------------------

def download_miningtool(mt_dir: Path, smoke: bool, console: Console) -> bool:
    console.print("[bold]WiFi-CSI-MiningTool[/bold] (GitHub, GPL-3.0)")

    if smoke:
        ok = True
        for rel in MININGTOOL_SMOKE_FILES:
            dest = mt_dir / rel
            if dest.exists():
                console.print(f"  {dest.name}: já presente — pulando")
                continue
            console.print(f"  {rel.split('/')[-1]}: baixando…")
            try:
                _download_to(MININGTOOL_RAW.format(path=quote(rel)), dest, console)
            except Exception as exc:
                console.print(f"  [red]{rel}: {exc}[/red]")
                ok = False
        return ok

    # O marcador distingue dataset completo de restos de um --smoke anterior
    # (que também criam Datasets/). Sem ele, o modo completo re-baixa tudo.
    if (mt_dir / ".full_download").exists():
        console.print("  já presente (completo) — pulando")
        return True
    console.print("  zipball completo (~459 MB)…")
    with tempfile.TemporaryDirectory() as tmp:
        zip_path = Path(tmp) / "miningtool.zip"
        _download_to(MININGTOOL_ZIPBALL, zip_path, console)
        with zipfile.ZipFile(zip_path) as zf:
            # O zipball tem pasta raiz <owner>-<repo>-<sha>/ — extrai apenas
            # Datasets/** removendo esse prefixo.
            for member in zf.namelist():
                parts = member.split("/", 1)
                if len(parts) < 2 or not parts[1].startswith("Datasets/"):
                    continue
                if member.endswith("/"):
                    continue
                dest = mt_dir / parts[1]
                dest.parent.mkdir(parents=True, exist_ok=True)
                with zf.open(member) as src, dest.open("wb") as out:
                    shutil.copyfileobj(src, out)
    (mt_dir / ".full_download").write_text("ok\n", encoding="utf-8")
    console.print(f"  extraído em {mt_dir / 'Datasets'}")
    return True


# ---------------------------------------------------------------------------
# NIST BreatheSmart
# ---------------------------------------------------------------------------

class _HttpRangeFile(io.RawIOBase):
    """
    Arquivo somente-leitura sobre HTTP Range. Permite abrir o zip remoto do
    NIST com zipfile sem baixar os 202 MB: no smoke só o central directory e
    os membros escolhidos são transferidos.
    """

    def __init__(self, url: str) -> None:
        resp = requests.get(url, headers={"Range": "bytes=0-0"},
                            timeout=_TIMEOUT, allow_redirects=True)
        resp.raise_for_status()
        content_range = resp.headers.get("Content-Range", "")
        if "/" not in content_range:
            raise RuntimeError(f"Servidor sem suporte a Range: {url}")
        self._url = url
        self._size = int(content_range.split("/")[-1])
        self._pos = 0

    def seekable(self) -> bool:
        return True

    def readable(self) -> bool:
        return True

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        if whence == io.SEEK_SET:
            self._pos = offset
        elif whence == io.SEEK_CUR:
            self._pos += offset
        elif whence == io.SEEK_END:
            self._pos = self._size + offset
        return self._pos

    def tell(self) -> int:
        return self._pos

    def read(self, n: int = -1) -> bytes:
        if n < 0:
            n = self._size - self._pos
        if n <= 0 or self._pos >= self._size:
            return b""
        end = min(self._pos + n, self._size) - 1
        resp = requests.get(
            self._url, headers={"Range": f"bytes={self._pos}-{end}"},
            timeout=_TIMEOUT, allow_redirects=True,
        )
        resp.raise_for_status()
        data = resp.content
        self._pos += len(data)
        return data

    def readinto(self, b) -> int:
        data = self.read(len(b))
        b[: len(data)] = data
        return len(data)


def _extract_targz(tar_path: Path, dest_dir: Path) -> None:
    with tarfile.open(tar_path, "r:gz") as tar:
        try:
            tar.extractall(dest_dir, filter="data")
        except TypeError:                      # Python < 3.11.4 sem filter=
            tar.extractall(dest_dir)


def _pick_nist_smoke_members(infos: list) -> list:
    """Escolhe tar.gz pequenos de diretórios variados (round-robin por pasta)
    até NIST_SMOKE_MAX_MEMBERS/NIST_SMOKE_MAX_BYTES."""
    by_dir: dict[str, list] = {}
    for info in sorted(infos, key=lambda i: i.file_size):
        parent = info.filename.rsplit("/", 1)[0]
        by_dir.setdefault(parent, []).append(info)

    picked, total = [], 0
    dirs = sorted(by_dir)
    i = 0
    while len(picked) < NIST_SMOKE_MAX_MEMBERS and any(by_dir.values()):
        d = dirs[i % len(dirs)]
        i += 1
        if not by_dir[d]:
            continue
        info = by_dir[d].pop(0)
        if total + info.file_size > NIST_SMOKE_MAX_BYTES and picked:
            break
        picked.append(info)
        total += info.file_size
    return picked


def download_nist(nist_dir: Path, smoke: bool, console: Console) -> bool:
    console.print("[bold]NIST BreatheSmart[/bold] (mds2-2963)")
    # No modo completo, só o marcador conta — gravações soltas podem ser resto
    # de um --smoke anterior. No smoke, qualquer gravação presente basta.
    if (nist_dir / ".full_download").exists() or (
        smoke and any(nist_dir.rglob("*_csi_real_log.csv"))
    ):
        console.print("  já presente — pulando")
        return True
    nist_dir.mkdir(parents=True, exist_ok=True)

    if smoke:
        console.print("  lendo índice do zip remoto via HTTP Range…")
        remote = io.BufferedReader(_HttpRangeFile(NIST_ZIP_URL),
                                   buffer_size=512 << 10)
        with zipfile.ZipFile(remote) as zf:
            tars = [i for i in zf.infolist()
                    if i.filename.lower().endswith(".tar.gz")]
            picked = _pick_nist_smoke_members(tars)
            console.print(
                f"  {len(picked)} de {len(tars)} gravações "
                f"({sum(i.file_size for i in picked) / 1e6:.1f} MB)…"
            )
            for info in picked:
                rel = Path(info.filename)
                dest_dir = nist_dir / rel.parent / rel.name.replace(".tar.gz", "")
                with tempfile.TemporaryDirectory() as tmp:
                    tar_path = Path(tmp) / "member.tar.gz"
                    with zf.open(info) as src, tar_path.open("wb") as out:
                        shutil.copyfileobj(src, out)
                    _extract_targz(tar_path, dest_dir)
        console.print(f"  extraído em {nist_dir}")
        return True

    console.print("  zip completo (~202 MB)…")
    with tempfile.TemporaryDirectory() as tmp:
        zip_path = Path(tmp) / "BreatheSmartv2.zip"
        _download_to(NIST_ZIP_URL, zip_path, console)
        with zipfile.ZipFile(zip_path) as zf:
            tars = [i for i in zf.infolist()
                    if i.filename.lower().endswith(".tar.gz")]
            console.print(f"  extraindo {len(tars)} gravações…")
            for info in tars:
                rel = Path(info.filename)
                dest_dir = nist_dir / rel.parent / rel.name.replace(".tar.gz", "")
                tar_path = Path(tmp) / "member.tar.gz"
                with zf.open(info) as src, tar_path.open("wb") as out:
                    shutil.copyfileobj(src, out)
                _extract_targz(tar_path, dest_dir)
    (nist_dir / ".full_download").write_text("ok\n", encoding="utf-8")
    console.print(f"  extraído em {nist_dir}")
    return True


# ---------------------------------------------------------------------------
# MM-Fi
# ---------------------------------------------------------------------------

def download_mmfi(
    mmfi_dir: Path, smoke: bool, include_csi: bool, console: Console
) -> bool:
    console.print("[bold]MM-Fi[/bold] (Google Drive, CC BY-NC 4.0)")
    import gdown

    mmfi_dir.mkdir(parents=True, exist_ok=True)
    segments = mmfi_dir / "MMFi_action_segments.csv"
    if not segments.exists():
        console.print("  anotações (MMFi_action_segments.csv, 148 KB)…")
        gdown.download(id=MMFI_SEGMENTS_GDRIVE_ID, output=str(segments),
                       quiet=True)
    else:
        console.print("  anotações já presentes — pulando")

    if smoke or not include_csi:
        console.print(
            "  [yellow]CSI WiFi do MM-Fi só é distribuído em zips de "
            "~18-21 GB por ambiente (sem amostra individual). "
            "Use --include-mmfi no modo completo para baixar E01.zip."
            "[/yellow]"
        )
        return True

    if any(mmfi_dir.rglob("wifi-csi")):
        console.print("  CSI já presente — pulando")
        return True
    console.print("  E01.zip (~21 GB) via gdown — pode levar horas…")
    zip_path = mmfi_dir / "E01.zip"
    gdown.download(id=MMFI_E01_GDRIVE_ID, output=str(zip_path), quiet=False)
    console.print("  extraindo apenas wifi-csi/ e ground_truth.npy…")
    with zipfile.ZipFile(zip_path) as zf:
        for member in zf.namelist():
            if member.endswith("/"):
                continue
            if "wifi-csi/" not in member and not member.endswith("ground_truth.npy"):
                continue
            dest = mmfi_dir / member
            dest.parent.mkdir(parents=True, exist_ok=True)
            with zf.open(member) as src, dest.open("wb") as out:
                shutil.copyfileobj(src, out)
    zip_path.unlink()
    console.print(f"  extraído em {mmfi_dir}")
    return True


# ---------------------------------------------------------------------------
# Wi-Pose (opcional)
# ---------------------------------------------------------------------------

def download_wipose(wipose_dir: Path, smoke: bool, console: Console) -> bool:
    console.print("[bold]Wi-Pose[/bold] (opcional)")
    if smoke:
        console.print(
            "  [yellow]zip único no Google Drive — não há modo amostra; "
            "pulado no smoke test[/yellow]"
        )
        return True
    if any(wipose_dir.rglob("*.mat")):
        console.print("  já presente — pulando")
        return True

    import gdown

    wipose_dir.mkdir(parents=True, exist_ok=True)
    zip_path = wipose_dir / "wipose_dataset.zip"
    gdown.download(WIPOSE_GDRIVE_URL, str(zip_path), fuzzy=True)
    with zipfile.ZipFile(zip_path) as zf:
        zf.extractall(wipose_dir)
    zip_path.unlink()
    console.print(f"  extraído em {wipose_dir}")
    return True


# ---------------------------------------------------------------------------
# Validação — instancia as classes reais da pipeline sobre o que baixou
# ---------------------------------------------------------------------------

def validate(data_dir: Path, console: Console) -> bool:
    """Carrega 1 amostra de cada dataset com as classes de datasets.py.

    Zenodo/WiMANS/CSI-MiningTool/NIST são obrigatórios (falha → exit 1);
    Wi-Pose e MM-Fi são opcionais — ausência é apenas informada.
    """
    from datasets import (
        CSIMiningToolDataset,
        MMFiDataset,
        NISTBreatheSmartDataset,
        WiMANSDataset,
        WiPoseDataset,
        ZenodoPresenceDataset,
    )

    ok = True
    console.print("[bold]Validação[/bold] (datasets.py sobre os dados baixados)")

    try:
        ds = ZenodoPresenceDataset(data_dir / "zenodo_8021099", split="train")
        x, y = ds[0]
        console.print(
            f"  [green]Zenodo OK[/green] — {len(ds)} amostras, "
            f"shape {tuple(x.shape)}, label {int(y)}"
        )
    except Exception as exc:
        console.print(f"  [red]Zenodo FALHOU: {exc}[/red]")
        ok = False

    try:
        ds = WiMANSDataset(data_dir / "WiMANS")
        csi, count, heat = ds[0]
        console.print(
            f"  [green]WiMANS OK[/green] — {len(ds)} amostras, "
            f"csi {tuple(csi.shape)}, count {int(count)}, "
            f"heatmap {tuple(heat.shape)}"
        )
    except Exception as exc:
        console.print(f"  [red]WiMANS FALHOU: {exc}[/red]")
        ok = False

    try:
        ds = CSIMiningToolDataset(data_dir / "WiFi-CSI-MiningTool")
        csi, target = ds[0]
        console.print(
            f"  [green]CSI-MiningTool OK[/green] — {len(ds)} janelas, "
            f"csi {tuple(csi.shape)}, target {target.tolist()}"
        )
    except Exception as exc:
        console.print(f"  [red]CSI-MiningTool FALHOU: {exc}[/red]")
        ok = False

    try:
        ds = NISTBreatheSmartDataset(data_dir / "NIST-BreatheSmart")
        csi, target = ds[0]
        console.print(
            f"  [green]NIST OK[/green] — {len(ds)} gravações, "
            f"csi {tuple(csi.shape)}, target {target.tolist()}"
        )
    except Exception as exc:
        console.print(f"  [red]NIST FALHOU: {exc}[/red]")
        ok = False

    for name, cls, sub in (
        ("Wi-Pose", WiPoseDataset, "Wi-Pose"),
        ("MM-Fi", MMFiDataset, "MM-Fi"),
    ):
        try:
            ds = cls(data_dir / sub)
            csi, count, heat = ds[0]
            console.print(
                f"  [green]{name} OK[/green] — {len(ds)} amostras, "
                f"csi {tuple(csi.shape)}"
            )
        except FileNotFoundError:
            console.print(f"  [yellow]{name}: sem dados (opcional)[/yellow]")
        except Exception as exc:
            console.print(f"  [red]{name} FALHOU: {exc}[/red]")
            ok = False

    return ok


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Baixa os datasets públicos do AetherSense (sem credenciais).",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--smoke", action="store_true",
                        help="Amostra pequena de cada dataset (smoke test).")
    parser.add_argument("--smoke-samples", type=int, default=24,
                        help="Nº de amostras WiMANS no modo --smoke "
                             "(estratificadas pelas classes de contagem 0..5).")
    parser.add_argument("--data-dir", type=Path,
                        default=_REPO_ROOT / "datasets",
                        help="Raiz dos datasets.")
    parser.add_argument("--workers", type=int, default=4,
                        help="Downloads concorrentes do WiMANS.")
    parser.add_argument("--include-wipose", action="store_true",
                        help="Baixa também o Wi-Pose (zip único; auxiliar "
                             "opcional de ocupação).")
    parser.add_argument("--include-mmfi", action="store_true",
                        help="Baixa o CSI WiFi do MM-Fi (E01.zip, ~21 GB; "
                             "CC BY-NC 4.0). Só no modo completo.")
    parser.add_argument("--no-validate", action="store_true",
                        help="Pula a validação com as classes de datasets.py.")
    args = parser.parse_args()

    # Consoles do Windows usam cp1252 e quebram nos glifos do Rich.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass

    console = Console(highlight=False, legacy_windows=False)
    mode = "SMOKE (amostra)" if args.smoke else "COMPLETO"
    console.print(f"[bold]AetherSense — download de datasets[/bold] — modo {mode}")
    console.print(f"Destino: {args.data_dir}\n")

    ok = True
    ok &= download_zenodo(args.data_dir / "zenodo_8021099", args.smoke, console)
    ok &= download_wimans(
        args.data_dir / "WiMANS", args.smoke, args.smoke_samples,
        args.workers, console,
    )
    ok &= download_miningtool(
        args.data_dir / "WiFi-CSI-MiningTool", args.smoke, console)
    ok &= download_nist(args.data_dir / "NIST-BreatheSmart", args.smoke, console)
    ok &= download_mmfi(args.data_dir / "MM-Fi", args.smoke,
                        args.include_mmfi, console)
    if args.include_wipose:
        ok &= download_wipose(args.data_dir / "Wi-Pose", args.smoke, console)

    if not args.no_validate:
        ok &= validate(args.data_dir, console)

    console.print()
    if ok:
        console.print("[bold green]Datasets prontos.[/bold green]")
        if args.smoke:
            console.print("Smoke test do treino:  uv run train.py --fast-dev")
        else:
            console.print("Treino completo:       uv run train.py --epochs 40")
    else:
        console.print("[bold red]Houve falhas — veja as mensagens acima.[/bold red]")
        raise SystemExit(1)


if __name__ == "__main__":
    main()
