#!/usr/bin/env bash
# STOCKGENIE_PYTHON_BOOTSTRAP_V1
# Pterodactyl 映像通常沒有系統 python3。把獨立 uv + CPython 裝到 run/，
# 再 pip 智選相依。不寫 src/、不碰點名。
#
# STOCKGENIE_BOOTSTRAP_SKIP_IF_PRESENT_V1：run/stockgenie-venv 裡只要已有
# python 可執行檔，開機就沿用，絕不重抓 uv / CPython / pip。
# STOCKGENIE_BOOTSTRAP_PIECEWISE_V1：若只缺其中一段，只補那一段；已有的不重抓。
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
UV_CACHE_DIR="$ROOT/run/uv-cache"
UV_PYTHON_INSTALL_DIR="$ROOT/run/stockgenie-python"

mkdir -p "$ROOT/run" "$ROOT/logs"

write_bin() {
  printf '%s\n' "$1" > "$BINFILE"
  echo "[stockgenie-bootstrap] python = $1"
}

venv_python() {
  if [[ -x "$PYBIN" ]]; then
    printf '%s\n' "$PYBIN"
    return 0
  fi
  if [[ -x "$VENV/bin/python3" ]]; then
    printf '%s\n' "$VENV/bin/python3"
    return 0
  fi
  if [[ -s "$BINFILE" ]]; then
    local listed
    listed="$(tr -d '\r\n' < "$BINFILE" 2>/dev/null || true)"
    if [[ -n "$listed" && -x "$listed" ]]; then
      printf '%s\n' "$listed"
      return 0
    fi
  fi
  return 1
}

pkgs_ok() {
  local sp=""
  sp="$(ls -d "$VENV"/lib/python3.*/site-packages 2>/dev/null | head -1 || true)"
  [[ -n "$sp" && -d "$sp/fastapi" && -d "$sp/pandas" && -d "$sp/uvicorn" ]]
}

# 已裝過：venv python + fastapi/pandas/uvicorn 目錄都在，不 import、不連網。
reuse_existing() {
  local bin=""
  bin="$(venv_python 2>/dev/null || true)"
  if [[ -z "$bin" ]]; then
    return 1
  fi
  if ! pkgs_ok; then
    echo "[stockgenie-bootstrap] python 已在但套件不齊，只補 pip"
    return 1
  fi
  write_bin "$bin"
  echo "[stockgenie-bootstrap] 沿用已安裝環境，略過下載"
  return 0
}

if reuse_existing; then
  exit 0
fi

clear_stale_lock() {
  [[ -d "$LOCK" ]] || return 0
  local pid=""
  pid="$(cat "$LOCK/pid" 2>/dev/null || true)"
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
    return 0
  fi
  echo "[stockgenie-bootstrap] 清除過期 lock"
  rm -rf "$LOCK"
}

clear_stale_lock

if ! mkdir "$LOCK" 2>/dev/null; then
  echo "[stockgenie-bootstrap] 另一個安裝正在進行，等候…"
  i=0
  while (( i < 180 )); do
    if reuse_existing; then
      exit 0
    fi
    sleep 1
    i=$((i + 1))
  done
  echo "[stockgenie-bootstrap] 等候逾時" >&2
  exit 1
fi
printf '%s\n' "$$" > "$LOCK/pid"
cleanup_lock() { rm -rf "$LOCK" 2>/dev/null || true; }
trap cleanup_lock EXIT

if reuse_existing; then
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
    curl -fL --connect-timeout 15 --max-time 180 --retry 3 --retry-all-errors -o "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -T 180 -t 3 -O "$out" "$url" && return 0
  fi
  return 1
}

if [[ -x "$UV" ]]; then
  echo "[stockgenie-bootstrap] 沿用已有 uv"
else
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

export UV_CACHE_DIR
export UV_PYTHON_INSTALL_DIR
mkdir -p "$UV_CACHE_DIR" "$UV_PYTHON_INSTALL_DIR"

cpython_present() {
  find "$UV_PYTHON_INSTALL_DIR" \( -type f -o -type l \) \
    \( -name python -o -name python3 -o -name 'python3.12' \) \
    -print 2>/dev/null | head -1 | grep -q .
}

if cpython_present; then
  echo "[stockgenie-bootstrap] 沿用已有 CPython"
else
  echo "[stockgenie-bootstrap] 安裝 CPython 3.12（僅首次）"
  "$UV" python install 3.12
fi

if venv_python >/dev/null; then
  echo "[stockgenie-bootstrap] 沿用已有 venv"
else
  echo "[stockgenie-bootstrap] 建立 venv（不 --clear，以免下次開機把已裝環境清掉）"
  "$UV" venv "$VENV" --python 3.12
fi

if pkgs_ok; then
  echo "[stockgenie-bootstrap] 套件已在，略過 pip"
else
  echo "[stockgenie-bootstrap] 安裝智選套件（僅缺件時；沿用 uv cache）"
  if [[ -f "$REQ" ]]; then
    "$UV" pip install --python "$PYBIN" -r "$REQ"
  else
    "$UV" pip install --python "$PYBIN" fastapi uvicorn yfinance pandas numpy httpx
  fi
fi

if [[ ! -x "$PYBIN" && ! -x "$VENV/bin/python3" ]]; then
  echo "[stockgenie-bootstrap] venv 沒有 python 可執行檔" >&2
  exit 1
fi
if reuse_existing; then
  echo "[stockgenie-bootstrap] 完成"
  exit 0
fi
echo "[stockgenie-bootstrap] 安裝後仍找不到 python" >&2
exit 1
