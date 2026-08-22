#!/bin/bash
# Pterodactyl 一鍵：Cloudflare Tunnel + Tailscale + 同容器 Nextcloud + 本程式
# 全部在面板完成，不需要 SSH。只要覆蓋這一個檔。
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

dl() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -O "$out" "$url" && return 0
  fi
  return 1
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
    dl "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64" ./cloudflared || true
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

# ─── 同容器 Nextcloud（已併入本檔，不必再上傳 nextcloud-sidecar.sh）──────────

nc_load_env() {
  [[ -f config/nextcloud.env ]] || return 0
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
      local key="${BASH_REMATCH[1]}"
      local val="${BASH_REMATCH[2]}"
      val="${val#\"}"; val="${val%\"}"
      val="${val#\'}"; val="${val%\'}"
      export "$key=$val"
    fi
  done < config/nextcloud.env
}

nc_rand_hex() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 24
    return
  fi
  python3 -c 'import secrets; print(secrets.token_hex(24))' 2>/dev/null && return
  tr -dc 'a-f0-9' </dev/urandom | head -c 48
  echo
}

nc_extract_zip() {
  local zip="$1" dest="$2"
  mkdir -p "$dest"
  if command -v unzip >/dev/null 2>&1; then
    unzip -q -o "$zip" -d "$dest" && return 0
  fi
  python3 - "$zip" "$dest" << 'PY'
import sys, zipfile
zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])
PY
}

nc_write_caddyfile() {
  local root="$PWD/nextcloud"
  local port="${NEXTCLOUD_PORT:-26001}"
  mkdir -p nextcloud-runtime
  cat > nextcloud-runtime/Caddyfile << EOF
{
    admin off
    auto_https off
    persist_config off
    frankenphp {
        num_threads 2
        php_ini {
            memory_limit 512M
            upload_max_filesize 512M
            post_max_size 512M
            max_execution_time 360
            max_input_time 360
            date.timezone Asia/Taipei
            output_buffering 0
        }
    }
}

http://127.0.0.1:${port} {
    root * ${root}
    encode gzip
    php_server
    file_server
    header {
        X-Content-Type-Options nosniff
        Referrer-Policy no-referrer
        -Server
    }
}
EOF
}

nc_ensure_secrets() {
  local admin_user="${NEXTCLOUD_ADMIN_USER:-admin}"
  local admin_pass="${NEXTCLOUD_ADMIN_PASSWORD:-}"
  local oidc_secret="${OIDC_CLIENT_SECRET:-}"

  if [[ -z "$admin_pass" && -f config/nextcloud-admin.txt ]]; then
    admin_pass="$(tr -d '\r\n' < config/nextcloud-admin.txt)"
  fi
  if [[ -z "$admin_pass" ]]; then
    admin_pass="$(nc_rand_hex)"
    umask 077
    printf '%s\n' "$admin_pass" > config/nextcloud-admin.txt
    echo "[nextcloud] 已產生管理員密碼 → config/nextcloud-admin.txt"
  fi

  if [[ -z "$oidc_secret" && -f config/nextcloud-oidc.secret ]]; then
    oidc_secret="$(tr -d '\r\n' < config/nextcloud-oidc.secret)"
  fi
  if [[ -z "$oidc_secret" ]]; then
    oidc_secret="$(nc_rand_hex)"
    umask 077
    printf '%s\n' "$oidc_secret" > config/nextcloud-oidc.secret
    echo "[nextcloud] 已產生 OIDC client_secret → config/nextcloud-oidc.secret"
  fi

  NEXTCLOUD_ADMIN_USER="$admin_user"
  NEXTCLOUD_ADMIN_PASSWORD="$admin_pass"
  OIDC_CLIENT_SECRET="$oidc_secret"
  OIDC_CLIENT_ID="${OIDC_CLIENT_ID:-nextcloud}"
}

nc_occ() {
  (cd nextcloud && ../frankenphp php-cli -d memory_limit=512M occ "$@")
}

nc_download_frankenphp() {
  if [[ -x ./frankenphp ]]; then
    echo "[nextcloud] 已有 ./frankenphp"
    return 0
  fi
  echo "[nextcloud] 下載 FrankenPHP"
  local ver="${FRANKENPHP_VERSION:-v1.9.1}"
  local u
  for u in \
    "https://github.com/php/frankenphp/releases/download/${ver}/frankenphp-linux-x86_64" \
    "https://github.com/php/frankenphp/releases/download/${ver}/frankenphp-linux-x86_64-mimalloc" \
    "https://github.com/php/frankenphp/releases/download/${ver}/frankenphp-linux-x86_64-gnu" \
    "https://github.com/php/frankenphp/releases/latest/download/frankenphp-linux-x86_64"
  do
    echo "[nextcloud] 嘗試 $u"
    if dl "$u" ./frankenphp.tmp; then
      chmod +x ./frankenphp.tmp
      if ./frankenphp.tmp version >/dev/null 2>&1 || ./frankenphp.tmp --version >/dev/null 2>&1; then
        mv ./frankenphp.tmp ./frankenphp
        echo "[nextcloud] FrankenPHP OK"
        return 0
      fi
    fi
    rm -f ./frankenphp.tmp
  done
  echo "[nextcloud] 下載 FrankenPHP 失敗"
  return 1
}

nc_download_nextcloud() {
  if [[ -f nextcloud/occ ]]; then
    echo "[nextcloud] 已有 nextcloud/ 程式"
    return 0
  fi
  echo "[nextcloud] 下載 Nextcloud 31（約 200MB，第一次要幾分鐘）"
  local zip=/tmp/nextcloud.zip
  rm -f "$zip"
  if ! dl "https://download.nextcloud.com/server/releases/latest-31.zip" "$zip"; then
    echo "[nextcloud] 下載 Nextcloud 失敗"
    return 1
  fi
  nc_extract_zip "$zip" /tmp/nc-extract || return 1
  if [[ -d /tmp/nc-extract/nextcloud ]]; then
    rm -rf nextcloud
    mv /tmp/nc-extract/nextcloud nextcloud
  else
    echo "[nextcloud] zip 內容不是預期的 nextcloud/ 目錄"
    return 1
  fi
  rm -rf /tmp/nc-extract "$zip"
  echo "[nextcloud] Nextcloud 檔案就緒"
}

nc_install() {
  if [[ -f nextcloud/config/config.php ]]; then
    echo "[nextcloud] 已安裝過，略過 maintenance:install"
    return 0
  fi
  echo "[nextcloud] 第一次安裝（SQLite）"
  mkdir -p nextcloud-data
  if ! nc_occ maintenance:install \
    --database=sqlite \
    --database-name=nextcloud \
    --admin-user="$NEXTCLOUD_ADMIN_USER" \
    --admin-pass="$NEXTCLOUD_ADMIN_PASSWORD" \
    --data-dir="$PWD/nextcloud-data" \
    --admin-email="admin@localhost"; then
    echo "[nextcloud] occ maintenance:install 失敗"
    return 1
  fi
  echo "[nextcloud] 安裝完成"
}

nc_write_snippet() {
  local public="$1" issuer="$2"
  local admin="$NEXTCLOUD_ADMIN_USER"
  local provider_id="1"
  local listed
  listed="$(nc_occ user_oidc:provider 2>/dev/null || true)"
  if [[ "$listed" =~ (^|[^0-9])([0-9]+)[[:space:]]+Dashboard ]]; then
    provider_id="${BASH_REMATCH[2]}"
  elif [[ "$listed" =~ Dashboard[^0-9]*([0-9]+) ]]; then
    provider_id="${BASH_REMATCH[1]}"
  fi

  umask 077
  cat > config/nextcloud-dashboard.snippet.toml << EOF
# 由 ptero-start.sh 產生。請把下面兩段貼進 config/config.toml
# （若還沒有 [materials] / [oidc]）。密碼見 config/nextcloud-admin.txt

[materials]
backend = "webdav"
webdav_url = "${public}/remote.php/dav/files/${admin}/tronclass-materials"
webdav_username = "${admin}"
webdav_password = "${NEXTCLOUD_ADMIN_PASSWORD}"
frontend_url = "${public}/apps/user_oidc/login/${provider_id}"
frontend_mode = "sso"

[oidc]
enabled = true
issuer = "${issuer}"
client_id = "${OIDC_CLIENT_ID}"
client_secret = "${OIDC_CLIENT_SECRET}"
redirect_uri = "${public}/apps/user_oidc/code"
EOF

  if [[ -f config/config.toml ]] && ! grep -qE '^[[:space:]]*\[oidc\]' config/config.toml; then
    cp -a config/config.toml "config/config.toml.bak.nextcloud.$(date +%Y%m%d%H%M%S)"
    printf '\n# --- Nextcloud SSO（同容器）---\n' >> config/config.toml
    cat config/nextcloud-dashboard.snippet.toml >> config/config.toml
    echo "[nextcloud] 已把 [materials]/[oidc] 附加到 config/config.toml（有備份）"
  else
    echo "[nextcloud] config.toml 已有 [oidc] 或尚不存在，片段寫在 config/nextcloud-dashboard.snippet.toml"
  fi
}

nc_configure() {
  local public="${NEXTCLOUD_PUBLIC_URL:-https://files.jasoncloudserver.me}"
  public="${public%/}"
  local host="${public#https://}"
  host="${host#http://}"
  host="${host%%/*}"
  local issuer="${DASHBOARD_ISSUER:-https://dashboard.jasoncloudserver.me}"
  issuer="${issuer%/}"

  nc_occ config:system:set trusted_domains 0 --value=localhost || true
  nc_occ config:system:set trusted_domains 1 --value=127.0.0.1 || true
  nc_occ config:system:set trusted_domains 2 --value="$host" || true
  nc_occ config:system:set overwrite.cli.url --value="$public" || true
  nc_occ config:system:set overwriteprotocol --value=https || true
  nc_occ config:system:set overwritehost --value="$host" || true
  nc_occ config:system:set overwritewebroot --value="" || true
  nc_occ config:system:set trusted_proxies 0 --value=127.0.0.1 || true
  nc_occ config:system:set htaccess.RewriteBase --value="/" || true
  nc_occ background:cron >/dev/null 2>&1 || true

  mkdir -p "nextcloud-data/${NEXTCLOUD_ADMIN_USER}/files/tronclass-materials"
  nc_occ files:scan "$NEXTCLOUD_ADMIN_USER" >/dev/null 2>&1 || true

  echo "[nextcloud] 安裝 OpenID 應用 user_oidc"
  nc_occ app:install user_oidc >/dev/null 2>&1 || true
  nc_occ app:enable user_oidc >/dev/null 2>&1 || true

  if nc_occ user_oidc:provider 2>/dev/null | grep -qi Dashboard; then
    echo "[nextcloud] OIDC provider Dashboard 已存在"
  else
    nc_occ user_oidc:provider "Dashboard" \
      --clientid="$OIDC_CLIENT_ID" \
      --clientsecret="$OIDC_CLIENT_SECRET" \
      --discoveryuri="${issuer}/admin/oidc/.well-known/openid-configuration" \
      --unique-uid=0 \
      --mapping-uid="preferred_username" || \
    echo "[nextcloud] user_oidc:provider 失敗（可之後在 Nextcloud 應用程式裡手動加）"
  fi

  nc_write_snippet "$public" "$issuer"
}

nc_already_running() {
  local port="${NEXTCLOUD_PORT:-26001}"
  python3 - "$port" << 'PY' >/dev/null 2>&1
import socket, sys
s = socket.socket()
s.settimeout(0.4)
try:
    s.connect(("127.0.0.1", int(sys.argv[1])))
except Exception:
    raise SystemExit(1)
s.close()
PY
}

nc_start_server() {
  local port="${NEXTCLOUD_PORT:-26001}"
  if nc_already_running; then
    echo "[nextcloud] 已在 127.0.0.1:${port} 聽"
    return 0
  fi
  nc_write_caddyfile
  echo "[nextcloud] 啟動 FrankenPHP :${port} → nextcloud/"
  nohup ./frankenphp run --config "$PWD/nextcloud-runtime/Caddyfile" \
    >> logs/nextcloud-frankenphp.log 2>&1 &
  echo $! > nextcloud-runtime/frankenphp.pid
  disown || true
  sleep 2
  if nc_already_running; then
    echo "[nextcloud] 已聽 127.0.0.1:${port}"
  else
    echo "[nextcloud] 還沒起來，見 logs/nextcloud-frankenphp.log"
    tail -n 40 logs/nextcloud-frankenphp.log 2>/dev/null || true
  fi
}

nc_bootstrap() {
  mkdir -p logs config nextcloud-runtime nextcloud-data
  echo "===== $(date -Iseconds) nextcloud in ptero-start ====="
  nc_load_env
  nc_download_frankenphp || return 0
  nc_download_nextcloud || return 0
  nc_ensure_secrets
  nc_install || return 0
  nc_configure
  nc_start_server
  echo "[nextcloud] 完成。公開網址：${NEXTCLOUD_PUBLIC_URL:-https://files.jasoncloudserver.me}"
  echo "[nextcloud] Tunnel：files.jasoncloudserver.me → http://127.0.0.1:${NEXTCLOUD_PORT:-26001}"
}

start_nextcloud() {
  nc_load_env
  if [[ ! -f config/nextcloud.enable && "${NEXTCLOUD_ENABLE:-0}" != "1" && ! -f nextcloud/config/config.php ]]; then
    echo "[ptero-start] 同容器 Nextcloud 未啟用（新增空檔 config/nextcloud.enable 後重啟）"
    return 0
  fi
  echo "[ptero-start] 背景啟動同容器 Nextcloud（127.0.0.1:${NEXTCLOUD_PORT:-26001}）"
  # 下載／安裝在背景，不擋 Dashboard
  ( nc_bootstrap ) >> logs/nextcloud.log 2>&1 &
  disown || true
}

start_nextcloud

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
