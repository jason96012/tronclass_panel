#!/bin/bash
# Pterodactyl 一鍵：Cloudflare Tunnel（config/cf.token）+ 本程式
# 全部在面板完成，不需要 SSH。
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

export RUST_LOG="${RUST_LOG:-info}"
PORT="${SERVER_PORT:-26000}"
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_INCREMENTAL=1

mkdir -p config logs

if [[ -x ./sync-to-github.sh || -f ./sync-to-github.sh ]]; then
  echo "[ptero-start] 背景同步程式碼到 GitHub（不含個資）"
  nohup bash ./sync-to-github.sh >/dev/null 2>&1 &
  disown || true
fi

if [[ -f config/config.toml ]]; then
  if grep -qE '^[[:space:]]*webhook_bind_addr' config/config.toml; then
    sed -i 's/^[[:space:]]*webhook_bind_addr[[:space:]]*=.*/webhook_bind_addr = "0.0.0.0"/' config/config.toml
  else
    printf '\nwebhook_bind_addr = "0.0.0.0"\n' >> config/config.toml
  fi
  if grep -qE '^[[:space:]]*webhook_port' config/config.toml; then
    sed -i "s/^[[:space:]]*webhook_port[[:space:]]*=.*/webhook_port = ${PORT}/" config/config.toml
  else
    printf '\nwebhook_port = %s\n' "${PORT}" >> config/config.toml
  fi
  echo "[ptero-start] 程式將聽 0.0.0.0:${PORT}"
fi

read_token() {
  if [[ -n "${CF_TUNNEL_TOKEN:-}" ]]; then
    printf '%s' "${CF_TUNNEL_TOKEN}" | tr -d '\r\n '
    return
  fi
  if [[ -f config/cf.token ]]; then
    tr -d '\r\n ' < config/cf.token
  fi
}

start_tunnel() {
  local token
  token="$(read_token)"
  if [[ -z "$token" ]]; then
    echo "[ptero-start] 沒有 Tunnel token。請新增檔案 config/cf.token（內容只有 Cloudflare 複製的 token）"
    return 0
  fi
  if [[ "${#token}" -lt 40 ]]; then
    echo "[ptero-start] config/cf.token 太短，請貼完整 token（eyJ 開頭）"
    return 0
  fi
  echo "[ptero-start] 讀到 token（長度 ${#token}）"

  if [[ ! -x ./cloudflared ]]; then
    echo "[ptero-start] 下載 cloudflared..."
    if command -v curl >/dev/null 2>&1; then
      curl -fsSL -o ./cloudflared "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64" || true
    fi
    if [[ ! -s ./cloudflared ]] && command -v wget >/dev/null 2>&1; then
      wget -q -O ./cloudflared "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64" || true
    fi
    chmod +x ./cloudflared 2>/dev/null || true
  fi
  if [[ ! -x ./cloudflared ]]; then
    echo "[ptero-start] 無法下載 cloudflared，Tunnel 沒啟動"
    return 0
  fi

  echo "[ptero-start] 啟動 Cloudflare Tunnel"
  nohup ./cloudflared tunnel --no-autoupdate run --token "$token" > logs/cloudflared.log 2>&1 &
  disown || true
  sleep 2
  echo "[ptero-start] cloudflared 最近 log："
  tail -n 15 logs/cloudflared.log 2>/dev/null || true
}

start_tunnel

dl() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url" && return 0
  fi
  return 1
}

start_tailscale() {
  local key=""
  if [[ -n "${TS_AUTHKEY:-}" ]]; then
    key="$(printf '%s' "${TS_AUTHKEY}" | tr -d '\r\n ')"
  elif [[ -f config/tailscale.authkey ]]; then
    key="$(tr -d '\r\n ' < config/tailscale.authkey)"
  fi
  if [[ -z "$key" ]]; then
    echo "[ptero-start] 未設定 Tailscale（可把 auth key 放到 config/tailscale.authkey）"
    return 0
  fi
  echo "[ptero-start] 讀到 Tailscale auth key（長度 ${#key}）"

  if [[ ! -x ./tailscaled || ! -x ./tailscale ]]; then
    echo "[ptero-start] 下載 Tailscale 靜態檔..."
    local ver=""
    ver="$(curl -fsSL https://api.github.com/repos/tailscale/tailscale/releases/latest 2>/dev/null | grep -o '"tag_name": *"v[^"]*"' | head -1 | grep -o 'v[0-9.]*' | tr -d v)"
    [[ -n "$ver" ]] || ver="1.84.0"
    dl "https://pkgs.tailscale.com/stable/tailscale_${ver}_amd64.tgz" /tmp/tailscale.tgz || {
      echo "[ptero-start] 下載 Tailscale 失敗"
      return 0
    }
    local dir
    dir="$(tar -tzf /tmp/tailscale.tgz | head -1 | cut -d/ -f1)"
    tar -xzf /tmp/tailscale.tgz -C /tmp
    cp "/tmp/${dir}/tailscale" ./tailscale
    cp "/tmp/${dir}/tailscaled" ./tailscaled
    chmod +x ./tailscale ./tailscaled
  fi

  mkdir -p tailscale-state logs
  export TS_VAR_ROOT="$PWD/tailscale-state"
  echo "[ptero-start] 啟動 tailscaled（userspace，statedir=$TS_VAR_ROOT）"
  nohup ./tailscaled \
    --statedir="$TS_VAR_ROOT" \
    --socket="$TS_VAR_ROOT/tailscaled.sock" \
    --tun=userspace-networking \
    > logs/tailscaled.log 2>&1 &
  disown || true
  sleep 2

  echo "[ptero-start] tailscale up"
  ./tailscale --socket=./tailscale-state/tailscaled.sock up \
    --authkey="$key" \
    --hostname="${TS_HOSTNAME:-tronclass-ptero}" \
    --accept-dns=false \
    >> logs/tailscale.log 2>&1 || {
    echo "[ptero-start] tailscale up 失敗，見 logs/tailscale.log"
    tail -n 20 logs/tailscale.log 2>/dev/null || true
    return 0
  }

  SOCK=./tailscale-state/tailscaled.sock
  TS_HOST="${TS_HOSTNAME:-tronclass-ptero}.tail31b530.ts.net"

  echo "[ptero-start] 申請 MagicDNS HTTPS 憑證 ${TS_HOST}"
  if ./tailscale --socket="$SOCK" cert --cert-file=./tailscale-state/cert.pem --key-file=./tailscale-state/key.pem "$TS_HOST"; then
    echo "[ptero-start] 憑證 OK，啟動本機 TLS 代理 :8443 -> :${PORT}"
    nohup python3 ./ts-https-proxy.py "${PORT}" ./tailscale-state/cert.pem ./tailscale-state/key.pem \
      > logs/ts-https-proxy.log 2>&1 &
    disown || true
    sleep 1
    ./tailscale --socket="$SOCK" serve reset >> logs/tailscale.log 2>&1 || true
    echo "[ptero-start] serve TCP 443 -> 127.0.0.1:8443（Tailscale 內 https:// 用這個）"
    ./tailscale --socket="$SOCK" serve --bg --tcp=443 tcp://127.0.0.1:8443 || \
      ./tailscale --socket="$SOCK" serve --bg tcp://127.0.0.1:8443 || true
  else
    echo "[ptero-start] 憑證失敗。後台 DNS 需 Enable HTTPS Certificates"
    ./tailscale --socket="$SOCK" serve reset >> logs/tailscale.log 2>&1 || true
  fi

  echo "[ptero-start] Funnel（沒開 Tailscale 的手機掃 QR 用）"
  ./tailscale --socket="$SOCK" funnel --bg "http://127.0.0.1:${PORT}" >> logs/tailscale.log 2>&1 || \
  ./tailscale --socket="$SOCK" funnel 443 on >> logs/tailscale.log 2>&1 || \
    echo "[ptero-start] Funnel 未開（可到後台打開 Funnel）"

  echo "[ptero-start] Tailscale 狀態："
  ./tailscale --socket="$SOCK" status 2>/dev/null | head -n 8 || true
  echo "[ptero-start] serve / funnel："
  ./tailscale --socket="$SOCK" serve status 2>/dev/null || true
  ./tailscale --socket="$SOCK" funnel status 2>/dev/null || true
}

start_tailscale

run_bin() {
  chmod +x "$1" 2>/dev/null || true
  echo "[ptero-start] 啟動 $1"
  exec "$1"
}

# 已編譯過就直接跑，改程式後把環境變數 FORCE_REBUILD=1 再重啟
if [[ -f ./target/release/tronclass_rollcall && "${FORCE_REBUILD:-0}" != "1" ]]; then
  run_bin ./target/release/tronclass_rollcall
fi

if [[ -f Cargo.toml && -d src ]]; then
  if [[ -f Cargo.lock ]] && grep -q 'reqwest 0.13' Cargo.lock; then
    echo "[ptero-start] 刪除不相容的 Cargo.lock"
    rm -f Cargo.lock
  fi
  echo "[ptero-start] cargo build --release --no-default-features"
  if cargo build --release --no-default-features; then
    run_bin ./target/release/tronclass_rollcall
  fi
  echo "[ptero-start] 編譯失敗"
  exit 1
fi

echo "[ptero-start] 找不到 target/release/tronclass_rollcall"
exit 1
