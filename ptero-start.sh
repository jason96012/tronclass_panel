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
    echo "[nextcloud] 用 unzip 解壓"
    unzip -q -o "$zip" -d "$dest" && return 0
  fi
  # rust yolk 通常沒有 python3 / unzip，用已下載的 FrankenPHP
  if [[ -x ./frankenphp ]]; then
    echo "[nextcloud] 用 FrankenPHP 解壓 zip"
    nc_write_php_ini
    cat > nextcloud-runtime/unzip.php << 'PHP'
<?php
$z = $argv[1] ?? '';
$d = $argv[2] ?? '';
if (!class_exists('ZipArchive')) { fwrite(STDERR, "no ZipArchive\n"); exit(1); }
$zip = new ZipArchive();
if ($zip->open($z) !== true) { fwrite(STDERR, "open failed\n"); exit(1); }
if (!$zip->extractTo($d)) { fwrite(STDERR, "extract failed\n"); exit(1); }
$zip->close();
PHP
    PHPRC="$PWD/nextcloud-runtime" ./frankenphp php-cli nextcloud-runtime/unzip.php "$zip" "$dest" && return 0
  fi
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$zip" "$dest" << 'PY'
import sys, zipfile
zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])
PY
    return $?
  fi
  echo "[nextcloud] 沒有 unzip / python3 / ZipArchive"
  return 1
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

nc_write_php_ini() {
  mkdir -p nextcloud-runtime
  cat > nextcloud-runtime/freetype-polyfill.php << 'PHP'
<?php
// static-php common 的 GD 沒有 FreeType，Nextcloud 安裝產頭像會炸
if (!function_exists('imagettfbbox')) {
    function imagettfbbox($size, $angle, $fontfile, $text) {
        $w = max(8, (int) (strlen((string) $text) * $size * 0.6));
        $h = max(8, (int) $size);
        return [0, $h, $w, $h, $w, 0, 0, 0];
    }
}
if (!function_exists('imagettftext')) {
    function imagettftext($image, $size, $angle, $x, $y, $color, $fontfile, $text) {
        if (function_exists('imagestring')) {
            imagestring($image, 5, (int) $x, max(0, (int) $y - 12), (string) $text, $color);
        }
        return true;
    }
}
PHP
  cat > nextcloud-runtime/php.ini << EOF
memory_limit=512M
upload_max_filesize=512M
post_max_size=512M
max_execution_time=360
max_input_time=360
date.timezone=Asia/Taipei
output_buffering=0
auto_prepend_file=${PWD}/nextcloud-runtime/freetype-polyfill.php
EOF
}

nc_php_works() {
  local bin="$1"
  [[ -x "$bin" ]] || return 1
  "$bin" -r 'echo "ok";' >/dev/null 2>&1
}

nc_ensure_php() {
  if nc_php_works ./php; then
    echo "[nextcloud] 使用 ./php"
    return 0
  fi
  echo "[nextcloud] 下載靜態 PHP 8.3（FrankenPHP php-cli 在這台 CPU 會 Illegal instruction）"
  mkdir -p nextcloud-runtime/php-extract
  local ver tgz="$PWD/nextcloud-runtime/php-cli.tar.gz"
  for ver in 8.3.23 8.3.22 8.3.21 8.3.16 8.3.12 8.2.28 8.4.10; do
    echo "[nextcloud] 嘗試 php-${ver}-cli-linux-x86_64"
    rm -f "$tgz"
    if nc_dl_resume "https://dl.static-php.dev/static-php-cli/common/php-${ver}-cli-linux-x86_64.tar.gz" "$tgz"; then
      rm -rf nextcloud-runtime/php-extract
      mkdir -p nextcloud-runtime/php-extract
      if tar -xzf "$tgz" -C nextcloud-runtime/php-extract; then
        local found
        found="$(find nextcloud-runtime/php-extract -type f -name php | head -1)"
        if [[ -n "$found" ]]; then
          cp "$found" ./php
          chmod +x ./php
          if nc_php_works ./php; then
            rm -f "$tgz"
            rm -rf nextcloud-runtime/php-extract
            echo "[nextcloud] 靜態 PHP OK：$("$PWD/php" -r 'echo PHP_VERSION;')"
            return 0
          fi
        fi
      fi
    fi
  done
  echo "[nextcloud] 靜態 PHP 下載失敗"
  return 1
}

nc_occ() {
  nc_write_php_ini
  if ! nc_php_works ./php; then
    nc_ensure_php || return 1
  fi
  (cd nextcloud && ../php -c ../nextcloud-runtime/php.ini occ "$@")
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

nc_free_mb() {
  df -Pm . 2>/dev/null | awk 'NR==2 {print $4}'
}

nc_need_mb() {
  local need="$1" have
  have="$(nc_free_mb)"
  echo "[nextcloud] 剩餘磁碟約 ${have:-?} MB（這一步需要約 ${need} MB）"
  if [[ -n "$have" && "$have" =~ ^[0-9]+$ && "$have" -lt "$need" ]]; then
    echo "[nextcloud] 磁碟不夠。請在面板加大磁碟，或刪掉用不到的大目錄後重啟："
    echo "[nextcloud]   target/release/deps  target/release/build  target/release/incremental  .cargo"
    echo "[nextcloud] 有編譯好的 target/release/tronclass_rollcall 就夠跑，那些中間檔可刪。"
    return 1
  fi
  return 0
}

nc_dl_resume() {
  local url="$1" out="$2"
  mkdir -p "$(dirname "$out")"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 5 --retry-all-errors -C - --output "$out" "$url" && return 0
    # 續傳檔可能壞了，整份重抓
    rm -f "$out"
    curl -fL --retry 3 --output "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -c -O "$out" "$url" && return 0
  fi
  return 1
}

nc_download_nextcloud() {
  if [[ -f nextcloud/occ ]]; then
    echo "[nextcloud] 已有 nextcloud/ 程式"
    return 0
  fi
  mkdir -p nextcloud-runtime
  # 不要寫 /tmp：Pterodactyl 的 /tmp 常常只有約 100MB
  rm -f /tmp/nextcloud.zip
  local zip="$PWD/nextcloud-runtime/nextcloud-31.zip"
  local extract="$PWD/nextcloud-runtime/extract"
  echo "[nextcloud] 下載 Nextcloud 31 到家目錄（不寫 /tmp）"
  df -h . 2>/dev/null | head -n 2 || true
  nc_need_mb 900 || return 1

  if [[ -f "$zip" ]]; then
    echo "[nextcloud] 發現未下完的 zip，嘗試續傳"
  fi
  if ! nc_dl_resume "https://download.nextcloud.com/server/releases/latest-31.zip" "$zip"; then
    echo "[nextcloud] 下載 Nextcloud 失敗"
    return 1
  fi
  local zsize
  zsize="$(wc -c < "$zip" 2>/dev/null | tr -d ' ')"
  echo "[nextcloud] zip 大小 ${zsize:-?} bytes"
  if [[ -n "$zsize" && "$zsize" -lt 80000000 ]]; then
    echo "[nextcloud] zip 太小，當失敗重抓"
    rm -f "$zip"
    return 1
  fi

  echo "[nextcloud] 解壓（解完就刪壓縮檔省空間）"
  rm -rf "$extract"
  if ! nc_extract_zip "$zip" "$extract"; then
    echo "[nextcloud] zip 解不開，改下 tar.gz（系統一定有 tar）"
    rm -rf "$extract"
    local tgz="$PWD/nextcloud-runtime/nextcloud-31.tar.gz"
    if ! nc_dl_resume "https://download.nextcloud.com/server/releases/latest-31.tar.gz" "$tgz"; then
      echo "[nextcloud] 下載 tar.gz 也失敗"
      return 1
    fi
    mkdir -p "$extract"
    if ! tar -xzf "$tgz" -C "$extract"; then
      echo "[nextcloud] tar 解壓失敗"
      rm -rf "$extract"
      return 1
    fi
    rm -f "$tgz"
  fi
  rm -f "$zip"
  if [[ -d "$extract/nextcloud" ]]; then
    rm -rf nextcloud
    mv "$extract/nextcloud" nextcloud
  else
    echo "[nextcloud] 壓縮檔內容不是預期的 nextcloud/ 目錄"
    rm -rf "$extract"
    return 1
  fi
  rm -rf "$extract"
  echo "[nextcloud] Nextcloud 檔案就緒"
}

nc_install() {
  if [[ -f nextcloud/config/config.php ]]; then
    echo "[nextcloud] 已安裝過，略過 maintenance:install"
    return 0
  fi
  echo "[nextcloud] 第一次安裝（SQLite）"
  # 前幾次失敗會留下 admin 的檔案，再裝會報 Login is invalid because files already exist
  if [[ -d nextcloud-data && ! -f nextcloud/config/config.php ]]; then
    local bak="nextcloud-data.bak.$(date +%Y%m%d%H%M%S)"
    echo "[nextcloud] 移走半套資料 → ${bak}"
    mv nextcloud-data "$bak" || true
  fi
  rm -f nextcloud/config/config.php nextcloud/config/config.php.bak 2>/dev/null || true
  mkdir -p nextcloud-data
  if ! nc_occ maintenance:install \
    --database=sqlite \
    --database-name=nextcloud \
    --admin-user="$NEXTCLOUD_ADMIN_USER" \
    --admin-pass="$NEXTCLOUD_ADMIN_PASSWORD" \
    --data-dir="$PWD/nextcloud-data"; then
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
      --mapping-uid="preferred_username" \
      --mapping-groups="groups" || \
    echo "[nextcloud] user_oidc:provider 失敗（可之後在 Nextcloud 應用程式裡手動加）"
  fi

  nc_setup_shared_cloud
  nc_write_snippet "$public" "$issuer"
}

nc_setup_shared_cloud() {
  # 全班共用同一個資料夾，總容量預設 10GB（不是每人 10GB）
  local quota_gb="${NEXTCLOUD_QUOTA_GB:-10}"
  local bytes=$((quota_gb * 1024 * 1024 * 1024))
  echo "[nextcloud] 設定共用雲端 ${quota_gb} GB（群組 classroom / 資料夾 教材）"

  nc_occ group:add classroom >/dev/null 2>&1 || true
  nc_occ group:adduser classroom "$NEXTCLOUD_ADMIN_USER" >/dev/null 2>&1 || true
  nc_occ config:app:set files default_quota --value="${quota_gb} GB" >/dev/null 2>&1 || true
  nc_occ user:setting "$NEXTCLOUD_ADMIN_USER" files quota "${quota_gb} GB" >/dev/null 2>&1 || true

  nc_occ app:install groupfolders >/dev/null 2>&1 || true
  nc_occ app:enable groupfolders >/dev/null 2>&1 || true

  local list id
  list="$(nc_occ groupfolders:list 2>/dev/null || true)"
  if echo "$list" | grep -q 教材; then
    echo "[nextcloud] 群組資料夾 教材 已存在"
  else
    nc_occ groupfolders:create 教材 >/dev/null 2>&1 || \
      echo "[nextcloud] groupfolders:create 失敗（可之後在 Nextcloud 應用程式 → 群組資料夾手動建）"
  fi
  id="$(nc_occ groupfolders:list 2>/dev/null | awk '/教材/{print $1}' | tr -dc '0-9' | head -c 8)"
  [[ -n "$id" ]] || id=1
  nc_occ groupfolders:quota "$id" "$bytes" >/dev/null 2>&1 || true
  nc_occ groupfolders:group "$id" classroom write >/dev/null 2>&1 || true
  nc_occ groupfolders:group "$id" admin write >/dev/null 2>&1 || true
  echo "[nextcloud] 共用資料夾就緒：每個人進 Nextcloud 都看得到「教材」，一共 ${quota_gb}GB"
}

nc_already_running() {
  local port="${NEXTCLOUD_PORT:-26001}"
  command -v curl >/dev/null 2>&1 || return 1
  local body
  body="$(curl -sS --max-time 3 "http://127.0.0.1:${port}/" 2>/dev/null || true)"
  echo "$body" | grep -qiE 'nextcloud|login|password|nc-|' || return 1
}

nc_start_server() {
  local port="${NEXTCLOUD_PORT:-26001}"
  # 這台 CPU 上 FrankenPHP 接 PHP 請求會白畫面，一律用靜態 PHP
  if [[ -f nextcloud-runtime/frankenphp.pid ]]; then
    kill "$(cat nextcloud-runtime/frankenphp.pid)" >/dev/null 2>&1 || true
  fi
  if command -v pkill >/dev/null 2>&1; then
    pkill -f 'frankenphp run' >/dev/null 2>&1 || true
    pkill -f "php -S 127.0.0.1:${port}" >/dev/null 2>&1 || true
  fi
  sleep 1
  nc_write_php_ini
  if ! nc_php_works ./php; then
    nc_ensure_php || return 1
  fi
  echo "[nextcloud] 用靜態 PHP 內建伺服器 :${port}（不用 FrankenPHP，避免白畫面）"
  cat > nextcloud/router.php << 'PHP'
<?php
$path = parse_url($_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH) ?: '/';
$file = __DIR__ . $path;
if ($path !== '/' && is_file($file)) {
    return false;
}
require __DIR__ . '/index.php';
PHP
  nohup ./php -c "$PWD/nextcloud-runtime/php.ini" \
    -S "127.0.0.1:${port}" -t "$PWD/nextcloud" \
    "$PWD/nextcloud/router.php" \
    >> logs/nextcloud-php.log 2>&1 &
  echo $! > nextcloud-runtime/frankenphp.pid
  disown || true
  local i
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if nc_already_running; then
      echo "[nextcloud] 已聽 127.0.0.1:${port}（有 Nextcloud HTML）"
      return 0
    fi
    sleep 2
  done
  echo "[nextcloud] 還沒看到登入頁，見 logs/nextcloud-php.log"
  tail -n 40 logs/nextcloud-php.log 2>/dev/null || true
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
  if [[ -f ./apply-nextcloud-sso-now.sh ]]; then
    echo "[nextcloud] 執行 apply-nextcloud-sso-now.sh"
    bash ./apply-nextcloud-sso-now.sh || true
  fi
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
