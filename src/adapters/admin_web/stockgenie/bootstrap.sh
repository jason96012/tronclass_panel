#!/usr/bin/env bash
# STOCKGENIE_PYTHON_BOOTSTRAP_V1
# Pterodactyl 映像通常沒有系統 python3。把獨立 uv + CPython 裝到 run/，
# 再 pip 智選相依。不寫 src/、不碰點名。可重入（已裝就秒退）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ -f "$PWD/src/adapters/admin_web/stockgenie/app.py" ]]; then
  ROOT="$PWD"
elif [[ -f "$SCRIPT_DIR/app.py" ]]; then
  ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
else
  echo "[stockgenie-bootstrap] 找不到 app.py（請在 /home/container 執行）" >&2
  exit 1
fi

VENV="$ROOT/run/stockgenie-venv"
PYBIN="$VENV/bin/python"
BINFILE="$ROOT/run/stockgenie-python-bin.txt"
UV_DIR="$ROOT/run/uv"
UV="$UV_DIR/uv"
LOCK="$ROOT/run/stockgenie-bootstrap.lock"
REQ="$SCRIPT_DIR/requirements.txt"

mkdir -p "$ROOT/run" "$ROOT/logs"

python_ok() {
  local bin="$1"
  [[ -x "$bin" ]] || return 1
  "$bin" -c "import fastapi,uvicorn,pandas,numpy,httpx" >/dev/null 2>&1
}

write_bin() {
  printf '%s\n' "$1" > "$BINFILE"
  echo "[stockgenie-bootstrap] python = $1"
}

if python_ok "$PYBIN"; then
  write_bin "$PYBIN"
  exit 0
fi
if python_ok "$VENV/bin/python3"; then
  write_bin "$VENV/bin/python3"
  exit 0
fi
if command -v python3 >/dev/null 2>&1 && python_ok "$(command -v python3)"; then
  write_bin "$(command -v python3)"
  exit 0
fi

if ! mkdir "$LOCK" 2>/dev/null; then
  echo "[stockgenie-bootstrap] 另一個安裝正在進行，等候…"
  i=0
  while (( i < 180 )); do
    if python_ok "$PYBIN"; then
      write_bin "$PYBIN"
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo "[stockgenie-bootstrap] 等候逾時" >&2
  exit 1
fi
cleanup_lock() { rmdir "$LOCK" 2>/dev/null || true; }
trap cleanup_lock EXIT

if python_ok "$PYBIN"; then
  write_bin "$PYBIN"
  exit 0
fi

arch="$(uname -m 2>/dev/null || echo x86_64)"
case "$arch" in
  x86_64|amd64) uv_arch="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) uv_arch="aarch64-unknown-linux-gnu" ;;
  *)
    echo "[stockgenie-bootstrap] 不支援的架構：$arch" >&2
    exit 1
    ;;
esac

dl() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --retry-all-errors -o "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -O "$out" "$url" && return 0
  fi
  return 1
}

if [[ ! -x "$UV" ]]; then
  echo "[stockgenie-bootstrap] 下載 uv（$uv_arch）"
  mkdir -p "$UV_DIR"
  tgz="$UV_DIR/uv.tgz"
  url="https://github.com/astral-sh/uv/releases/latest/download/uv-${uv_arch}.tar.gz"
  if ! dl "$url" "$tgz"; then
    echo "[stockgenie-bootstrap] 下載 uv 失敗（需要出網）" >&2
    exit 1
  fi
  tar -xzf "$tgz" -C "$UV_DIR"
  if [[ -x "$UV_DIR/uv-${uv_arch}/uv" ]]; then
    mv -f "$UV_DIR/uv-${uv_arch}/uv" "$UV"
    rm -rf "$UV_DIR/uv-${uv_arch}"
  elif [[ -x "$UV_DIR/uv" ]]; then
    :
  else
    echo "[stockgenie-bootstrap] uv 壓縮檔格式不符" >&2
    exit 1
  fi
  chmod +x "$UV"
  rm -f "$tgz"
fi

export UV_CACHE_DIR="$ROOT/run/uv-cache"
export UV_PYTHON_INSTALL_DIR="$ROOT/run/stockgenie-python"
mkdir -p "$UV_CACHE_DIR" "$UV_PYTHON_INSTALL_DIR"

echo "[stockgenie-bootstrap] 安裝 CPython 3.12"
"$UV" python install 3.12
echo "[stockgenie-bootstrap] 建立 venv"
"$UV" venv "$VENV" --python 3.12 --clear
echo "[stockgenie-bootstrap] 安裝智選套件"
if [[ -f "$REQ" ]]; then
  "$UV" pip install --python "$PYBIN" -r "$REQ"
else
  "$UV" pip install --python "$PYBIN" fastapi uvicorn yfinance pandas numpy httpx
fi

if ! python_ok "$PYBIN"; then
  echo "[stockgenie-bootstrap] venv 無法 import fastapi/uvicorn" >&2
  exit 1
fi
write_bin "$PYBIN"
echo "[stockgenie-bootstrap] 完成"
