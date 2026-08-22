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
  echo $! > logs/cloudflared.pid
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
  echo $! > logs/tailscaled.pid
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
    echo $! > logs/ts-https-proxy.pid
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


# ─── Filebrowser（單一 Go 執行檔，適合這台面板）────────────────────────────

fb_rand() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 16
    return
  fi
  tr -dc 'a-f0-9' </dev/urandom | head -c 32
  echo
}

fb_ensure_bin() {
  if [[ -x ./filebrowser ]]; then
    echo "[filebrowser] 已有 ./filebrowser"
    return 0
  fi
  echo "[filebrowser] 下載 Filebrowser"
  mkdir -p filebrowser-runtime
  local tgz="$PWD/filebrowser-runtime/fb.tgz" ver
  for ver in 2.32.3 2.32.0 2.31.2; do
    if nc_dl_resume "https://github.com/filebrowser/filebrowser/releases/download/v${ver}/linux-amd64-filebrowser.tar.gz" "$tgz" \
      || nc_dl_resume "https://github.com/filebrowser/filebrowser/releases/latest/download/linux-amd64-filebrowser.tar.gz" "$tgz"; then
      tar -xzf "$tgz" -C filebrowser-runtime
      if [[ -f filebrowser-runtime/filebrowser ]]; then
        mv filebrowser-runtime/filebrowser ./filebrowser
        chmod +x ./filebrowser
        rm -f "$tgz"
        echo "[filebrowser] 下載完成"
        return 0
      fi
    fi
    break
  done
  echo "[filebrowser] 下載失敗"
  return 1
}

fb_init() {
  # 一般使用者（Dashboard SSO 免登入口）可見的範圍，相對於 root（cloud/）。
  # 想改成整個 cloud 就設成 "/"。
  FB_USER_SCOPE="${FB_USER_SCOPE:-/教材}"
  mkdir -p cloud/教材 logs fb-data
  mkdir -p "cloud/${FB_USER_SCOPE#/}" 2>/dev/null || true
  local pass=""
  if [[ -f config/filebrowser-admin.txt ]]; then
    pass="$(tr -d '\r\n' < config/filebrowser-admin.txt)"
  fi
  if [[ -z "$pass" ]]; then
    pass="$(fb_rand)"
    umask 077
    printf '%s\n' "$pass" > config/filebrowser-admin.txt
    echo "[filebrowser] 已產生密碼 → config/filebrowser-admin.txt"
  fi
  if [[ ! -f config/fb-sso.secret ]]; then
    umask 077
    printf '%s\n' "$(fb_rand)$(fb_rand)" > config/fb-sso.secret
    echo "[filebrowser] 已產生 SSO 密鑰 → config/fb-sso.secret"
  fi
  local auth_db="$PWD/fb-data/auth.db"
  local open_db="$PWD/fb-data/open.db"
  local admin_db="$PWD/fb-data/admin.db"

  # 確保資料庫「有使用者」。舊版只看檔案存在就跳過，一旦當初 users add 失敗，
  # 就會永遠卡在「有 DB、沒使用者」→ Filebrowser 登入直接 500，且不會自我修復。
  fb_db_has_user() {
    local db="$1"
    [[ -f "$db" ]] || return 1
    ./filebrowser users ls -d "$db" 2>/dev/null | grep -qE '^[[:space:]]*[0-9]+' 
  }

  fb_prepare_db() {
    local db="$1" method="$2" port="$3" label="$4" mode="$5"
    if [[ ! -f "$db" ]]; then
      echo "[filebrowser] 初始化 $label 資料庫"
      if ! ./filebrowser config init -d "$db" >> logs/filebrowser-init.log 2>&1; then
        echo "[filebrowser] config init 失敗（$label），詳見 logs/filebrowser-init.log"
        return 1
      fi
    fi

    # 免登實例：檔案操作全開（上傳/下載/覆寫/刪除/改名搬移/公開分享），
    # 但不得管理帳號或改全域設定，也不能執行指令。
    # scope 鎖在 /教材：使用者看不到也進不去 cloud/ 底下的其他資料夾。
    local -a perms
    if [[ "$mode" == "files-full" ]]; then
      perms=( --perm.admin=false --perm.create=true --perm.download=true
              --perm.modify=true --perm.delete=true --perm.rename=true
              --perm.share=true --perm.execute=false
              --scope "$FB_USER_SCOPE" )
    elif [[ "$mode" == "admin-full" ]]; then
      # Dashboard 管理員：看得到整個 cloud/，檔案操作全開。
      # 仍保持 perm.admin=false，避免從免登入口新增/刪除 Filebrowser 帳號。
      perms=( --perm.admin=false --perm.create=true --perm.download=true
              --perm.modify=true --perm.delete=true --perm.rename=true
              --perm.share=true --perm.execute=false
              --scope "/" )
    else
      perms=( --perm.admin=true --scope "/" )
    fi

    if ! fb_db_has_user "$db"; then
      echo "[filebrowser] $label 沒有任何使用者，建立 admin"
      if ! ./filebrowser users add admin "$pass" "${perms[@]}" -d "$db" >> logs/filebrowser-init.log 2>&1; then
        ./filebrowser users update admin --password "$pass" -d "$db" >> logs/filebrowser-init.log 2>&1 \
          || echo "[filebrowser] 建立/更新 admin 失敗（$label），詳見 logs/filebrowser-init.log"
      fi
    fi

    # 每次啟動都重新套用權限。只在建立時設定的話，既有的 admin 使用者
    # 會永遠保留舊的完整權限，限制形同虛設。
    ./filebrowser users update admin "${perms[@]}" -d "$db" >> logs/filebrowser-init.log 2>&1 \
      || echo "[filebrowser] 套用權限失敗（$label）"

    ./filebrowser config set -d "$db" \
      --address 127.0.0.1 --port "$port" --root "$PWD/cloud" \
      --locale zh-tw --auth.method "$method" --baseurl "" >> logs/filebrowser-init.log 2>&1 \
      || echo "[filebrowser] config set 失敗（$label）"

    # 全域預設也鎖起來：noauth 模式下新 session 會沿用這組預設值。
    if [[ "$mode" == "files-full" || "$mode" == "admin-full" ]]; then
      local gscope="/"
      [[ "$mode" == "files-full" ]] && gscope="$FB_USER_SCOPE"
      ./filebrowser config set -d "$db" \
        --perm.admin=false --perm.create=true --perm.download=true \
        --perm.modify=true --perm.delete=true --perm.rename=true \
        --perm.share=true --perm.execute=false >> logs/filebrowser-init.log 2>&1 || true
      ./filebrowser config set -d "$db" --signup=false --createUserDir=false \
        >> logs/filebrowser-init.log 2>&1 || true
      # noauth 模式下新 session 會沿用全域預設 scope，必須一併鎖住。
      ./filebrowser config set -d "$db" --scope "$gscope" \
        >> logs/filebrowser-init.log 2>&1 || true
    fi

    if fb_db_has_user "$db"; then
      local shown="/"
      [[ "$mode" == "files-full" ]] && shown="$FB_USER_SCOPE"
      echo "[filebrowser] $label OK（port $port, auth=$method, mode=$mode, scope=$shown）"
    else
      echo "[filebrowser] 警告：$label 仍然沒有使用者，登入會 500"
    fi
  }


  # 密碼版（:26011）＝你的管理入口，保留完整權限
  fb_prepare_db "$auth_db" json 26011 "auth.db" admin
  # 免登版（:26012）＝一般使用者從 Dashboard SSO 進來的入口
  # 檔案操作全開，只擋帳號管理與全域設定
  fb_prepare_db "$open_db" noauth 26012 "open.db" files-full
  # 免登版（:26013）＝Dashboard 管理員，看得到整個 cloud/
  fb_prepare_db "$admin_db" noauth 26013 "admin.db" admin-full
}

fb_write_caddy() {
  local dash_port="${SERVER_PORT:-26000}"
  mkdir -p nextcloud-runtime
  cat > nextcloud-runtime/Caddyfile.fb << EOF
{
    admin off
    auto_https off
    persist_config off
}

:26001 {
    bind 127.0.0.1
    # 只壓縮文字類。教材多半是 pdf/docx/pptx/mp4/zip，這些本身已壓縮，
    # 再 gzip 只是白燒 CPU 又拖慢下載。
    encode {
        zstd
        gzip 5
        match {
            header Content-Type text/*
            header Content-Type application/json*
            header Content-Type application/javascript*
            header Content-Type application/xml*
            header Content-Type image/svg+xml*
        }
    }
    # Filebrowser 的前端是 SPA：一個頁面會拉幾十個 js/css/圖示，
    # 每一個都觸發 forward_auth 就是幾十次到 Dashboard 的往返。
    # 靜態資源不含任何使用者資料，直接放行給密碼實例（同一份前端），
    # 真正要授權的是 /api/* 與頁面路由。
    @fb_static path /static/* /themes/* *.js *.css *.woff2 *.woff *.ttf *.svg *.png *.ico
    handle @fb_static {
        reverse_proxy 127.0.0.1:26011
    }

    forward_auth 127.0.0.1:${dash_port} {
        uri /admin/api/fb-sso-check
        copy_headers X-Fb-Authed X-Fb-Admin
        header_up X-Forwarded-Proto https
    }

    # 三種身分：
    #   X-Fb-Admin: yes  → Dashboard 管理員，整個 cloud/（:26013）
    #   X-Fb-Authed: yes → 一般使用者，只有教材（:26012）
    #   兩者皆無        → 陌生訪客，要密碼（:26011）
    @admin header X-Fb-Admin yes
    @user {
        header X-Fb-Authed yes
        not header X-Fb-Admin yes
    }

    # 免登通道一律不得管理 Filebrowser 帳號或改全域設定，管理員也一樣：
    # 那是伺服器層設定，應該從密碼入口進。
    @fb_admin_api {
        header X-Fb-Authed yes
        path /api/users* /api/settings*
    }
    respond @fb_admin_api 403

    @fb_admin_page {
        header X-Fb-Authed yes
        path /settings/users* /settings/global*
    }
    redir @fb_admin_page /files 302

    handle @admin {
        reverse_proxy 127.0.0.1:26013
    }
    handle @user {
        reverse_proxy 127.0.0.1:26012
    }
    handle {
        reverse_proxy 127.0.0.1:26011
    }
}
EOF
}

fb_write_snippet() {
  echo "[filebrowser] 教材跳轉 https://files.jasoncloudserver.me/files"
  echo "[filebrowser] 從 Dashboard 登入後免密碼；直接開 files. 網址要密碼（config/filebrowser-admin.txt）"
}

start_filebrowser() {
  if [[ ! -f config/filebrowser.enable && "${FILEBROWSER_ENABLE:-0}" != "1" ]]; then
    return 0
  fi
  echo "[ptero-start] 啟動 Filebrowser（SSO 閘道 :26001）"
  (
    echo "===== $(date -Iseconds) filebrowser ====="
    if command -v pkill >/dev/null 2>&1; then
      pkill -f 'caddy run' >/dev/null 2>&1 || true
      pkill -f './filebrowser ' >/dev/null 2>&1 || true
    fi
    fb_ensure_bin || exit 0
    nc_ensure_caddy || true
    # 放一個空檔 config/fb-reset 就重建資料庫（帳號密碼保留在
    # config/filebrowser-admin.txt）。用來救回 500 的殘缺 DB。
    if [[ -f config/fb-reset ]]; then
      echo "[filebrowser] 偵測到 config/fb-reset，重建資料庫"
      rm -f fb-data/auth.db fb-data/open.db fb-data/admin.db
      rm -f config/fb-reset
    fi
    fb_init
    fb_write_caddy
    fb_write_snippet
    echo "[filebrowser] 密碼 :26011 / 使用者免登 :26012 / 管理員免登 :26013 / Caddy :26001"
    nohup ./filebrowser -d "$PWD/fb-data/auth.db" >> logs/filebrowser-auth.log 2>&1 &
    nohup ./filebrowser -d "$PWD/fb-data/open.db" >> logs/filebrowser-open.log 2>&1 &
    nohup ./filebrowser -d "$PWD/fb-data/admin.db" >> logs/filebrowser-admin.log 2>&1 &
    sleep 1
    if [[ -x ./caddy ]]; then
      exec ./caddy run --config "$PWD/nextcloud-runtime/Caddyfile.fb" --adapter caddyfile
    fi
    echo "[filebrowser] 沒有 caddy，改只開密碼版 :26001（無法 SSO）"
    exec ./filebrowser -d "$PWD/fb-data/auth.db" -a 127.0.0.1 -p 26001
  ) >> logs/filebrowser.log 2>&1 &
  disown || true
}

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
  # Cloudflare 送來 Host=files.xxx，不能只寫 http://127.0.0.1
  # php_fastcgi 已內建轉 index.php，不要再加 try_files
  cat > nextcloud-runtime/Caddyfile << EOF
{
    admin off
    auto_https off
    persist_config off
}

:${port} {
    bind 127.0.0.1
    root * ${root}
    encode gzip
    php_fastcgi 127.0.0.1:9001
    file_server
    header {
        X-Content-Type-Options nosniff
        Referrer-Policy no-referrer
        -Server
    }
    log {
        output file ${PWD}/logs/caddy-access.log
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
  nc_write_phe_php_ini
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

nc_ensure_caddy() {
  if [[ -x ./caddy ]] && ./caddy version >/dev/null 2>&1; then
    echo "[nextcloud] 已有 ./caddy"
    return 0
  fi
  echo "[nextcloud] 下載 Caddy"
  local tgz="$PWD/nextcloud-runtime/caddy.tgz"
  local ver
  for ver in 2.10.2 2.9.1 2.8.4; do
    if nc_dl_resume "https://github.com/caddyserver/caddy/releases/download/v${ver}/caddy_${ver}_linux_amd64.tar.gz" "$tgz"; then
      tar -xzf "$tgz" -C nextcloud-runtime caddy 2>/dev/null || tar -xzf "$tgz" -C nextcloud-runtime
      if [[ -f nextcloud-runtime/caddy ]]; then
        mv nextcloud-runtime/caddy ./caddy
        chmod +x ./caddy
        rm -f "$tgz"
        echo "[nextcloud] Caddy OK"
        return 0
      fi
    fi
  done
  echo "[nextcloud] 下載 Caddy 失敗"
  return 1
}

nc_ensure_php_fpm() {
  if [[ -x ./php-fpm ]]; then
    echo "[nextcloud] 已有 ./php-fpm"
    return 0
  fi
  echo "[nextcloud] 下載靜態 PHP-FPM 8.3"
  mkdir -p nextcloud-runtime/fpm-extract
  local ver tgz="$PWD/nextcloud-runtime/php-fpm.tar.gz"
  for ver in 8.3.23 8.3.22 8.3.21 8.3.16; do
    if nc_dl_resume "https://dl.static-php.dev/static-php-cli/common/php-${ver}-fpm-linux-x86_64.tar.gz" "$tgz"; then
      rm -rf nextcloud-runtime/fpm-extract
      mkdir -p nextcloud-runtime/fpm-extract
      tar -xzf "$tgz" -C nextcloud-runtime/fpm-extract
      local found
      found="$(find nextcloud-runtime/fpm-extract -type f -name php-fpm | head -1)"
      if [[ -n "$found" ]]; then
        cp "$found" ./php-fpm
        chmod +x ./php-fpm
        rm -f "$tgz"
        rm -rf nextcloud-runtime/fpm-extract
        echo "[nextcloud] PHP-FPM OK"
        return 0
      fi
    fi
  done
  echo "[nextcloud] 下載 PHP-FPM 失敗"
  return 1
}

nc_write_fpm_conf() {
  mkdir -p nextcloud-runtime
  cat > nextcloud-runtime/php-fpm.conf << EOF
[global]
pid = $PWD/nextcloud-runtime/php-fpm.pid
error_log = $PWD/logs/php-fpm.log
daemonize = no

[www]
listen = 127.0.0.1:9001
listen.allowed_clients = 127.0.0.1
pm = ondemand
pm.max_children = 8
pm.process_idle_timeout = 20s
pm.max_requests = 200
catch_workers_output = yes
php_admin_value[memory_limit] = 512M
php_admin_value[upload_max_filesize] = 512M
php_admin_value[post_max_size] = 512M
php_admin_value[max_execution_time] = 360
php_admin_value[date.timezone] = Asia/Taipei
php_admin_value[auto_prepend_file] = $PWD/nextcloud-runtime/freetype-polyfill.php
EOF
}

nc_already_running() {
  local port="${NEXTCLOUD_PORT:-26001}"
  command -v curl >/dev/null 2>&1 || return 1
  local code body
  code="$(curl -sS -o /tmp/nc-health.body -w '%{http_code}' --max-time 8 "http://127.0.0.1:${port}/" -H "Host: files.jasoncloudserver.me" 2>/dev/null || echo 000)"
  body="$(cat /tmp/nc-health.body 2>/dev/null || true)"
  echo "[nextcloud] health HTTP ${code} bytes ${#body}"
  [[ "$code" == "200" || "$code" == "302" || "$code" == "301" || "$code" == "303" ]] || return 1
  [[ "${#body}" -gt 20 || "$code" == "302" || "$code" == "301" ]]
}

nc_start_server() {
  local port="${NEXTCLOUD_PORT:-26001}"
  if command -v pkill >/dev/null 2>&1; then
    pkill -f 'frankenphp run' >/dev/null 2>&1 || true
    pkill -f 'php-serve.sh' >/dev/null 2>&1 || true
    pkill -f "php -S 127.0.0.1:${port}" >/dev/null 2>&1 || true
    pkill -f 'php-fpm' >/dev/null 2>&1 || true
    pkill -f 'caddy run' >/dev/null 2>&1 || true
  fi
  sleep 1
  nc_write_php_ini
  nc_write_fpm_conf
  nc_write_caddyfile

  local mode="php-s"
  if nc_ensure_php_fpm && nc_ensure_caddy; then
    echo "[nextcloud] 啟動 PHP-FPM"
    nohup ./php-fpm -y "$PWD/nextcloud-runtime/php-fpm.conf" -c "$PWD/nextcloud-runtime/php.ini" \
      >> logs/php-fpm.log 2>&1 &
    echo $! > nextcloud-runtime/php-fpm.pid
    disown || true
    sleep 2
    if [[ -f nextcloud-runtime/php-fpm.pid ]] && kill -0 "$(cat nextcloud-runtime/php-fpm.pid)" 2>/dev/null; then
      echo "[nextcloud] 啟動 Caddy :${port}"
      nohup ./caddy run --config "$PWD/nextcloud-runtime/Caddyfile" --adapter caddyfile \
        >> logs/nextcloud-php.log 2>&1 &
      echo $! > nextcloud-runtime/frankenphp.pid
      disown || true
      mode="fpm-caddy"
    else
      echo "[nextcloud] PHP-FPM 立刻結束，見 logs/php-fpm.log"
    fi
  fi

  if [[ "$mode" != "fpm-caddy" ]]; then
    echo "[nextcloud] 改用 php -S :${port}"
    if ! nc_php_works ./php; then
      nc_ensure_php || return 1
    fi
    nohup ./php -c "$PWD/nextcloud-runtime/php.ini" \
      -S "127.0.0.1:${port}" -t "$PWD/nextcloud" \
      >> logs/nextcloud-php.log 2>&1 &
    echo $! > nextcloud-runtime/frankenphp.pid
    disown || true
  fi

  local i
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    if nc_already_running; then
      echo "[nextcloud] 已聽 127.0.0.1:${port}（有 Nextcloud HTML） mode=${mode}"
      return 0
    fi
    sleep 2
  done
  echo "[nextcloud] 還沒看到登入頁，見 logs/nextcloud-php.log logs/php-fpm.log"
  tail -n 30 logs/nextcloud-php.log 2>/dev/null || true
  tail -n 20 logs/php-fpm.log 2>/dev/null || true
}

nc_disable_apps_in_db() {
  local db
  db="$(find nextcloud-data nextcloud -name 'owncloud.db' -o -name 'nextcloud.db' 2>/dev/null | head -1)"
  [[ -n "$db" && -x ./php ]] || return 0
  ./php -r '
    $db=$argv[1];
    $pdo=new PDO("sqlite:".$db);
    $pdo->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);
    $st=$pdo->prepare("UPDATE oc_appconfig SET configvalue=? WHERE appid IN (\"user_oidc\",\"groupfolders\") AND configkey=\"enabled\"");
    $st->execute(["no"]);
    echo "disabled user_oidc groupfolders\n";
  ' "$db" && echo "[nextcloud] 已停用不相容應用，網站應可再開" || true
}

nc_sideload_apps() {
  echo "[nextcloud] 改從 GitHub 放入應用（不走商店，避免 FPM EOF）"
  mkdir -p nextcloud/apps nextcloud-runtime
  nc_put_app() {
    local name="$1"; shift
    if [[ -f "nextcloud/apps/${name}/appinfo/info.xml" ]]; then
      echo "[nextcloud] 已有 apps/${name}"
      return 0
    fi
    local url tgz="$PWD/nextcloud-runtime/${name}.tgz"
    for url in "$@"; do
      echo "[nextcloud] 下載 ${name} $url"
      rm -f "$tgz"
      if nc_dl_resume "$url" "$tgz"; then
        rm -rf nextcloud-runtime/app-extract
        mkdir -p nextcloud-runtime/app-extract
        if tar -tzf "$tgz" >/dev/null 2>&1 && tar -xzf "$tgz" -C nextcloud-runtime/app-extract; then
          if [[ -d nextcloud-runtime/app-extract/${name} ]]; then
            rm -rf "nextcloud/apps/${name}"
            mv "nextcloud-runtime/app-extract/${name}" "nextcloud/apps/${name}"
          else
            local inner
            inner="$(find nextcloud-runtime/app-extract -maxdepth 2 -name info.xml | head -1)"
            if [[ -n "$inner" ]]; then
              rm -rf "nextcloud/apps/${name}"
              mv "$(dirname "$(dirname "$inner")")" "nextcloud/apps/${name}" 2>/dev/null || mv "$(dirname "$inner")/.." "nextcloud/apps/${name}"
            fi
          fi
        fi
        rm -f "$tgz"
        rm -rf nextcloud-runtime/app-extract
        if [[ -f "nextcloud/apps/${name}/appinfo/info.xml" ]]; then
          echo "[nextcloud] ${name} 檔案就緒"
          return 0
        fi
      fi
    done
    echo "[nextcloud] ${name} 下載失敗"
    return 1
  }

  nc_put_app user_oidc \
    "https://github.com/nextcloud-releases/user_oidc/releases/download/v8.10.1/user_oidc-v8.10.1.tar.gz" \
    "https://github.com/nextcloud-releases/user_oidc/releases/download/v7.2.0/user_oidc-v7.2.0.tar.gz" \
    "https://github.com/nextcloud-releases/user_oidc/releases/download/v6.2.0/user_oidc-v6.2.0.tar.gz"

  nc_put_app groupfolders \
    "https://github.com/nextcloud-releases/groupfolders/releases/download/v19.1.3/groupfolders-v19.1.3.tar.gz" \
    "https://github.com/nextcloud-releases/groupfolders/releases/download/v18.1.2/groupfolders-v18.1.2.tar.gz" \
    "https://github.com/nextcloud-releases/groupfolders/releases/download/v17.0.5/groupfolders-v17.0.5.tar.gz"

  local db
  db="$(find nextcloud-data nextcloud -name 'owncloud.db' -o -name 'nextcloud.db' 2>/dev/null | head -1)"
  if [[ -z "$db" ]]; then
    echo "[nextcloud] 找不到 sqlite，略過寫入 enabled"
    return 0
  fi
  if ! nc_php_works ./php; then
    echo "[nextcloud] 沒有可用 php，略過寫入 enabled"
    return 0
  fi
  ./php -r '
    $db=$argv[1];
    $pdo=new PDO("sqlite:".$db);
    $pdo->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);
    $cols=$pdo->query("PRAGMA table_info(oc_appconfig)")->fetchAll(PDO::FETCH_ASSOC);
    $names=array_map(function($c){return $c["name"];}, $cols);
    foreach (["user_oidc","groupfolders"] as $app) {
      if (in_array("type",$names,true) && in_array("lazy",$names,true)) {
        $st=$pdo->prepare("INSERT INTO oc_appconfig (appid,configkey,configvalue,type,lazy) VALUES (?,?,?,?,?) ON CONFLICT(appid,configkey) DO UPDATE SET configvalue=excluded.configvalue");
        $st->execute([$app,"enabled","yes",2,0]);
      } else {
        $st=$pdo->prepare("INSERT OR REPLACE INTO oc_appconfig (appid,configkey,configvalue) VALUES (?,?,?)");
        $st->execute([$app,"enabled","yes"]);
      }
      echo "enabled $app\n";
    }
  ' "$db" && echo "[nextcloud] 已在資料庫啟用應用" || echo "[nextcloud] 寫資料庫失敗"
}

nc_bootstrap() {
  mkdir -p logs config nextcloud-runtime nextcloud-data
  echo "===== $(date -Iseconds) nextcloud in ptero-start ====="
  nc_load_env
  nc_download_nextcloud || return 0
  nc_ensure_secrets
  nc_install || return 0
  nc_disable_apps_in_db
  nc_configure
  nc_start_server
  echo "[nextcloud] 完成。公開網址：${NEXTCLOUD_PUBLIC_URL:-https://files.jasoncloudserver.me}"
  echo "[nextcloud] Tunnel：files.jasoncloudserver.me → http://127.0.0.1:${NEXTCLOUD_PORT:-26001}"
}

# 清除 Nextcloud 殘留（停用後省磁碟）。找不到就安靜跳過。
purge_nextcloud_leftovers() {
  if command -v pkill >/dev/null 2>&1; then
    pkill -f 'frankenphp' >/dev/null 2>&1 || true
    pkill -f 'nextcloud-runtime/Caddyfile' >/dev/null 2>&1 || true
  fi
  local removed=0 d
  for d in nextcloud nextcloud-data nextcloud-runtime; do
    if [[ -e "$d" ]]; then
      rm -rf -- "$d" && removed=1
    fi
  done
  rm -f -- nextcloud.zip latest.zip frankenphp frankenphp.tmp 2>/dev/null || true
  if [[ "$removed" == "1" ]]; then
    echo "[ptero-start] 已清除 Nextcloud 殘留目錄"
  fi
  return 0
}

start_nextcloud() {
  echo "[ptero-start] Nextcloud 已停用並刪除，略過啟動"
  purge_nextcloud_leftovers
  return 0
  if [[ -f config/filebrowser.enable || "${FILEBROWSER_ENABLE:-0}" == "1" ]]; then
    echo "[ptero-start] 已改用 Filebrowser，略過 Nextcloud"
    return 0
  fi
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

start_filebrowser

# Filebrowser 開啟時，Dashboard 教材自動指向同一包 cloud/
if [[ -f config/filebrowser.enable || "${FILEBROWSER_ENABLE:-0}" == "1" ]]; then
  export TRONCLASS_MATERIALS_DIR="${TRONCLASS_MATERIALS_DIR:-cloud}"
  export TRONCLASS_MATERIALS_BACKEND="${TRONCLASS_MATERIALS_BACKEND:-local}"
  export TRONCLASS_MATERIALS_FRONTEND_URL="${TRONCLASS_MATERIALS_FRONTEND_URL:-https://files.jasoncloudserver.me/files}"
  export TRONCLASS_MATERIALS_FRONTEND_MODE="${TRONCLASS_MATERIALS_FRONTEND_MODE:-redirect}"
  if [[ -f config/fb-sso.secret ]]; then
    export TRONCLASS_FB_SSO_SECRET="$(tr -d '\r\n' < config/fb-sso.secret)"
  fi
  export TRONCLASS_FB_SSO_DOMAIN="${TRONCLASS_FB_SSO_DOMAIN:-.jasoncloudserver.me}"
  echo "[ptero-start] Dashboard 教材 → ${TRONCLASS_MATERIALS_DIR}，雲端按鈕 ${TRONCLASS_MATERIALS_FRONTEND_URL}（mode=${TRONCLASS_MATERIALS_FRONTEND_MODE}）"
fi

start_nextcloud

# 監控由本啟動程序建立的背景服務。Pterodactyl 只會監控啟動腳本的
# 前景程序，因此不能只依靠 nohup；任何 connector/代理死亡都要自動拉起。
watchdog_pid_alive() {
  local file="$1" pid
  [[ -s "$file" ]] || return 1
  pid="$(cat "$file" 2>/dev/null || true)"
  [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null
}

watchdog_start_tunnel() {
  if ! watchdog_pid_alive logs/cloudflared.pid; then
    echo "[watchdog] Cloudflare Tunnel 不存在，重新啟動"
    start_tunnel
  fi
}

watchdog_start_tailscale() {
  if ! watchdog_pid_alive logs/tailscaled.pid; then
    echo "[watchdog] tailscaled 不存在，重新啟動 Tailscale"
    start_tailscale
  fi
}

watchdog_loop() {
  local bin="$1" child="$2"
  while kill -0 "$child" 2>/dev/null; do
    watchdog_start_tunnel
    watchdog_start_tailscale
    if [[ -f logs/ts-https-proxy.pid ]] && ! watchdog_pid_alive logs/ts-https-proxy.pid; then
      echo "[watchdog] Tailscale TLS proxy 已停止；清除 pid，下一次重啟 Tailscale 時重建"
      rm -f logs/ts-https-proxy.pid
    fi
    sleep "${WATCHDOG_INTERVAL_SECS:-15}"
  done
  wait "$child" 2>/dev/null || true
  echo "[watchdog] $bin 已停止，讓 Pterodactyl 重新啟動整個服務"
  exit 1
}

run_bin() {
  chmod +x "$1" 2>/dev/null || true
  echo "[ptero-start] 啟動 $1（watchdog interval=${WATCHDOG_INTERVAL_SECS:-15}s）"
  "$1" &
  local child=$!
  watchdog_loop "$1" "$child" &
  local wd=$!
  wait "$child"
  local rc=$?
  kill "$wd" 2>/dev/null || true
  wait "$wd" 2>/dev/null || true
  return "$rc"
}

# 已編譯過就直接跑。原本只認 FORCE_REBUILD，改了 .rs / static 檔卻忘了設，
# 就會一直跑舊 binary，看起來像「修了沒效」。改成自動比對修改時間。
BIN=./target/release/tronclass_rollcall
if [[ -f "$BIN" && "${FORCE_REBUILD:-0}" != "1" ]]; then
  newer="$(find src Cargo.toml Cargo.lock -newer "$BIN" 2>/dev/null | head -5)"
  if [[ -n "$newer" ]]; then
    echo "[ptero-start] 偵測到原始碼比 binary 新，將重新編譯："
    printf '[ptero-start]   %s\n' $newer
  else
    echo "[ptero-start] binary 已是最新（$(date -r "$BIN" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)）"
    run_bin "$BIN"
    exit $?
  fi
fi

if [[ -f Cargo.toml && -d src ]]; then
  if [[ -f Cargo.lock ]] && grep -q 'reqwest 0.13' Cargo.lock; then
    echo "[ptero-start] 刪除不相容的 Cargo.lock"
    rm -f Cargo.lock
  fi
  # 舊版 materials_webdav 依賴 MaterialsConfig，或檔案根本沒上傳。
  if [[ -f ./apply-fix-compile-purge-nc.sh ]]; then
    _need_fix=0
    [[ -f src/adapters/admin_web/materials_webdav.rs ]] || _need_fix=1
    [[ -f src/adapters/admin_web/oidc.rs ]] || _need_fix=1
    grep -q 'use crate::config::MaterialsConfig' src/adapters/admin_web/materials_webdav.rs 2>/dev/null && _need_fix=1
    grep -q 'use crate::config::OidcConfig' src/adapters/admin_web/oidc.rs 2>/dev/null && _need_fix=1
    if [[ "$_need_fix" == "1" ]]; then
      echo "[ptero-start] 套用相容模組並清除 Nextcloud"
      bash ./apply-fix-compile-purge-nc.sh "$PWD" || true
    fi
  elif [[ -f ./apply-fix-missing-rs-modules.sh ]]; then
    if [[ ! -f src/adapters/admin_web/materials_webdav.rs || ! -f src/adapters/admin_web/oidc.rs || ! -f src/adapters/admin_web/fb_sso.rs ]]; then
      echo "[ptero-start] 補上缺少的 Rust 模組"
      bash ./apply-fix-missing-rs-modules.sh "$PWD" || true
    fi
  fi
  for _need in materials_webdav.rs oidc.rs fb_sso.rs; do
    if [[ ! -f "src/adapters/admin_web/${_need}" ]]; then
      echo "[ptero-start] 缺少 src/adapters/admin_web/${_need}"
      echo "[ptero-start] 請上傳 apply-fix-missing-rs-modules.sh 或該 .rs 後刪 target/release/tronclass_rollcall 再重啟"
    fi
  done
  echo "[ptero-start] cargo build --release --no-default-features"
  export TRONCLASS_BUILD_STAMP="$(date '+%Y-%m-%d %H:%M:%S')"
  if cargo build --release --no-default-features; then
    run_bin ./target/release/tronclass_rollcall
    exit $?
  fi
  echo "[ptero-start] 編譯失敗"
  exit 1
fi

echo "[ptero-start] 找不到 target/release/tronclass_rollcall"
exit 1
