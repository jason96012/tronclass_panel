#!/bin/bash
# Pterodactyl 一鍵：Cloudflare Tunnel + 同容器 Nextcloud + 本程式
# 全部在面板完成，不需要 SSH。只要覆蓋這一個檔。
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

# ─── 測試模式（面板沒有 shell，用這個開關在 console 跑測試）───────────────
# 用法：在面板「Startup」把啟動命令改成 `RUN_MODE=test ./ptero-start.sh`，
#       或在環境變數加 RUN_MODE=test，然後 Restart。跑完看 console 即可。
#       確認後改回原本的 `./ptero-start.sh` 再 Restart 恢復正常服務。
# 預設不帶 ocr feature（本映像無 libstdc++/g++，與下方正式編譯一致）。
if [[ "${RUN_MODE:-}" == "test" ]]; then
  echo "[ptero-start] 測試模式：cargo test --no-default-features"
  cargo test --no-default-features
  echo "[ptero-start] 測試結束，exit code = $?"
  exit 0
fi

export RUST_LOG="${RUST_LOG:-info}"
PORT="${SERVER_PORT:-26000}"
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_INCREMENTAL=1

mkdir -p config logs run

# Keep a consistent, local recovery copy of the Dashboard credential database.
# A missing accounts.db would otherwise be silently recreated by SQLite and
# make every existing Dashboard password look incorrect after a restart.
backup_accounts_database() {
  local db="config/accounts.db"
  local backup_dir="config/backups"
  local backup="$backup_dir/accounts.latest.db"
  mkdir -p "$backup_dir"
  chmod 700 "$backup_dir" 2>/dev/null || true

  if [[ ! -s "$db" && -s "$backup" ]]; then
    echo "[ptero-start] accounts.db 遺失，從本機備份還原 Dashboard 帳號資料庫"
    cp -f "$backup" "$db"
    chmod 600 "$db" 2>/dev/null || true
  fi

  if [[ -s "$db" && -x "$(command -v python3 2>/dev/null || true)" ]]; then
    # sqlite3.Connection.backup creates a consistent snapshot even when WAL is
    # enabled; do not use a raw cp of accounts.db while it is being written.
    if python3 - "$db" "$backup" <<'PY'
import sqlite3, sys
source, target = sys.argv[1], sys.argv[2]
with sqlite3.connect(source) as src, sqlite3.connect(target) as dst:
    src.backup(dst)
PY
    then
      chmod 600 "$backup" 2>/dev/null || true
      echo "[ptero-start] Dashboard 帳號資料庫備份已確認"
    else
      echo "[ptero-start] 警告：accounts.db 備份失敗，將繼續使用原資料庫"
    fi
  elif [[ ! -s "$db" ]]; then
    echo "[ptero-start] 警告：找不到 accounts.db，首次初始化前既有 Dashboard 密碼無法驗證"
  fi
}
backup_accounts_database

# Atomic lock: only one copy of the Pterodactyl startup/supervisor may run.
if ! mkdir run/ptero-start.lock 2>/dev/null; then
  old="$(cat run/ptero-start.lock/pid 2>/dev/null || true)"
  if [[ "$old" =~ ^[0-9]+$ ]] && kill -0 "$old" 2>/dev/null; then
    echo "[ptero-start] 已有啟動程序執行中（PID $old），拒絕重複啟動"
    exit 1
  fi
  rm -rf run/ptero-start.lock
  mkdir run/ptero-start.lock || exit 1
fi
secure_runtime_permissions() {
  # Keep credentials and runtime databases private even when the image starts
  # with permissive defaults. Never change source/binary permissions here.
  umask 077
  chmod 700 config logs run 2>/dev/null || true
  local f
  for f in config/config.toml config/*.db config/*.db-* config/*.token config/*.env \
           config/*secret* config/*-password.txt config/filebrowser-admin.txt; do
    [[ -f "$f" ]] && chmod 600 "$f"
  done
  for f in config/accounts.db config/config.toml; do
    [[ -f "$f" ]] && chmod 600 "$f"
  done
  find logs -maxdepth 1 -type f -exec chmod 600 {} + 2>/dev/null || true
}
secure_runtime_permissions

printf '%s\n' "$$" > run/ptero-start.lock/pid
cleanup_start_lock() { rm -rf run/ptero-start.lock; }
# TERM must terminate this shell after releasing the lock; otherwise the shell
# may continue waiting on a child and leave run/ptero-start.lock behind.
trap 'cleanup_start_lock; exit 143' TERM
trap 'cleanup_start_lock; exit 130' INT
trap 'cleanup_start_lock' EXIT

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

# Resumable downloader used by the optional Filebrowser/Caddy helpers.
nc_dl_resume() {
  local url="$1" out="$2"
  mkdir -p "$(dirname "$out")"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 5 --retry-all-errors -C - --output "$out" "$url" && return 0
    rm -f "$out"
    curl -fL --retry 3 --output "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -c -O "$out" "$url" && return 0
  fi
  return 1
}

# Defined before start_tunnel (which calls it) so bash finds it; the watchdog
# block later re-defines the same body harmlessly.
cloudflare_validate_config() {
  local token="$1"
  [[ "$token" == eyJ* ]] || { echo "[watchdog] Cloudflare token 格式異常" >> logs/cloudflared-watchdog.log; return 1; }
  [[ "${#token}" -ge 40 ]] || return 1
  return 0
}

start_tunnel() {
  local token
  token="$(read_token)"
  if [[ -z "$token" ]]; then
    echo "[ptero-start] 沒有 Tunnel token。請新增檔案 config/cf.token（內容只有 Cloudflare 複製的 token）"
    return 0
  fi
  if ! cloudflare_validate_config "$token"; then
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

nc_ensure_caddy() {
  if [[ -x ./caddy ]]; then
    echo "[filebrowser] 已有 ./caddy"
    return 0
  fi
  echo "[filebrowser] 下載 Caddy"
  if nc_dl_resume "https://caddyserver.com/api/download?os=linux&arch=amd64" ./caddy; then
    chmod +x ./caddy
    echo "[filebrowser] Caddy 下載完成"
    return 0
  fi
  rm -f ./caddy
  echo "[filebrowser] Caddy 下載失敗"
  return 1
}

fb_init() {
  # 一般使用者（Dashboard SSO 免登入口）可見的範圍，相對於 root（cloud/）。
  # 想改成整個 cloud 就設成 "/"。
  FB_USER_SCOPE="${FB_USER_SCOPE:-/教材}"
  mkdir -p cloud/教材 logs fb-data
  # Keep the storage and Filebrowser databases private to the Pterodactyl
  # service user. Filebrowser and the Rust process run as that same user.
  chmod 700 cloud fb-data 2>/dev/null || true
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

    # 免登實例：一般使用者的完整檔案操作仍鎖在 /教材；管理員實例
    # 才能看到整個 cloud/。兩者都不能執行指令或管理 Filebrowser 帳號。
    # scope 鎖在 /教材：使用者看不到也進不去 cloud/ 底下的其他資料夾。
    local -a perms
    if [[ "$mode" == "files-full" ]]; then
      # Full file operations are allowed only inside the configured scope.
      perms=( --perm.admin=false --perm.create=true --perm.download=true
              --perm.modify=true --perm.delete=true --perm.rename=true
              --perm.share=true --perm.execute=false
              --scope "$FB_USER_SCOPE" )
    elif [[ "$mode" == "files-upload" ]]; then
      # Optional upload-only mode: SSO users may add materials, but must not
      # alter or delete shared files. Enable with FILEBROWSER_USER_MODE=upload.
      perms=( --perm.admin=false --perm.create=true --perm.download=true
              --perm.modify=false --perm.delete=false --perm.rename=false
              --perm.share=false --perm.execute=false
              --scope "$FB_USER_SCOPE" )
    elif [[ "$mode" == "files-readonly" ]]; then
      # Secure default for untrusted students: download only.
      perms=( --perm.admin=false --perm.create=false --perm.download=true
              --perm.modify=false --perm.delete=false --perm.rename=false
              --perm.share=false --perm.execute=false
              --scope "$FB_USER_SCOPE" )
    elif [[ "$mode" == "admin-full" ]]; then
      # Dashboard 管理員：看得到整個 cloud/，並恢復 Filebrowser 管理權限。
      perms=( --perm.admin=true --perm.create=true --perm.download=true
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
      --locale zh-tw --auth.method "$method" --baseurl "" --branding.files "" >> logs/filebrowser-init.log 2>&1 \
      || echo "[filebrowser] config set 失敗（$label）"

    # 全域預設也鎖起來：noauth 模式下新 session 會沿用這組預設值。
    if [[ "$mode" == "files-full" || "$mode" == "admin-full" || "$mode" == "files-upload" || "$mode" == "files-readonly" ]]; then
      local gscope="/"
      [[ "$mode" == "files-full" || "$mode" == "files-upload" || "$mode" == "files-readonly" ]] && gscope="$FB_USER_SCOPE"
      if [[ "$mode" == "files-upload" ]]; then
        ./filebrowser config set -d "$db" \
          --perm.admin=false --perm.create=true --perm.download=true \
          --perm.modify=false --perm.delete=false --perm.rename=false \
          --perm.share=false --perm.execute=false >> logs/filebrowser-init.log 2>&1 || true
      elif [[ "$mode" == "files-readonly" ]]; then
        ./filebrowser config set -d "$db" \
          --perm.admin=false --perm.create=false --perm.download=true \
          --perm.modify=false --perm.delete=false --perm.rename=false \
          --perm.share=false --perm.execute=false >> logs/filebrowser-init.log 2>&1 || true
      else
        ./filebrowser config set -d "$db" \
          --perm.admin=false --perm.create=true --perm.download=true \
          --perm.modify=true --perm.delete=true --perm.rename=true \
          --perm.share=true --perm.execute=false >> logs/filebrowser-init.log 2>&1 || true
      fi
      ./filebrowser config set -d "$db" --signup=false --createUserDir=false \
        >> logs/filebrowser-init.log 2>&1 || true
      # noauth 模式下新 session 會沿用全域預設值，必須一併鎖住 scope。
      ./filebrowser config set -d "$db" --scope "$gscope" \
        >> logs/filebrowser-init.log 2>&1 || true
    fi

    if fb_db_has_user "$db"; then
      local shown="/"
      [[ "$mode" == "files-full" || "$mode" == "files-upload" || "$mode" == "files-readonly" ]] && shown="$FB_USER_SCOPE"
      echo "[filebrowser] $label OK（port $port, auth=$method, mode=$mode, scope=$shown）"
    else
      echo "[filebrowser] 警告：$label 仍然沒有使用者，登入會 500"
    fi
  }


  # 密碼版（:26101）＝內部備援入口，保留完整權限
  fb_prepare_db "$auth_db" json 26101 "auth.db" files-full
  # Existing password users keep their own scope; update the dedicated low-
  # privilege test user explicitly so it cannot retain the old `/` scope.
  local low_user="${FILEBROWSER_LOW_PRIV_USER:-test}"
  if [[ -n "$low_user" ]]; then
    ./filebrowser users update "$low_user" \
      --scope "/教材" --perm.admin=false --perm.create=true --perm.download=true \
      --perm.modify=false --perm.delete=false --perm.rename=false --perm.share=false \
      --perm.execute=false -d "$auth_db" >> logs/filebrowser-init.log 2>&1 \
      || echo "[filebrowser] 低權限帳號 $low_user 不存在或權限更新失敗" >> logs/filebrowser-init.log
  fi
  # 免登版（:26102）＝一般使用者從 Dashboard SSO 進來的入口。
  # 一般使用者可在 /教材 內下載、上傳、修改、刪除、改名及公開分享，
  # 但永遠看不到 cloud/教材 以外的內容。需要唯讀時可設定
  # FILEBROWSER_USER_MODE=readonly；需要只能上傳時可設定 upload。
  local user_mode="files-full"
  case "${FILEBROWSER_USER_MODE:-full}" in
    readonly) user_mode="files-readonly" ;;
    upload) user_mode="files-upload" ;;
    *) user_mode="files-full" ;;
  esac
  fb_prepare_db "$open_db" noauth 26102 "open.db" "$user_mode"
  # 免登版（:26103）＝Dashboard 管理員，看得到整個 cloud/
  fb_prepare_db "$admin_db" noauth 26103 "admin.db" admin-full
}

fb_write_caddy() {
  local dash_port="${SERVER_PORT:-26000}"
  local dashboard_url="${TRONCLASS_DASHBOARD_URL:-https://dashboard.jasoncloudserver.me/admin/dashboard}"
  local shell_secret="disabled"
  if [[ -f config/fb-sso.secret ]]; then
    shell_secret="$(tr -d '\r\n' < config/fb-sso.secret)"
  fi
  mkdir -p nextcloud-runtime
  chmod 700 nextcloud-runtime 2>/dev/null || true
  cat > nextcloud-runtime/Caddyfile.fb << EOF
{
    admin off
    auto_https off
    persist_config off
}

:26001, :26011, :26012, :26013, :26021 {
    bind 127.0.0.1

    # Filebrowser is public through the Cloudflare Tunnel, so apply a small
    # edge hardening baseline and reject oversized uploads before they reach
    # the Go process.
    header {
        -Server
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Cross-Origin-Opener-Policy "same-origin"
        Cross-Origin-Resource-Policy "same-origin"
        Referrer-Policy "no-referrer"
        Permissions-Policy "camera=(), microphone=(), geolocation=()"
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        # Default CSP covers Caddy-generated 3xx responses too. Rust-generated
        # HTML responses already carry a stronger per-response nonce policy, so
        # this default never overwrites their CSP header.
        ?Content-Security-Policy "default-src 'none'"
    }
    request_body {
        max_size 60MB
    }

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
    @trusted_types_assets path /admin/static/js/dompurify.min.js /admin/static/js/trusted-types-init.js
    handle @trusted_types_assets {
        reverse_proxy 127.0.0.1:${dash_port}
    }

    @fb_static path /static/* /themes/* *.js *.css *.woff2 *.woff *.ttf *.svg *.png *.ico
    handle @fb_static {
        reverse_proxy 127.0.0.1:26101
    }

    # 公開分享頁面不需要 Dashboard session，但先經 Rust bridge
    # 把 Filebrowser 內建的「登入」動作改成 Dashboard 登入。
    @public_share_page path /share/*
    handle @public_share_page {
      reverse_proxy 127.0.0.1:${dash_port} {
        header_up X-Fb-Share-Path {http.request.uri}
        header_up X-Fb-Shell-Secret ${shell_secret}
        header_up X-Forwarded-Proto https
      }
    }
    # 分享實際內容與公開下載 API 仍直接走免登入的 /教材 實例。
    @public_share_api path /api/public/*
    handle @public_share_api {
      reverse_proxy 127.0.0.1:26102
    }


    # Filebrowser's built-in password screen must never be the public entry
    # point. Shared links are handled above; every other login navigation goes
    # back to the Dashboard.
    @fb_builtin_login path /login*
    redir @fb_builtin_login "${dashboard_url}" 302

    forward_auth 127.0.0.1:${dash_port} {
      uri /admin/api/fb-sso-check
      copy_headers X-Fb-Authed X-Fb-Admin
      header_up X-Forwarded-Proto https
    }

    # 三種身分：
    #   X-Fb-Admin: yes  → Dashboard 管理員，整個 cloud/（:26103）
    #   X-Fb-Authed: yes → 一般使用者，只有教材（:26102）
    #   兩者皆無        → 陌生訪客，直接拒絕（內部 Filebrowser 不公開）
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

    # Only the initial Filebrowser HTML shell is sent through the Dashboard
    # so Rust can add the return button. Static assets and all file APIs below
    # continue to use Filebrowser directly.
    @fb_shell_admin {
        path /files /files/
        header X-Fb-Admin yes
    }
    handle @fb_shell_admin {
        rewrite * /admin/api/filebrowser-shell
        reverse_proxy 127.0.0.1:${dash_port} {
            header_up X-Fb-Backend admin
            header_up X-Fb-Shell-Secret ${shell_secret}
            header_up X-Forwarded-Proto https
        }
    }
    @fb_shell_user {
        path /files /files/
        header X-Fb-Authed yes
        not header X-Fb-Admin yes
    }
    handle @fb_shell_user {
        rewrite * /admin/api/filebrowser-shell
        reverse_proxy 127.0.0.1:${dash_port} {
            header_up X-Fb-Backend user
            header_up X-Fb-Shell-Secret ${shell_secret}
            header_up X-Forwarded-Proto https
        }
    }
    handle @admin {
        reverse_proxy 127.0.0.1:26103
    }
    handle @user {
        reverse_proxy 127.0.0.1:26102
    }
    # 未經 Dashboard SSO 的一般檔案頁面直接導向 Dashboard 登入，
    # 不顯示任何 Filebrowser 內建登入或純文字錯誤頁。
    handle {
      redir "${dashboard_url}" 302
    }
}
EOF
  chmod 600 nextcloud-runtime/Caddyfile.fb 2>/dev/null || true
}

fb_write_snippet() {
  echo "[filebrowser] 教材跳轉 https://files.jasoncloudserver.me/files"
  echo "[filebrowser] 只有 Dashboard SSO 可進入；未經 Dashboard 的 files. 登入會被拒絕，公開分享連結除外"
}

start_filebrowser() {
  if [[ ! -f config/filebrowser.enable && "${FILEBROWSER_ENABLE:-0}" != "1" ]]; then
    return 0
  fi
  echo "[ptero-start] 啟動 Filebrowser（SSO 閘道 :26001）"
  (
    echo "===== $(date -Iseconds) filebrowser ====="
    for pidfile in logs/filebrowser-auth.pid logs/filebrowser-open.pid logs/filebrowser-admin.pid logs/filebrowser-caddy.pid; do
      if [[ -s "$pidfile" ]]; then
        pid="$(cat "$pidfile" 2>/dev/null || true)"
        [[ "$pid" =~ ^[0-9]+$ ]] && kill "$pid" 2>/dev/null || true
      fi
      rm -f "$pidfile"
    done
    sleep 1
    fb_ensure_bin || exit 0
    nc_ensure_caddy || true
    # Never fall back to Filebrowser's built-in password screen on the public
    # port. If Caddy cannot be installed, fail closed and keep the files site
    # unavailable until the SSO gateway is available.
    if [[ ! -x ./caddy ]]; then
      echo "[filebrowser] Caddy 不可用，安全模式：不啟動公開 Filebrowser"
      exit 0
    fi
    # 放一個空檔 config/fb-reset 就重建資料庫（帳號密碼保留在
    # config/filebrowser-admin.txt）。用來救回 500 的殘缺 DB。
    if [[ -f config/fb-reset ]]; then
      echo "[filebrowser] 偵測到 config/fb-reset，重建資料庫"
      rm -f fb-data/auth.db fb-data/open.db fb-data/admin.db
      rm -f config/fb-reset
    fi
    fb_init
    fb_write_snippet
    echo "[filebrowser] 內部備援 :26101 / 使用者免登 :26102 / 管理員免登 :26103 / Caddy :26001/26011/26012/26013/26021"
    nohup ./filebrowser -d "$PWD/fb-data/auth.db" >> logs/filebrowser-auth.log 2>&1 &
    echo $! > logs/filebrowser-auth.pid
    nohup ./filebrowser -d "$PWD/fb-data/open.db" >> logs/filebrowser-open.log 2>&1 &
    echo $! > logs/filebrowser-open.pid
    nohup ./filebrowser -d "$PWD/fb-data/admin.db" >> logs/filebrowser-admin.log 2>&1 &
    echo $! > logs/filebrowser-admin.pid
    sleep 1
    fb_write_caddy

    ./caddy run --config "$PWD/nextcloud-runtime/Caddyfile.fb" --adapter caddyfile &
    echo $! > logs/filebrowser-caddy.pid
    wait $!
  ) >> logs/filebrowser.log 2>&1 &
  disown || true
}


start_filebrowser

# At-rest encryption master key for the accounts database. Encrypts upstream
# passwords and session cookies with AES-256-GCM before storing them.
# Resolution order:
#   1. $TRONCLASS_MASTER_KEY (Pterodactyl env var) — highest priority
#   2. config/master.key file — already exists
#   3. auto-generate a fresh key into config/master.key (so no manual step is
#      needed in the Pterodactyl panel)
# Never change the key after accounts exist, or existing encrypted values
# cannot be decrypted.
if [[ -n "${TRONCLASS_MASTER_KEY:-}" ]]; then
  export TRONCLASS_MASTER_KEY
elif [[ -f config/master.key ]]; then
  export TRONCLASS_MASTER_KEY="$(tr -d '\r\n ' < config/master.key)"
else
  # Generate a 32-byte (64 hex) key once, with umask 077 so the file is 0600.
  ( umask 077
    if command -v openssl >/dev/null 2>&1; then
      openssl rand -hex 32 > config/master.key
    else
      tr -dc 'a-f0-9' </dev/urandom | head -c 64 > config/master.key
    fi )
  chmod 600 config/master.key 2>/dev/null || true
  export TRONCLASS_MASTER_KEY="$(tr -d '\r\n ' < config/master.key)"
  echo "[ptero-start] 已自動產生 at-rest 加密 master key → config/master.key（請勿外流或變更）"
fi

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

# 監控由本啟動程序建立的背景服務。Pterodactyl 只會監控啟動腳本的
# 前景程序，因此不能只依靠 nohup；任何 connector/代理死亡都要自動拉起。
rotate_one_log() {
  local file="$1" max_bytes="${LOG_MAX_BYTES:-10485760}" keep="${LOG_ROTATE_KEEP:-5}" size i
  [[ -f "$file" ]] || return 0
  size="$(wc -c < "$file" 2>/dev/null || echo 0)"
  [[ "$size" =~ ^[0-9]+$ ]] || return 0
  (( size < max_bytes )) && return 0
  for ((i=keep-1; i>=1; i--)); do
    [[ -f "${file}.${i}" ]] && mv -f "${file}.${i}" "${file}.$((i+1))"
  done
  cp -f "$file" "${file}.1" 2>/dev/null || true
  # copytruncate keeps the already-open stdout file descriptor valid.
  : > "$file"
  chmod 600 "$file" 2>/dev/null || true
  rm -f "${file}.$((keep+1))"
  echo "[watchdog] rotated $file (${size} bytes)" >> logs/watchdog.log
}

rotate_logs() {
  local file
  for file in logs/*.log; do
    [[ -f "$file" ]] || continue
    rotate_one_log "$file"
  done
}

watchdog_http_ok() {
  local url="$1" code
  command -v curl >/dev/null 2>&1 || return 0
  code="$(curl -L -sS --max-time "${WATCHDOG_HTTP_TIMEOUT_SECS:-5}" -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo 000)"
  # 401/403 still prove the public route reached the application.
  [[ "$code" =~ ^(2[0-9][0-9]|3[0-9][0-9]|401|403)$ ]]
}

watchdog_check_filebrowser_http() {
  [[ -f config/filebrowser.enable || "${FILEBROWSER_ENABLE:-0}" == "1" ]] || return 0
  [[ -x ./caddy ]] || return 0
  local bad=0 port
  # Caddy is the public entry point; the individual services are checked too.
  for port in 26001 26011 26012 26013 26021; do
    if ! watchdog_http_ok "http://127.0.0.1:${port}/"; then bad=1; fi
  done
  local count=0
  count="$(cat logs/filebrowser-http-failures 2>/dev/null || echo 0)"
  if (( bad == 1 )); then
    count=$((count + 1)); printf '%s\n' "$count" > logs/filebrowser-http-failures
    echo "[watchdog] Filebrowser/Caddy HTTP health check 失敗（${count}/3）" >> logs/filebrowser-watchdog.log
    if (( count >= ${WATCHDOG_FAILURE_THRESHOLD:-3} )); then
      watchdog_restart_filebrowser
      printf '0\n' > logs/filebrowser-http-failures
    fi
  else
    rm -f logs/filebrowser-http-failures
  fi
}

watchdog_pid_alive() {
  local file="$1" pid expected cmdline
  [[ -s "$file" ]] || return 1
  pid="$(cat "$file" 2>/dev/null || true)"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  # A reused PID must not be mistaken for our service.
  case "$file" in
    *cloudflared.pid) expected='cloudflared' ;;
    *filebrowser-caddy.pid) expected='filebrowser|caddy' ;;
    *filebrowser-*.pid) expected='filebrowser' ;;
    *dashboard.pid) expected='tronclass_rollcall' ;;
    *) return 0 ;;
  esac
  cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
  [[ "$cmdline" =~ $expected ]]
}

watchdog_restart_tunnel() {
  local now last=0 pid
  now="$(date +%s)"
  last="$(cat logs/cloudflared-watchdog.last 2>/dev/null || echo 0)"
  if (( now - last < ${CLOUDFLARE_RESTART_BACKOFF_SECS:-30} )); then return; fi
  printf '%s\n' "$now" > logs/cloudflared-watchdog.last
  pid="$(cat logs/cloudflared.pid 2>/dev/null || true)"
  [[ "$pid" =~ ^[0-9]+$ ]] && kill "$pid" 2>/dev/null || true
  echo "[watchdog] Cloudflare Tunnel connector 異常，重新啟動（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
  rm -f logs/cloudflared.pid
  start_tunnel
}

watchdog_check_tunnel_connector() {
  watchdog_pid_alive logs/cloudflared.pid || return 0
  local recent public_url
  recent="$(tail -n 80 logs/cloudflared.log 2>/dev/null || true)"
  if printf '%s\n' "$recent" | grep -Eiq 'connection refused|no connection|failed to serve|unable to reach|context deadline exceeded|connection lost|quic connection closed|serve connection error'; then
    watchdog_restart_tunnel
    return
  fi
  public_url="${CLOUDFLARE_PUBLIC_URL:-}"
  if [[ -n "$public_url" ]]; then
    if watchdog_http_ok "$public_url"; then
      echo "[watchdog] Cloudflare public URL OK：$public_url" >> logs/cloudflared-watchdog.log
    else
      echo "[watchdog] Cloudflare public URL health check 失敗：$public_url" >> logs/cloudflared-watchdog.log
      watchdog_restart_tunnel
    fi
  fi
}

watchdog_start_tunnel() {
  if ! watchdog_pid_alive logs/cloudflared.pid; then
    echo "[watchdog] Cloudflare Tunnel 不存在，重新啟動"
    start_tunnel
  fi
}

watchdog_restart_filebrowser() {
  local now last=0
  now="$(date +%s)"
  last="$(cat logs/filebrowser-watchdog.last 2>/dev/null || echo 0)"
  if (( now - last < ${FILEBROWSER_RESTART_BACKOFF_SECS:-30} )); then return; fi
  printf '%s\n' "$now" > logs/filebrowser-watchdog.last
  echo "[watchdog] Filebrowser/Caddy 服務異常，重新啟動（$(date -Iseconds)）" >> logs/filebrowser-watchdog.log
  start_filebrowser
}

watchdog_check_filebrowser() {
  [[ -f config/filebrowser.enable || "${FILEBROWSER_ENABLE:-0}" == "1" ]] || return 0
  [[ -x ./caddy ]] || return 0
  local bad=0 f
  for f in logs/filebrowser-auth.pid logs/filebrowser-open.pid logs/filebrowser-admin.pid logs/filebrowser-caddy.pid; do
    watchdog_pid_alive "$f" || bad=1
  done
  (( bad == 1 )) && watchdog_restart_filebrowser
}


watchdog_loop() {
  local bin="$1" child="$2" started_at now elapsed
  started_at="$(date +%s)"
  while kill -0 "$child" 2>/dev/null; do
    watchdog_start_tunnel
    now="$(date +%s)"
    elapsed=$((now - started_at))
    # Login/initialisation may take longer with many accounts. Do not kill a
    # healthy process merely because its HTTP listener is not ready yet.
    if (( elapsed >= ${WATCHDOG_STARTUP_GRACE_SECS:-180} )); then
      watchdog_check_tunnel_connector
      watchdog_check_filebrowser
      watchdog_check_filebrowser_http
      local dashboard_failures=0
      dashboard_failures="$(cat logs/dashboard-http-failures 2>/dev/null || echo 0)"
      if ! watchdog_http_ok "http://127.0.0.1:${SERVER_PORT:-26000}/healthz"; then
        dashboard_failures=$((dashboard_failures + 1)); printf '%s\n' "$dashboard_failures" > logs/dashboard-http-failures
        echo "[watchdog] Dashboard HTTP health check 失敗（${dashboard_failures}/${WATCHDOG_FAILURE_THRESHOLD:-3}）"
        if (( dashboard_failures >= ${WATCHDOG_FAILURE_THRESHOLD:-3} )); then
          echo "[watchdog] Dashboard 連續健康檢查失敗，交由 Pterodactyl 重啟"
          kill "$child" 2>/dev/null || true
          break
        fi
      else
        rm -f logs/dashboard-http-failures
      fi
    else
      echo "[watchdog] 啟動寬限中 ${elapsed}/${WATCHDOG_STARTUP_GRACE_SECS:-180}s"
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
  echo "$child" > logs/dashboard.pid
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
