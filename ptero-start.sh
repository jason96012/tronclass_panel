#!/bin/bash
# Pterodactyl 一鍵：Cloudflare Tunnel + 同容器 Nextcloud + 本程式
# 全部在面板完成，不需要 SSH。只要覆蓋這一個檔。
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

# ─── TOOLCHAIN_FILE_GUARD_V1：清掉會讓編譯在「進入 rustc 之前」就失敗的檔案 ───────
# 症狀（2026-09-18 實際發生）：
#   info: syncing channel updates for 'stable-x86_64-unknown-linux-gnu'
#   error: could not create temp file /usr/local/rustup/tmp/..._file: Read-only file system (os error 30)
#   → 啟動路徑直接 exit 1（服務離線）；熱重編則是每次重試都失敗、一直維持舊 binary。
#
# 原因：容器裡的 cargo 是 rustup 的 proxy。專案根目錄只要存在 rust-toolchain.toml，
#   rustup 就會去「同步／安裝」它指定的通道與元件（rustfmt、clippy…），
#   這需要寫入 RUSTUP_HOME（本映像為 /usr/local/rustup，**唯讀**）→ 必定失敗。
#   與專案原始碼完全無關，是環境面問題。
#
# 處置：這個檔案不屬於本專案（本專案的編譯參數寫在下方環境變數），殘留時一律清掉，
#   讓 cargo 直接用容器內既有工具鏈編譯。要恢復預設行為就把這一段拿掉即可。
for _tc_file in rust-toolchain.toml rust-toolchain; do
  if [ -f "$_tc_file" ]; then
    echo "[ptero-start] 移除殘留的 $_tc_file（會使 rustup 嘗試寫入唯讀的 RUSTUP_HOME，導致編譯失敗）"
    rm -f "$_tc_file" 2>/dev/null || true
  fi
done
unset _tc_file

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

# ─── PEN_TEST_V1：滲透測試建置（關閉防護，僅供受控測試）──────────────────
# 設 PEN_TEST=1 會讓**所有** cargo build 帶上 `--features pen_test`，效果等同
# 手動 `cargo build --release --no-default-features --features pen_test`。
#
# 面板設定方式：Startup 環境變數加 PEN_TEST=1，或在 Startup 命令前綴
# `PEN_TEST=1 ./ptero-start.sh`，然後 Restart。
#
# 啟用後的行為（詳見 reports/pentest-feature-review-2026-10-27.md）：
#   · WAF 只偵測不封鎖（遙測與 warn! 紀錄完整保留）
#   · 全域 L7 限速跳過；不自動寫入 IP 黑名單
#   · 登入節流 5/20 → 10000；帳號鎖定、密碼變更限速跳過
#   · /pub/* 分享端點限速（120/60s、10/60s）停用
#   · WAF 本體掃描上限 32 MB → 64 MB
#
# 🚨 這是**關閉防護**的建置，嚴禁用於對外服務：任何人都能對任何帳號
#    無限次嘗試密碼。測完務必設回 PEN_TEST=0（或移除該變數）並 Restart。
#
# 注意：首次建置（檔案末尾）與 watchdog 熱重編（hot_rebuild_if_needed）
# **兩邊都會吃到這個值**，所以測試期間熱重編不會讓模式跑掉；
# 但這也代表只要變數還在，每次重啟都還是測試模式。
export PEN_TEST="${PEN_TEST:-0}"
PEN_TEST_FEATURE_ARGS=()
if [[ "$PEN_TEST" == "1" ]]; then
  PEN_TEST_FEATURE_ARGS=(--features pen_test)
  echo "═══════════════════════════════════════════════════════════════"
  echo "🚨 [ptero-start] PEN_TEST=1 —— 滲透測試建置，防護已關閉"
  echo "🚨 WAF 不封鎖 / 不限流 / 不自動封禁 / 帳號鎖定已關閉"
  echo "🚨 嚴禁用於對外服務；測完請設回 PEN_TEST=0 並 Restart"
  echo "═══════════════════════════════════════════════════════════════"
fi

export RUST_LOG="${RUST_LOG:-info}"
PORT="${SERVER_PORT:-26000}"
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_INCREMENTAL=1

# CARGO_AUTO_UPDATE_V1：定期在背景跑 `cargo update`，把 Cargo.lock 裡鎖住的
# 依賴套件版本拉到「Cargo.toml 允許範圍內」的最新版（只在 semver 相容範圍內移動，
# 不會跳 major 版、不會違反 Cargo.toml 寫的版本限制）。用來讓已知的上游安全性
# 修補（例如 rustls / rustls-webpki 這類純函式庫更新）自動跟上，不必每次都手動
# SSH 進容器跑 `cargo update`。
#   CARGO_AUTO_UPDATE=0            關閉此功能，完全不會跑 cargo update。
#   CARGO_AUTO_UPDATE_INTERVAL_SECS 兩次 cargo update 間至少間隔多久（預設 24 小時）。
#   CARGO_AUTO_UPDATE_MAX_FAILURES 這次更新造成連續編譯失敗達此次數就自動回滾
#                                   Cargo.lock 到更新前的版本（預設 3 次）。
#
# CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：`cargo update` 本身不會中斷服務，但一旦真的
# 拉到新版本、Cargo.lock 內容變了，下一輪 watchdog 的熱重編（hot_rebuild_if_needed）
# 就會去編譯並在編完後把正在跑的服務程序短暫重啟一次換上新 binary——這段重啟
# 如果剛好發生在上課時間，會打斷正在監控、正在點名的帳號。改成只在「晚上沒有人
# 在上課」的時段才真的去跑 cargo update：不在時段內時，每輪 watchdog（預設 15 秒）
# 只是很便宜地比對一下目前的小時數，到點了才會真的觸發，不會因此延後 24 小時基準。
#   CARGO_AUTO_UPDATE_REQUIRE_NIGHT=0     關閉夜間限制，恢復「到時間就更新」的舊行為。
#   CARGO_AUTO_UPDATE_WINDOW_START_HOUR   夜間時段開始的小時數，24 小時制（預設 2，凌晨 2 點）。
#   CARGO_AUTO_UPDATE_WINDOW_END_HOUR     夜間時段結束的小時數，24 小時制（預設 6，清晨 6 點）。
#   以容器系統時區為準（本專案其餘排課／休息日邏輯也是用系統本地時間，這裡保持一致）。
export CARGO_AUTO_UPDATE="${CARGO_AUTO_UPDATE:-1}"
export CARGO_AUTO_UPDATE_INTERVAL_SECS="${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}"
export CARGO_AUTO_UPDATE_MAX_FAILURES="${CARGO_AUTO_UPDATE_MAX_FAILURES:-3}"
export CARGO_AUTO_UPDATE_REQUIRE_NIGHT="${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}"
export CARGO_AUTO_UPDATE_WINDOW_START_HOUR="${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}"
export CARGO_AUTO_UPDATE_WINDOW_END_HOUR="${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}"

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
  local token size_before
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
  # CLOUDFLARED_LOG_APPEND_V1：這裡以前是 `> logs/cloudflared.log`（截斷）。
  # watchdog 重啟時舊的 cloudflared 可能還活著且持有 offset 很大的 fd，
  # 檔案一被截斷，舊行程接著在原 offset 寫入就會留下一段 NUL 空洞，
  # 之後 `$(tail ...)` 就會噴「ignored null byte in input」。
  # 改用 >>（O_APPEND）：多個寫入端都只會往檔尾追加，不會產生空洞。
  # 檔案過大時才輪替，而且只在沒有存活的寫入端時才做。
  if [[ -f logs/cloudflared.log ]]; then
    _log_bytes="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
    if [[ "$_log_bytes" =~ ^[0-9]+$ ]] && (( _log_bytes > ${CLOUDFLARE_LOG_MAX_BYTES:-5242880} )); then
      mv -f logs/cloudflared.log logs/cloudflared.log.1 2>/dev/null || true
    fi
  fi
  # TUNNEL_RESTART_LOOP_FIX_V1：啟動前先記下檔尾位置，watchdog 只會檢查這之後的 log。
  size_before="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size_before" =~ ^[0-9]+$ ]] || size_before=0
  printf '%s\n' "$size_before" > logs/cloudflared-watchdog.offset
  rm -f logs/cloudflared-log-failures logs/cloudflared-http-failures
  nohup ./cloudflared tunnel --no-autoupdate run --token "$token" >> logs/cloudflared.log 2>&1 &
  echo $! > logs/cloudflared.pid
  disown || true
  # 初次啟動也套用寬限期（watchdog_check_tunnel_connector 以此時間戳計算）。
  date +%s > logs/cloudflared-watchdog.last
  sleep 2
  echo "[ptero-start] cloudflared 最近 log："
  tail -n 15 logs/cloudflared.log 2>/dev/null | tr -d '\000' || true
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

# STOCKGENIE_BOOT_V1：容器／主機一啟動就拉起智選 sidecar（只聽 127.0.0.1:18765）。
# 面板經 /admin/stock-api 反代；絕不開 0.0.0.0、不佔頂層 /api（那是 iClass）。
# 失敗只記 log，不擋點名／Dashboard。STOCKGENIE_ENABLE=0 可關。
STOCKGENIE_PORT="${STOCKGENIE_PORT:-18765}"
STOCKGENIE_DIR="src/adapters/admin_web/stockgenie"

start_stockgenie() {
  if [[ "${STOCKGENIE_ENABLE:-1}" != "1" ]]; then
    echo "[ptero-start] 智選 sidecar 已關閉（STOCKGENIE_ENABLE=0）"
    return 0
  fi
  if [[ ! -f "$STOCKGENIE_DIR/app.py" ]]; then
    echo "[ptero-start] 找不到 $STOCKGENIE_DIR/app.py，略過智選"
    return 0
  fi
  mkdir -p logs run
  # bootstrap.sh 自己判斷：完整就瞬間結束；缺套件才 pip；缺 python 才下載。
  # STOCKGENIE_PYTHON_BOOTSTRAP_V1／SKIP_IF_PRESENT／PIECEWISE。失敗不擋點名。
  if ! bash "$STOCKGENIE_DIR/bootstrap.sh" >> logs/stockgenie-bootstrap.log 2>&1; then
    echo "[ptero-start] 智選 Python 環境安裝失敗（詳見 logs/stockgenie-bootstrap.log）；面板其餘功能不受影響"
    return 0
  fi
  local py=""
  if [[ -x run/stockgenie-venv/bin/python ]]; then
    py="$PWD/run/stockgenie-venv/bin/python"
  elif [[ -x run/stockgenie-venv/bin/python3 ]]; then
    py="$PWD/run/stockgenie-venv/bin/python3"
  else
    py="$(tr -d '\r\n' < run/stockgenie-python-bin.txt 2>/dev/null || true)"
  fi
  if [[ ! -x "$py" ]]; then
    echo "[ptero-start] 智選 python 仍不存在，略過"
    return 0
  fi
  mkdir -p logs
  local code="000"
  if command -v curl >/dev/null 2>&1; then
    code="$(curl -sS --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${STOCKGENIE_PORT}/api/health" 2>/dev/null || echo 000)"
  fi
  if [[ "$code" == "200" || "$code" == "202" ]]; then
    echo "[ptero-start] 智選 sidecar 已在 127.0.0.1:${STOCKGENIE_PORT}"
    return 0
  fi
  if [[ -s logs/stockgenie.pid ]]; then
    local pid
    pid="$(cat logs/stockgenie.pid 2>/dev/null || true)"
    if [[ "$pid" =~ ^[0-9]+$ ]]; then
      kill "$pid" 2>/dev/null || true
      sleep 0.3
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f logs/stockgenie.pid
  fi
  echo "[ptero-start] 啟動智選 sidecar（127.0.0.1:${STOCKGENIE_PORT}）"
  local sg_root sg_log sg_cache
  sg_root="$PWD/$STOCKGENIE_DIR"
  sg_log="$PWD/logs/stockgenie.log"
  sg_cache="$PWD/run/stockgenie-cache"
  mkdir -p "$sg_cache"
  # nohup + disown：熱重編只殺 Rust binary，智選繼續跑。
  # $4＝可寫 cache（不可用內層 $PWD，cd 進 src 後會指到唯讀目錄）。
  nohup bash -c '
    cd "$1" || exit 1
    exec env PYTHONUNBUFFERED=1 STOCKGENIE_EMBED=1 PYTHONNOUSERSITE=1 \
      DD_TRACE_ENABLED=0 DD_PROFILING_ENABLED=0 STOCKGENIE_CACHE="$4" \
      "$2" -m uvicorn app:app \
      --host 127.0.0.1 --port "$3" --log-level warning
  ' bash "$sg_root" "$py" "$STOCKGENIE_PORT" "$sg_cache" >> "$sg_log" 2>&1 &
  echo $! > logs/stockgenie.pid
  disown || true
}

# STOCKGENIE_BOOT_BG_V1：venv／sidecar 放到背景，不擋面板入口與點名。
# 已有 run/stockgenie-venv 時 start_stockgenie 幾乎立刻結束；首次安裝才會下載。
mkdir -p logs run
echo "[ptero-start] 智選在背景啟動（venv 下載不擋面板）"
start_stockgenie >> logs/stockgenie-boot.log 2>&1 &
echo $! > logs/stockgenie-boot.pid

# At-rest encryption master key for the accounts database. Encrypts upstream
# passwords and session cookies with AES-256-GCM before storing them.
# Resolution order:
#   1. $TRONCLASS_MASTER_KEY (Pterodactyl env var) — highest priority
#   2. config/master.key file — already exists
#   3. auto-generate a fresh key into config/master.key (so no manual step is
#      needed in the Pterodactyl panel)
# Key rotation is supported (MASTER_KEY_ROTATION_V1): set the NEW key above and
# list the OLD key(s) in TRONCLASS_MASTER_KEY_OLD / config/master.old.keys;
# stored secrets are re-encrypted in the background as accounts are loaded.
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
# SEC_MASTER_KEY_FAIL_CLOSED_V1：正式部署一定有 key（上面三條路徑保證），因此讓程式在
# key 缺失／無效時拒絕啟動，避免學校密碼以明文落地（可設 0 關閉，不建議）。
export TRONCLASS_REQUIRE_MASTEMASTER_KEY="${TRONCLASS_REQUIRE_MASTER_KEY:-1}"

# MASTER_KEY_ROTATION_V1：舊 key 僅供解密＋背景重加密（一行一個 64 hex key）。
# 輪替步驟：
#   1. openssl rand -hex 32 產生新 key，寫入 config/master.key（或 TRONCLASS_MASTER_KEY）
#   2. 把舊 key 放進 config/master.old.keys（或 TRONCLASS_MASTER_KEY_OLD，逗號分隔）
#   3. 重啟；log 出現各帳號「master key 輪替：已將儲存密文重加密」即完成
#   4. 刪除 master.old.keys／環境變數後再重啟（舊 key 功成身退）
if [[ -n "${TRONCLASS_MASTER_KEY_OLD:-}" ]]; then
  export TRONCLASS_MASTER_KEY_OLD
elif [[ -f config/master.old.keys ]]; then
  OLD_KEYS="$(grep -vE '^[[:space:]]*(#|$)' config/master.old.keys | tr -d '\r' | paste -sd, -)"
  if [[ -n "$OLD_KEYS" ]]; then
    export TRONCLASS_MASTER_KEY_OLD="$OLD_KEYS"
    echo "[ptero-start] 已載入舊 master key（config/master.old.keys）供輪替解密與背景重加密"
  fi
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

# ── HIYORI_SHOP_V1：日和選物所靜態站（shop.jasoncloudserver.me → 127.0.0.1:26200）──
# 純靜態網站，由 ./shop-static-server.sh 啟動（沿用 ./caddy，與面板 binary 完全無關）。
# 移除商店：把這段與 watchdog_loop 裡的 shop_static_watchdog 那行刪掉即可。
# 網站檔案更新：shop-static-server.sh stop → 換掉 shop/* → shop-static-server.sh start
if [[ -f ./shop-static-server.sh ]]; then
  source ./shop-static-server.sh
  shop_static_start
else
  echo "[shop] 找不到 ./shop-static-server.sh，略過商店靜態站（shop.jasoncloudserver.me 會 502）"
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
  # LOG_DISK_GUARD_V1：若 LOG_ROTATE_KEEP 被調小，把多出來的舊份也清掉。
  for ((i=keep+2; i<=20; i++)); do
    [[ -f "${file}.${i}" ]] && rm -f "${file}.${i}"
  done
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
    *stockgenie.pid) expected='uvicorn|python|bootstrap.sh' ;;
    *dashboard.pid) expected='tronclass_rollcall' ;;
    *) return 0 ;;
  esac
  cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
  [[ "$cmdline" =~ $expected ]]
}

# TUNNEL_RESTART_LOOP_FIX_V1
# 之前的 watchdog 會陷入「無限重啟迴圈」：
#   1. cloudflared.log 是 >> 追加、永不清除；
#   2. 邊緣節點一次正常的瞬斷（connection lost / quic connection closed）被 grep 到 → 重啟；
#   3. 重啟後檔案還是同一份，最後 80 行裡那條錯誤仍在 → 15 秒後再重啟；
#   4. 重啟本身又製造新的 connection lost → 永遠跳不出來。
# 修法：只看「上次重啟之後」新增的 log（記錄 byte offset）、把單次瞬斷與
# 持續失敗分開（要連續 N 次檢查都失敗才重啟）、並把重啟間隔拉長。
watchdog_tunnel_mark_log_offset() {
  local size
  size="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size" =~ ^[0-9]+$ ]] || size=0
  printf '%s\n' "$size" > logs/cloudflared-watchdog.offset
}

# 讀出上次標記之後新寫入的 log（檔案被輪替變小時，從頭讀）。
watchdog_tunnel_new_log() {
  local offset size
  offset="$(cat logs/cloudflared-watchdog.offset 2>/dev/null || echo 0)"
  [[ "$offset" =~ ^[0-9]+$ ]] || offset=0
  size="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size" =~ ^[0-9]+$ ]] || size=0
  (( offset > size )) && offset=0
  tail -c +"$((offset + 1))" logs/cloudflared.log 2>/dev/null | tr -d '\000' || true
}

watchdog_restart_tunnel() {
  local now last=0 pid
  now="$(date +%s)"
  last="$(cat logs/cloudflared-watchdog.last 2>/dev/null || echo 0)"
  if (( now - last < ${CLOUDFLARE_RESTART_BACKOFF_SECS:-120} )); then return; fi
  printf '%s\n' "$now" > logs/cloudflared-watchdog.last
  pid="$(cat logs/cloudflared.pid 2>/dev/null || true)"
  # CLOUDFLARED_LOG_APPEND_V1：一定要等舊行程真的結束再啟新的。
  # 否則會有兩個 connector 同時跑，而且舊的還會繼續寫 log。
  if [[ "$pid" =~ ^[0-9]+$ ]]; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.25
    done
    if kill -0 "$pid" 2>/dev/null; then
      echo "[watchdog] cloudflared $pid 未在 5 秒內結束，強制終止" >> logs/cloudflared-watchdog.log
      kill -9 "$pid" 2>/dev/null || true
      sleep 0.5
    fi
  fi
  echo "[watchdog] Cloudflare Tunnel connector 異常，重新啟動（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
  rm -f logs/cloudflared.pid
  start_tunnel
  # TUNNEL_RESTART_LOOP_FIX_V1：重啟後從「現在的檔尾」開始看，舊錯誤不再算數；
  # 失敗計數歸零，並給新程序一段寬限期（以 last 時間戳計算）。
  watchdog_tunnel_mark_log_offset
  rm -f logs/cloudflared-log-failures logs/cloudflared-http-failures
}

watchdog_check_tunnel_connector() {
  watchdog_pid_alive logs/cloudflared.pid || return 0
  local new_log public_url now last
  # TUNNEL_RESTART_LOOP_FIX_V1：剛（重）啟動的 connector 給它時間完成 4 條 QUIC 連線，
  # 這段期間 log 裡必然有連線建立中的雜訊，不檢查。
  now="$(date +%s)"
  last="$(cat logs/cloudflared-watchdog.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${CLOUDFLARE_STARTUP_GRACE_SECS:-90} )); then
    return 0
  fi
  [[ -f logs/cloudflared-watchdog.offset ]] || watchdog_tunnel_mark_log_offset

  # ── 1) log 判定：只看新寫入的部分，而且要分「還在壞」與「已經自己恢復」──
  # 舊檔案可能已經含有 NUL 空洞，讀取時直接濾掉，避免 bash 一直噴警告。
  new_log="$(watchdog_tunnel_new_log)"
  local log_failures=0
  log_failures="$(cat logs/cloudflared-log-failures 2>/dev/null || echo 0)"
  [[ "$log_failures" =~ ^[0-9]+$ ]] || log_failures=0
  if [[ -n "$new_log" ]]; then
    # 這些是「致命、不會自己好」的訊息：token 失效、tunnel 被刪、被 Cloudflare 拒絕。
    if printf '%s\n' "$new_log" | grep -Eiq 'Unauthorized|invalid tunnel|tunnel not found|failed to serve|Token is invalid|credentials'; then
      echo "[watchdog] cloudflared 出現致命錯誤（設定／token 問題），重新啟動（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
      watchdog_restart_tunnel
      return
    fi
    # 這些是「邊緣節點瞬斷」：cloudflared 自己會在幾秒內重連，看到「Registered tunnel
    # connection」或「Connection ... registered」代表已恢復；只有錯誤持續且沒有恢復訊息
    # 才累計失敗，連續 N 次檢查（預設 4 次 ≈ 1 分鐘）都如此才重啟。
    local has_error=0 has_recovery=0
    printf '%s\n' "$new_log" | grep -Eiq 'connection refused|no connection|unable to reach|context deadline exceeded|connection lost|quic connection closed|serve connection error|failed to dial|Retrying connection' && has_error=1
    printf '%s\n' "$new_log" | grep -Eiq 'Registered tunnel connection|Connection .* registered|Updated to new configuration' && has_recovery=1
    if (( has_error == 1 && has_recovery == 0 )); then
      log_failures=$((log_failures + 1)); printf '%s\n' "$log_failures" > logs/cloudflared-log-failures
      echo "[watchdog] cloudflared 新 log 有連線錯誤且尚未見到重連成功（${log_failures}/${CLOUDFLARE_LOG_FAILURE_THRESHOLD:-4}）" >> logs/cloudflared-watchdog.log
      if (( log_failures >= ${CLOUDFLARE_LOG_FAILURE_THRESHOLD:-4} )); then
        watchdog_restart_tunnel
        return
      fi
    else
      # 沒有錯誤、或已經看到重連成功：視為健康，歸零並把游標往前推，
      # 這樣已經處理過的舊錯誤不會在下一輪再被算一次。
      rm -f logs/cloudflared-log-failures
      watchdog_tunnel_mark_log_offset
    fi
  fi

  # ── 2) 公開網址探測：單次失敗可能只是 Cloudflare 邊緣／DNS 抖動，連續 N 次才重啟 ──
  public_url="${CLOUDFLARE_PUBLIC_URL:-}"
  if [[ -n "$public_url" ]]; then
    local http_failures=0
    http_failures="$(cat logs/cloudflared-http-failures 2>/dev/null || echo 0)"
    [[ "$http_failures" =~ ^[0-9]+$ ]] || http_failures=0
    if watchdog_http_ok "$public_url"; then
      # LOG_DISK_GUARD_V1：只在「剛從失敗恢復」時寫一行，不再每 15 秒寫一次 OK
      #（原本一年會累積約 180 MB 的 OK 行）。
      if [[ -f logs/cloudflared-http-failures ]]; then
        echo "[watchdog] Cloudflare public URL 已恢復：$public_url（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
      fi
      rm -f logs/cloudflared-http-failures
    else
      http_failures=$((http_failures + 1)); printf '%s\n' "$http_failures" > logs/cloudflared-http-failures
      echo "[watchdog] Cloudflare public URL health check 失敗（${http_failures}/${WATCHDOG_FAILURE_THRESHOLD:-3}）：$public_url" >> logs/cloudflared-watchdog.log
      if (( http_failures >= ${WATCHDOG_FAILURE_THRESHOLD:-3} )); then
        watchdog_restart_tunnel
      fi
    fi
  fi
}

watchdog_start_tunnel() {
  if ! watchdog_pid_alive logs/cloudflared.pid; then
    echo "[watchdog] Cloudflare Tunnel 不存在，重新啟動"
    start_tunnel
  fi
}

watchdog_start_stockgenie() {
  [[ "${STOCKGENIE_ENABLE:-1}" == "1" ]] || return 0
  [[ -f src/adapters/admin_web/stockgenie/app.py ]] || return 0
  if watchdog_pid_alive logs/stockgenie.pid; then
    return 0
  fi
  # 首次 venv 安裝中：不要再同步跑一次 bootstrap，以免看門狗卡住數分鐘。
  if [[ -d run/stockgenie-bootstrap.lock ]]; then
    return 0
  fi
  if [[ -s logs/stockgenie-boot.pid ]]; then
    local boot_pid
    boot_pid="$(cat logs/stockgenie-boot.pid 2>/dev/null || true)"
    if [[ "$boot_pid" =~ ^[0-9]+$ ]] && kill -0 "$boot_pid" 2>/dev/null; then
      return 0
    fi
  fi
  local now last=0
  now="$(date +%s)"
  last="$(cat logs/stockgenie-watchdog.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${STOCKGENIE_RESTART_BACKOFF_SECS:-30} )); then
    return 0
  fi
  printf '%s\n' "$now" > logs/stockgenie-watchdog.last
  echo "[watchdog] 智選 sidecar 不存在，重新啟動（$(date -Iseconds)）" >> logs/stockgenie-watchdog.log
  start_stockgenie >> logs/stockgenie-boot.log 2>&1 &
  echo $! > logs/stockgenie-boot.pid
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

# ─── 來源內容 hash：不受 mtime / 時區影響的變更偵測 ──────────────────────
# 為什麼不用「檔案修改時間（mtime）」比對：透過 zip 上傳並解壓時，會保留
# 打包機器的舊時間戳。若打包機器的時鐘比伺服器慢，來源檔 mtime 就會比 binary
# 還舊，導致 find -newer 永遠偵測不到變更（看起來像「修了沒效」）。
# 因此改比對「內容 hash」：任何 .rs / static / Cargo 檔的內容或增刪都會被偵測。
src_hash() {
  local h
  # STOCKGENIE_SRC_HASH_SKIP_V1：智選 Python／HTML 從磁碟讀、不編進 binary。
  # 只換 stockgenie 時不要觸發 cargo（否則每次 overlay zip 都要等數分鐘才能進面板）。
  h="$(find src Cargo.toml Cargo.lock -type f \
        ! -path 'src/adapters/admin_web/stockgenie/*' \
        -print0 2>/dev/null \
        | xargs -0 sha256sum 2>/dev/null \
        | sort \
        | sha256sum \
        | awk '{print $1}')"
  [[ -n "$h" ]] && printf '%s' "$h" || printf 'empty'
}

# CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：「現在是不是晚上沒人上課的時段」——用容器
# 系統本地時間的小時數判斷，跟專案其餘排課／休息日邏輯一致。支援時段跨過午夜
# （例如 START=22、END=6 代表晚上 10 點到隔天早上 6 點）：
#   START <= END（例如 2～6）：hour 落在 [START, END) 之間才算夜間。
#   START >  END（例如 22～6）：hour >= START 或 hour < END 才算夜間。
in_cargo_auto_update_window() {
  local h start end
  h="$(date +%H)"
  h=$((10#$h))
  start="${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}"
  end="${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}"
  if (( start <= end )); then
    (( h >= start && h < end ))
  else
    (( h >= start || h < end ))
  fi
}

# ─── CARGO_AUTO_UPDATE_V1：定期背景跑 cargo update，自動跟上上游安全性修補 ──
# 只在 Cargo.toml 允許的 semver 範圍內移動版本（不會跳 major、不會破壞既有
# 版本限制），等同於請人定期手動跑一次 `cargo update`。真的更新到東西時，
# Cargo.lock 內容會變 → 下一輪 src_hash() 自然偵測到變化、交給既有的
# hot_rebuild_if_needed() 去重編＋換 binary，不另外寫一套重編流程。
# 若這次更新造成連續編譯失敗，自動把 Cargo.lock 還原成更新前的版本，
# 避免服務卡在「舊 binary 一直在跑、但新版怎麼編都失敗」的無限重試迴圈。
cargo_auto_update_if_due() {
  [[ "${CARGO_AUTO_UPDATE:-1}" == "1" ]] || return 1
  [[ -f Cargo.toml ]] || return 1
  command -v cargo >/dev/null 2>&1 || return 1
  local now last
  now="$(date +%s)"
  last="$(cat logs/cargo-auto-update.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400} )); then
    return 1
  fi
  # CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：時間到了但還不是夜間時段——先不更新 `last`，
  # 讓下一輪 watchdog（預設 15 秒後）再檢查一次，一進入夜間時段就會立刻觸發，
  # 不會因為這次「檢查但沒動手」而把 24 小時基準往後延。
  if [[ "${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}" == "1" ]] && ! in_cargo_auto_update_window; then
    return 1
  fi
  printf '%s\n' "$now" > logs/cargo-auto-update.last
  echo "[watchdog] 定期 cargo update：檢查依賴套件是否有相容的新版本"
  cp -f Cargo.lock logs/Cargo.lock.before-auto-update 2>/dev/null || true
  if ! cargo update >> logs/cargo-auto-update.log 2>&1; then
    echo "[watchdog] cargo update 本身失敗（可能是網路問題），還原 Cargo.lock"
    [[ -f logs/Cargo.lock.before-auto-update ]] && cp -f logs/Cargo.lock.before-auto-update Cargo.lock
    return 1
  fi
  if cmp -s Cargo.lock logs/Cargo.lock.before-auto-update 2>/dev/null; then
    echo "[watchdog] cargo update：依賴套件已是允許範圍內的最新版，無變更"
    rm -f logs/Cargo.lock.before-auto-update
    return 1
  fi
  echo "[watchdog] cargo update 已更新 Cargo.lock，詳見 logs/cargo-auto-update.log；將於下一輪熱重編套用"
  rm -f logs/cargo-auto-update-rebuild-failures
  touch logs/cargo-auto-update-pending-verify
  return 0
}

# ─── 熱重編：來源內容有變時，背景重新編譯並替換執行中的程式 ─────────────
# 讓使用者在面板上傳新 src/ 後「不用按 Restart」：看門狗會自動偵測、編譯，
# 成功後短暫重啟服務程序（Pterodactyl 本身不重啟）。設 HOT_REBUILD=0 可關閉。
# 回傳值：0 = 已編譯完成、準備替換；1 = 無需編譯 / 冷卻中 / 編譯失敗。
hot_rebuild_if_needed() {
  local bin="$1"
  [[ "${HOT_REBUILD:-1}" == "1" ]] || return 1
  [[ -f "$bin" && "${FORCE_REBUILD:-0}" != "1" ]] || return 1
  local cur saved
  cur="$(src_hash)"
  saved="$(cat logs/src-hash 2>/dev/null || true)"
  [[ -n "$saved" && "$cur" == "$saved" ]] && return 1
  # 編譯失敗時避免每 15 秒就重試一次（編譯動輒數分鐘），設最小重試間隔。
  local now last
  now="$(date +%s)"
  last="$(cat logs/hot-rebuild.last 2>/dev/null || echo 0)"
  if (( now - last < ${HOT_REBUILD_BACKOFF_SECS:-120} )); then
    return 1
  fi
  printf '%s\n' "$now" > logs/hot-rebuild.last
  echo "[watchdog] 偵測到原始碼內容有變，背景重新編譯（服務持續運作）"
  # HOT_REBUILD_LOW_PRIORITY_V1：release 編譯（lto + codegen-units=1）會吃滿所有 CPU 數分鐘，
  # 同一容器裡正在服務的 Dashboard 會被餓死 → /admin/api/status 幾十秒都回不來、
  # Cloudflare 回 502/524，使用者看到「頁面有殼但數字全是 —」。降低編譯優先權
  # （nice 15；有 ionice 再加磁碟低優先權），並限制平行工作數（預設 nproc-1，至少 1），
  # 讓服務程序永遠搶得到 CPU。可用 HOT_REBUILD_JOBS 覆寫工作數。
  local _jobs _nproc _nice=()
  _nproc="$(nproc 2>/dev/null || echo 2)"
  _jobs="${HOT_REBUILD_JOBS:-$(( _nproc > 1 ? _nproc - 1 : 1 ))}"
  command -v nice >/dev/null 2>&1 && _nice=(nice -n 15)
  command -v ionice >/dev/null 2>&1 && _nice+=(ionice -c 3)
  if ${_nice[@]+"${_nice[@]}"} env CARGO_BUILD_JOBS="$_jobs" cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[@]+"${PEN_TEST_FEATURE_ARGS[@]}"}; then
    printf '%s\n' "$cur" > logs/src-hash
    touch "$bin" 2>/dev/null || true
    touch logs/hot-rebuild.pending
    echo "[watchdog] 編譯完成，即將以新 binary 替換（僅服務程序短暫重啟）"
    # 這次重編成功套用了 cargo_auto_update_if_due() 更新過的 Cargo.lock：
    # 不需要再保留回滾備份，清掉驗證旗標與失敗計數。
    if [[ -f logs/cargo-auto-update-pending-verify ]]; then
      echo "[watchdog] cargo update 的新依賴版本編譯成功，保留使用"
      rm -f logs/cargo-auto-update-pending-verify logs/cargo-auto-update-rebuild-failures logs/Cargo.lock.before-auto-update
    fi
    return 0
  fi
  echo "[watchdog] 編譯失敗，維持舊 binary；上傳修正後的新檔會自動重試"
  # CARGO_AUTO_UPDATE_ROLLBACK_V1：如果這次失敗是 cargo_auto_update_if_due()
  # 更新 Cargo.lock 造成的（而不是使用者自己上傳的 src/ 有問題），連續失敗
  # 達 CARGO_AUTO_UPDATE_MAX_FAILURES 次就自動還原成更新前的 Cargo.lock，
  # 避免每 HOT_REBUILD_BACKOFF_SECS 秒就徒勞重編一次、永遠卡住。
  if [[ -f logs/cargo-auto-update-pending-verify && -f logs/Cargo.lock.before-auto-update ]]; then
    local auf
    auf="$(cat logs/cargo-auto-update-rebuild-failures 2>/dev/null || echo 0)"
    [[ "$auf" =~ ^[0-9]+$ ]] || auf=0
    auf=$((auf + 1))
    printf '%s\n' "$auf" > logs/cargo-auto-update-rebuild-failures
    echo "[watchdog] 本次編譯失敗疑似與自動 cargo update 有關（連續 ${auf}/${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次）"
    if (( auf >= ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} )); then
      echo "[watchdog] 已達上限，自動回滾 Cargo.lock 到 cargo update 之前的版本"
      cp -f logs/Cargo.lock.before-auto-update Cargo.lock
      rm -f logs/cargo-auto-update-pending-verify logs/cargo-auto-update-rebuild-failures logs/Cargo.lock.before-auto-update
      # 讓下一輪 watchdog 用還原後的 Cargo.lock 重新觸發一次編譯。
      rm -f logs/hot-rebuild.last
    fi
  fi
  return 1
}


watchdog_loop() {
  local bin="$1" child="$2" started_at now elapsed
  started_at="$(date +%s)"
  while kill -0 "$child" 2>/dev/null; do
    # LOG_DISK_GUARD_V1：rotate_logs 以前只有定義沒有被呼叫，logs/*.log 會一直長大。
    # 每輪檢查一次大小（每檔一個 wc -c，很便宜），超過 LOG_MAX_BYTES（預設 10 MB）
    # 就 copytruncate 輪替，最多保留 LOG_ROTATE_KEEP（預設 5）份 → 每個 log 上限約 60 MB。
    rotate_logs
    watchdog_start_tunnel
    watchdog_start_stockgenie
    # HIYORI_SHOP_V1：商店靜態站（26200）健康檢查，掛掉自動重啟
    if declare -F shop_static_watchdog >/dev/null 2>&1; then shop_static_watchdog; fi
    cargo_auto_update_if_due || true
    if hot_rebuild_if_needed "$bin"; then
      kill "$child" 2>/dev/null || true
      break
    fi
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
  if [[ -f logs/hot-rebuild.pending ]]; then
    echo "[watchdog] 熱重編流程：交回 run_bin 重啟新 binary"
    exit 0
  fi
  echo "[watchdog] $bin 已停止，讓 Pterodactyl 重新啟動整個服務"
  exit 1
}

run_bin() {
  local bin="$1"
  # 熱重編後在此迴圈內直接重啟新 binary，不需退出讓 Pterodactyl 重啟。
  while true; do
    chmod +x "$bin" 2>/dev/null || true
    echo "[ptero-start] 啟動 $bin（watchdog interval=${WATCHDOG_INTERVAL_SECS:-15}s）"
    if [[ "${HOT_REBUILD:-1}" == "1" ]]; then
      echo "[ptero-start] 熱重編監控：已啟用（每 ${WATCHDOG_INTERVAL_SECS:-15}s 檢查 src/ 是否比 binary 新，新則自動重編、免重啟）"
    else
      echo "[ptero-start] 熱重編監控：已關閉（HOT_REBUILD=0）"
    fi
    if [[ "${CARGO_AUTO_UPDATE:-1}" == "1" ]]; then
      if [[ "${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}" == "1" ]]; then
        echo "[ptero-start] cargo update 自動更新：已啟用（每 ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}s 到期檢查一次，但只會在夜間 ${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}:00-${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}:00 這段沒人上課的時段真的更新＋重編，連續編譯失敗 ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次會自動回滾）"
      else
        echo "[ptero-start] cargo update 自動更新：已啟用（每 ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}s 檢查一次依賴套件，未限制時段，連續編譯失敗 ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次會自動回滾）"
      fi
    else
      echo "[ptero-start] cargo update 自動更新：已關閉（CARGO_AUTO_UPDATE=0）"
    fi
    "$bin" &
    local child=$!
    echo "$child" > logs/dashboard.pid
    watchdog_loop "$bin" "$child" &
    local wd=$!
    wait "$child"
    local rc=$?
    kill "$wd" 2>/dev/null || true
    wait "$wd" 2>/dev/null || true
    if [[ -f logs/hot-rebuild.pending ]]; then
      rm -f logs/hot-rebuild.pending
      echo "[ptero-start] 熱重編完成，重新啟動新 binary"
      continue
    fi
    return "$rc"
  done
}

# 已編譯過就直接跑。原本只認 FORCE_REBUILD，改了 .rs / static 檔卻忘了設，
# 就會一直跑舊 binary，看起來像「修了沒效」。改成自動比對「內容 hash」
# （src_hash），不靠 mtime，避免 zip 上傳保留舊時間戳造成誤判。
BIN=./target/release/tronclass_rollcall
_cur_hash="$(src_hash)"
if [[ -f "$BIN" && "${FORCE_REBUILD:-0}" != "1" ]]; then
  _saved_hash="$(cat logs/src-hash 2>/dev/null || true)"
  if [[ -n "$_saved_hash" && "$_cur_hash" == "$_saved_hash" ]]; then
    echo "[ptero-start] binary 已是最新（$(date -r "$BIN" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)）"
    run_bin "$BIN"
    exit $?
  fi
  if [[ -z "$_saved_hash" ]]; then
    echo "[ptero-start] 尚未有來源 hash 基準，重新編譯一次以建立基準"
  else
    echo "[ptero-start] 偵測到原始碼內容有變，將重新編譯"
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
  echo "[ptero-start] cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[*]:-}"
  export TRONCLASS_BUILD_STAMP="$(date '+%Y-%m-%d %H:%M:%S')"
  if cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[@]+"${PEN_TEST_FEATURE_ARGS[@]}"}; then
    mkdir -p logs
    printf '%s\n' "$_cur_hash" > logs/src-hash
    run_bin ./target/release/tronclass_rollcall
    exit $?
  fi
  echo "[ptero-start] 編譯失敗"
  exit 1
fi

echo "[ptero-start] 找不到 target/release/tronclass_rollcall"
exit 1
#!/bin/bash
# Pterodactyl 一鍵：Cloudflare Tunnel + 同容器 Nextcloud + 本程式
# 全部在面板完成，不需要 SSH。只要覆蓋這一個檔。
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

# ─── TOOLCHAIN_FILE_GUARD_V1：清掉會讓編譯在「進入 rustc 之前」就失敗的檔案 ───────
# 症狀（2026-09-18 實際發生）：
#   info: syncing channel updates for 'stable-x86_64-unknown-linux-gnu'
#   error: could not create temp file /usr/local/rustup/tmp/..._file: Read-only file system (os error 30)
#   → 啟動路徑直接 exit 1（服務離線）；熱重編則是每次重試都失敗、一直維持舊 binary。
#
# 原因：容器裡的 cargo 是 rustup 的 proxy。專案根目錄只要存在 rust-toolchain.toml，
#   rustup 就會去「同步／安裝」它指定的通道與元件（rustfmt、clippy…），
#   這需要寫入 RUSTUP_HOME（本映像為 /usr/local/rustup，**唯讀**）→ 必定失敗。
#   與專案原始碼完全無關，是環境面問題。
#
# 處置：這個檔案不屬於本專案（本專案的編譯參數寫在下方環境變數），殘留時一律清掉，
#   讓 cargo 直接用容器內既有工具鏈編譯。要恢復預設行為就把這一段拿掉即可。
for _tc_file in rust-toolchain.toml rust-toolchain; do
  if [ -f "$_tc_file" ]; then
    echo "[ptero-start] 移除殘留的 $_tc_file（會使 rustup 嘗試寫入唯讀的 RUSTUP_HOME，導致編譯失敗）"
    rm -f "$_tc_file" 2>/dev/null || true
  fi
done
unset _tc_file

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

# ─── PEN_TEST_V1：滲透測試建置（關閉防護，僅供受控測試）──────────────────
# 設 PEN_TEST=1 會讓**所有** cargo build 帶上 `--features pen_test`，效果等同
# 手動 `cargo build --release --no-default-features --features pen_test`。
#
# 面板設定方式：Startup 環境變數加 PEN_TEST=1，或在 Startup 命令前綴
# `PEN_TEST=1 ./ptero-start.sh`，然後 Restart。
#
# 啟用後的行為（詳見 reports/pentest-feature-review-2026-10-27.md）：
#   · WAF 只偵測不封鎖（遙測與 warn! 紀錄完整保留）
#   · 全域 L7 限速跳過；不自動寫入 IP 黑名單
#   · 登入節流 5/20 → 10000；帳號鎖定、密碼變更限速跳過
#   · /pub/* 分享端點限速（120/60s、10/60s）停用
#   · WAF 本體掃描上限 32 MB → 64 MB
#
# 🚨 這是**關閉防護**的建置，嚴禁用於對外服務：任何人都能對任何帳號
#    無限次嘗試密碼。測完務必設回 PEN_TEST=0（或移除該變數）並 Restart。
#
# 注意：首次建置（檔案末尾）與 watchdog 熱重編（hot_rebuild_if_needed）
# **兩邊都會吃到這個值**，所以測試期間熱重編不會讓模式跑掉；
# 但這也代表只要變數還在，每次重啟都還是測試模式。
export PEN_TEST="${PEN_TEST:-0}"
PEN_TEST_FEATURE_ARGS=()
if [[ "$PEN_TEST" == "1" ]]; then
  PEN_TEST_FEATURE_ARGS=(--features pen_test)
  echo "═══════════════════════════════════════════════════════════════"
  echo "🚨 [ptero-start] PEN_TEST=1 —— 滲透測試建置，防護已關閉"
  echo "🚨 WAF 不封鎖 / 不限流 / 不自動封禁 / 帳號鎖定已關閉"
  echo "🚨 嚴禁用於對外服務；測完請設回 PEN_TEST=0 並 Restart"
  echo "═══════════════════════════════════════════════════════════════"
fi

export RUST_LOG="${RUST_LOG:-info}"
PORT="${SERVER_PORT:-26000}"
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_INCREMENTAL=1

# CARGO_AUTO_UPDATE_V1：定期在背景跑 `cargo update`，把 Cargo.lock 裡鎖住的
# 依賴套件版本拉到「Cargo.toml 允許範圍內」的最新版（只在 semver 相容範圍內移動，
# 不會跳 major 版、不會違反 Cargo.toml 寫的版本限制）。用來讓已知的上游安全性
# 修補（例如 rustls / rustls-webpki 這類純函式庫更新）自動跟上，不必每次都手動
# SSH 進容器跑 `cargo update`。
#   CARGO_AUTO_UPDATE=0            關閉此功能，完全不會跑 cargo update。
#   CARGO_AUTO_UPDATE_INTERVAL_SECS 兩次 cargo update 間至少間隔多久（預設 24 小時）。
#   CARGO_AUTO_UPDATE_MAX_FAILURES 這次更新造成連續編譯失敗達此次數就自動回滾
#                                   Cargo.lock 到更新前的版本（預設 3 次）。
#
# CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：`cargo update` 本身不會中斷服務，但一旦真的
# 拉到新版本、Cargo.lock 內容變了，下一輪 watchdog 的熱重編（hot_rebuild_if_needed）
# 就會去編譯並在編完後把正在跑的服務程序短暫重啟一次換上新 binary——這段重啟
# 如果剛好發生在上課時間，會打斷正在監控、正在點名的帳號。改成只在「晚上沒有人
# 在上課」的時段才真的去跑 cargo update：不在時段內時，每輪 watchdog（預設 15 秒）
# 只是很便宜地比對一下目前的小時數，到點了才會真的觸發，不會因此延後 24 小時基準。
#   CARGO_AUTO_UPDATE_REQUIRE_NIGHT=0     關閉夜間限制，恢復「到時間就更新」的舊行為。
#   CARGO_AUTO_UPDATE_WINDOW_START_HOUR   夜間時段開始的小時數，24 小時制（預設 2，凌晨 2 點）。
#   CARGO_AUTO_UPDATE_WINDOW_END_HOUR     夜間時段結束的小時數，24 小時制（預設 6，清晨 6 點）。
#   以容器系統時區為準（本專案其餘排課／休息日邏輯也是用系統本地時間，這裡保持一致）。
export CARGO_AUTO_UPDATE="${CARGO_AUTO_UPDATE:-1}"
export CARGO_AUTO_UPDATE_INTERVAL_SECS="${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}"
export CARGO_AUTO_UPDATE_MAX_FAILURES="${CARGO_AUTO_UPDATE_MAX_FAILURES:-3}"
export CARGO_AUTO_UPDATE_REQUIRE_NIGHT="${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}"
export CARGO_AUTO_UPDATE_WINDOW_START_HOUR="${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}"
export CARGO_AUTO_UPDATE_WINDOW_END_HOUR="${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}"

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
  local token size_before
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
  # CLOUDFLARED_LOG_APPEND_V1：這裡以前是 `> logs/cloudflared.log`（截斷）。
  # watchdog 重啟時舊的 cloudflared 可能還活著且持有 offset 很大的 fd，
  # 檔案一被截斷，舊行程接著在原 offset 寫入就會留下一段 NUL 空洞，
  # 之後 `$(tail ...)` 就會噴「ignored null byte in input」。
  # 改用 >>（O_APPEND）：多個寫入端都只會往檔尾追加，不會產生空洞。
  # 檔案過大時才輪替，而且只在沒有存活的寫入端時才做。
  if [[ -f logs/cloudflared.log ]]; then
    _log_bytes="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
    if [[ "$_log_bytes" =~ ^[0-9]+$ ]] && (( _log_bytes > ${CLOUDFLARE_LOG_MAX_BYTES:-5242880} )); then
      mv -f logs/cloudflared.log logs/cloudflared.log.1 2>/dev/null || true
    fi
  fi
  # TUNNEL_RESTART_LOOP_FIX_V1：啟動前先記下檔尾位置，watchdog 只會檢查這之後的 log。
  size_before="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size_before" =~ ^[0-9]+$ ]] || size_before=0
  printf '%s\n' "$size_before" > logs/cloudflared-watchdog.offset
  rm -f logs/cloudflared-log-failures logs/cloudflared-http-failures
  nohup ./cloudflared tunnel --no-autoupdate run --token "$token" >> logs/cloudflared.log 2>&1 &
  echo $! > logs/cloudflared.pid
  disown || true
  # 初次啟動也套用寬限期（watchdog_check_tunnel_connector 以此時間戳計算）。
  date +%s > logs/cloudflared-watchdog.last
  sleep 2
  echo "[ptero-start] cloudflared 最近 log："
  tail -n 15 logs/cloudflared.log 2>/dev/null | tr -d '\000' || true
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

# STOCKGENIE_BOOT_V1：容器／主機一啟動就拉起智選 sidecar（只聽 127.0.0.1:18765）。
# 面板經 /admin/stock-api 反代；絕不開 0.0.0.0、不佔頂層 /api（那是 iClass）。
# 失敗只記 log，不擋點名／Dashboard。STOCKGENIE_ENABLE=0 可關。
STOCKGENIE_PORT="${STOCKGENIE_PORT:-18765}"
STOCKGENIE_DIR="src/adapters/admin_web/stockgenie"

start_stockgenie() {
  if [[ "${STOCKGENIE_ENABLE:-1}" != "1" ]]; then
    echo "[ptero-start] 智選 sidecar 已關閉（STOCKGENIE_ENABLE=0）"
    return 0
  fi
  if [[ ! -f "$STOCKGENIE_DIR/app.py" ]]; then
    echo "[ptero-start] 找不到 $STOCKGENIE_DIR/app.py，略過智選"
    return 0
  fi
  mkdir -p logs run
  # bootstrap.sh 自己判斷：完整就瞬間結束；缺套件才 pip；缺 python 才下載。
  # STOCKGENIE_PYTHON_BOOTSTRAP_V1／SKIP_IF_PRESENT／PIECEWISE。失敗不擋點名。
  if ! bash "$STOCKGENIE_DIR/bootstrap.sh" >> logs/stockgenie-bootstrap.log 2>&1; then
    echo "[ptero-start] 智選 Python 環境安裝失敗（詳見 logs/stockgenie-bootstrap.log）；面板其餘功能不受影響"
    return 0
  fi
  local py=""
  if [[ -x run/stockgenie-venv/bin/python ]]; then
    py="$PWD/run/stockgenie-venv/bin/python"
  elif [[ -x run/stockgenie-venv/bin/python3 ]]; then
    py="$PWD/run/stockgenie-venv/bin/python3"
  else
    py="$(tr -d '\r\n' < run/stockgenie-python-bin.txt 2>/dev/null || true)"
  fi
  if [[ ! -x "$py" ]]; then
    echo "[ptero-start] 智選 python 仍不存在，略過"
    return 0
  fi
  mkdir -p logs
  local code="000"
  if command -v curl >/dev/null 2>&1; then
    code="$(curl -sS --max-time 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${STOCKGENIE_PORT}/api/health" 2>/dev/null || echo 000)"
  fi
  if [[ "$code" == "200" || "$code" == "202" ]]; then
    echo "[ptero-start] 智選 sidecar 已在 127.0.0.1:${STOCKGENIE_PORT}"
    return 0
  fi
  if [[ -s logs/stockgenie.pid ]]; then
    local pid
    pid="$(cat logs/stockgenie.pid 2>/dev/null || true)"
    if [[ "$pid" =~ ^[0-9]+$ ]]; then
      kill "$pid" 2>/dev/null || true
      sleep 0.3
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f logs/stockgenie.pid
  fi
  echo "[ptero-start] 啟動智選 sidecar（127.0.0.1:${STOCKGENIE_PORT}）"
  local sg_root sg_log sg_cache
  sg_root="$PWD/$STOCKGENIE_DIR"
  sg_log="$PWD/logs/stockgenie.log"
  sg_cache="$PWD/run/stockgenie-cache"
  mkdir -p "$sg_cache"
  # nohup + disown：熱重編只殺 Rust binary，智選繼續跑。
  # $4＝可寫 cache（不可用內層 $PWD，cd 進 src 後會指到唯讀目錄）。
  nohup bash -c '
    cd "$1" || exit 1
    exec env PYTHONUNBUFFERED=1 STOCKGENIE_EMBED=1 PYTHONNOUSERSITE=1 \
      DD_TRACE_ENABLED=0 DD_PROFILING_ENABLED=0 STOCKGENIE_CACHE="$4" \
      "$2" -m uvicorn app:app \
      --host 127.0.0.1 --port "$3" --log-level warning
  ' bash "$sg_root" "$py" "$STOCKGENIE_PORT" "$sg_cache" >> "$sg_log" 2>&1 &
  echo $! > logs/stockgenie.pid
  disown || true
}

# STOCKGENIE_BOOT_BG_V1：venv／sidecar 放到背景，不擋面板入口與點名。
# 已有 run/stockgenie-venv 時 start_stockgenie 幾乎立刻結束；首次安裝才會下載。
mkdir -p logs run
echo "[ptero-start] 智選在背景啟動（venv 下載不擋面板）"
start_stockgenie >> logs/stockgenie-boot.log 2>&1 &
echo $! > logs/stockgenie-boot.pid

# At-rest encryption master key for the accounts database. Encrypts upstream
# passwords and session cookies with AES-256-GCM before storing them.
# Resolution order:
#   1. $TRONCLASS_MASTER_KEY (Pterodactyl env var) — highest priority
#   2. config/master.key file — already exists
#   3. auto-generate a fresh key into config/master.key (so no manual step is
#      needed in the Pterodactyl panel)
# Key rotation is supported (MASTER_KEY_ROTATION_V1): set the NEW key above and
# list the OLD key(s) in TRONCLASS_MASTER_KEY_OLD / config/master.old.keys;
# stored secrets are re-encrypted in the background as accounts are loaded.
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
# SEC_MASTER_KEY_FAIL_CLOSED_V1：正式部署一定有 key（上面三條路徑保證），因此讓程式在
# key 缺失／無效時拒絕啟動，避免學校密碼以明文落地（可設 0 關閉，不建議）。
export TRONCLASS_REQUIRE_MASTEMASTER_KEY="${TRONCLASS_REQUIRE_MASTER_KEY:-1}"

# MASTER_KEY_ROTATION_V1：舊 key 僅供解密＋背景重加密（一行一個 64 hex key）。
# 輪替步驟：
#   1. openssl rand -hex 32 產生新 key，寫入 config/master.key（或 TRONCLASS_MASTER_KEY）
#   2. 把舊 key 放進 config/master.old.keys（或 TRONCLASS_MASTER_KEY_OLD，逗號分隔）
#   3. 重啟；log 出現各帳號「master key 輪替：已將儲存密文重加密」即完成
#   4. 刪除 master.old.keys／環境變數後再重啟（舊 key 功成身退）
if [[ -n "${TRONCLASS_MASTER_KEY_OLD:-}" ]]; then
  export TRONCLASS_MASTER_KEY_OLD
elif [[ -f config/master.old.keys ]]; then
  OLD_KEYS="$(grep -vE '^[[:space:]]*(#|$)' config/master.old.keys | tr -d '\r' | paste -sd, -)"
  if [[ -n "$OLD_KEYS" ]]; then
    export TRONCLASS_MASTER_KEY_OLD="$OLD_KEYS"
    echo "[ptero-start] 已載入舊 master key（config/master.old.keys）供輪替解密與背景重加密"
  fi
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

# ── HIYORI_SHOP_V1：日和選物所靜態站（shop.jasoncloudserver.me → 127.0.0.1:26200）──
# 純靜態網站，由 ./shop-static-server.sh 啟動（沿用 ./caddy，與面板 binary 完全無關）。
# 移除商店：把這段與 watchdog_loop 裡的 shop_static_watchdog 那行刪掉即可。
# 網站檔案更新：shop-static-server.sh stop → 換掉 shop/* → shop-static-server.sh start
if [[ -f ./shop-static-server.sh ]]; then
  source ./shop-static-server.sh
  shop_static_start
else
  echo "[shop] 找不到 ./shop-static-server.sh，略過商店靜態站（shop.jasoncloudserver.me 會 502）"
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
  # LOG_DISK_GUARD_V1：若 LOG_ROTATE_KEEP 被調小，把多出來的舊份也清掉。
  for ((i=keep+2; i<=20; i++)); do
    [[ -f "${file}.${i}" ]] && rm -f "${file}.${i}"
  done
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
    *stockgenie.pid) expected='uvicorn|python|bootstrap.sh' ;;
    *dashboard.pid) expected='tronclass_rollcall' ;;
    *) return 0 ;;
  esac
  cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
  [[ "$cmdline" =~ $expected ]]
}

# TUNNEL_RESTART_LOOP_FIX_V1
# 之前的 watchdog 會陷入「無限重啟迴圈」：
#   1. cloudflared.log 是 >> 追加、永不清除；
#   2. 邊緣節點一次正常的瞬斷（connection lost / quic connection closed）被 grep 到 → 重啟；
#   3. 重啟後檔案還是同一份，最後 80 行裡那條錯誤仍在 → 15 秒後再重啟；
#   4. 重啟本身又製造新的 connection lost → 永遠跳不出來。
# 修法：只看「上次重啟之後」新增的 log（記錄 byte offset）、把單次瞬斷與
# 持續失敗分開（要連續 N 次檢查都失敗才重啟）、並把重啟間隔拉長。
watchdog_tunnel_mark_log_offset() {
  local size
  size="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size" =~ ^[0-9]+$ ]] || size=0
  printf '%s\n' "$size" > logs/cloudflared-watchdog.offset
}

# 讀出上次標記之後新寫入的 log（檔案被輪替變小時，從頭讀）。
watchdog_tunnel_new_log() {
  local offset size
  offset="$(cat logs/cloudflared-watchdog.offset 2>/dev/null || echo 0)"
  [[ "$offset" =~ ^[0-9]+$ ]] || offset=0
  size="$(stat -c%s logs/cloudflared.log 2>/dev/null || echo 0)"
  [[ "$size" =~ ^[0-9]+$ ]] || size=0
  (( offset > size )) && offset=0
  tail -c +"$((offset + 1))" logs/cloudflared.log 2>/dev/null | tr -d '\000' || true
}

watchdog_restart_tunnel() {
  local now last=0 pid
  now="$(date +%s)"
  last="$(cat logs/cloudflared-watchdog.last 2>/dev/null || echo 0)"
  if (( now - last < ${CLOUDFLARE_RESTART_BACKOFF_SECS:-120} )); then return; fi
  printf '%s\n' "$now" > logs/cloudflared-watchdog.last
  pid="$(cat logs/cloudflared.pid 2>/dev/null || true)"
  # CLOUDFLARED_LOG_APPEND_V1：一定要等舊行程真的結束再啟新的。
  # 否則會有兩個 connector 同時跑，而且舊的還會繼續寫 log。
  if [[ "$pid" =~ ^[0-9]+$ ]]; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.25
    done
    if kill -0 "$pid" 2>/dev/null; then
      echo "[watchdog] cloudflared $pid 未在 5 秒內結束，強制終止" >> logs/cloudflared-watchdog.log
      kill -9 "$pid" 2>/dev/null || true
      sleep 0.5
    fi
  fi
  echo "[watchdog] Cloudflare Tunnel connector 異常，重新啟動（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
  rm -f logs/cloudflared.pid
  start_tunnel
  # TUNNEL_RESTART_LOOP_FIX_V1：重啟後從「現在的檔尾」開始看，舊錯誤不再算數；
  # 失敗計數歸零，並給新程序一段寬限期（以 last 時間戳計算）。
  watchdog_tunnel_mark_log_offset
  rm -f logs/cloudflared-log-failures logs/cloudflared-http-failures
}

watchdog_check_tunnel_connector() {
  watchdog_pid_alive logs/cloudflared.pid || return 0
  local new_log public_url now last
  # TUNNEL_RESTART_LOOP_FIX_V1：剛（重）啟動的 connector 給它時間完成 4 條 QUIC 連線，
  # 這段期間 log 裡必然有連線建立中的雜訊，不檢查。
  now="$(date +%s)"
  last="$(cat logs/cloudflared-watchdog.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${CLOUDFLARE_STARTUP_GRACE_SECS:-90} )); then
    return 0
  fi
  [[ -f logs/cloudflared-watchdog.offset ]] || watchdog_tunnel_mark_log_offset

  # ── 1) log 判定：只看新寫入的部分，而且要分「還在壞」與「已經自己恢復」──
  # 舊檔案可能已經含有 NUL 空洞，讀取時直接濾掉，避免 bash 一直噴警告。
  new_log="$(watchdog_tunnel_new_log)"
  local log_failures=0
  log_failures="$(cat logs/cloudflared-log-failures 2>/dev/null || echo 0)"
  [[ "$log_failures" =~ ^[0-9]+$ ]] || log_failures=0
  if [[ -n "$new_log" ]]; then
    # 這些是「致命、不會自己好」的訊息：token 失效、tunnel 被刪、被 Cloudflare 拒絕。
    if printf '%s\n' "$new_log" | grep -Eiq 'Unauthorized|invalid tunnel|tunnel not found|failed to serve|Token is invalid|credentials'; then
      echo "[watchdog] cloudflared 出現致命錯誤（設定／token 問題），重新啟動（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
      watchdog_restart_tunnel
      return
    fi
    # 這些是「邊緣節點瞬斷」：cloudflared 自己會在幾秒內重連，看到「Registered tunnel
    # connection」或「Connection ... registered」代表已恢復；只有錯誤持續且沒有恢復訊息
    # 才累計失敗，連續 N 次檢查（預設 4 次 ≈ 1 分鐘）都如此才重啟。
    local has_error=0 has_recovery=0
    printf '%s\n' "$new_log" | grep -Eiq 'connection refused|no connection|unable to reach|context deadline exceeded|connection lost|quic connection closed|serve connection error|failed to dial|Retrying connection' && has_error=1
    printf '%s\n' "$new_log" | grep -Eiq 'Registered tunnel connection|Connection .* registered|Updated to new configuration' && has_recovery=1
    if (( has_error == 1 && has_recovery == 0 )); then
      log_failures=$((log_failures + 1)); printf '%s\n' "$log_failures" > logs/cloudflared-log-failures
      echo "[watchdog] cloudflared 新 log 有連線錯誤且尚未見到重連成功（${log_failures}/${CLOUDFLARE_LOG_FAILURE_THRESHOLD:-4}）" >> logs/cloudflared-watchdog.log
      if (( log_failures >= ${CLOUDFLARE_LOG_FAILURE_THRESHOLD:-4} )); then
        watchdog_restart_tunnel
        return
      fi
    else
      # 沒有錯誤、或已經看到重連成功：視為健康，歸零並把游標往前推，
      # 這樣已經處理過的舊錯誤不會在下一輪再被算一次。
      rm -f logs/cloudflared-log-failures
      watchdog_tunnel_mark_log_offset
    fi
  fi

  # ── 2) 公開網址探測：單次失敗可能只是 Cloudflare 邊緣／DNS 抖動，連續 N 次才重啟 ──
  public_url="${CLOUDFLARE_PUBLIC_URL:-}"
  if [[ -n "$public_url" ]]; then
    local http_failures=0
    http_failures="$(cat logs/cloudflared-http-failures 2>/dev/null || echo 0)"
    [[ "$http_failures" =~ ^[0-9]+$ ]] || http_failures=0
    if watchdog_http_ok "$public_url"; then
      # LOG_DISK_GUARD_V1：只在「剛從失敗恢復」時寫一行，不再每 15 秒寫一次 OK
      #（原本一年會累積約 180 MB 的 OK 行）。
      if [[ -f logs/cloudflared-http-failures ]]; then
        echo "[watchdog] Cloudflare public URL 已恢復：$public_url（$(date -Iseconds)）" >> logs/cloudflared-watchdog.log
      fi
      rm -f logs/cloudflared-http-failures
    else
      http_failures=$((http_failures + 1)); printf '%s\n' "$http_failures" > logs/cloudflared-http-failures
      echo "[watchdog] Cloudflare public URL health check 失敗（${http_failures}/${WATCHDOG_FAILURE_THRESHOLD:-3}）：$public_url" >> logs/cloudflared-watchdog.log
      if (( http_failures >= ${WATCHDOG_FAILURE_THRESHOLD:-3} )); then
        watchdog_restart_tunnel
      fi
    fi
  fi
}

watchdog_start_tunnel() {
  if ! watchdog_pid_alive logs/cloudflared.pid; then
    echo "[watchdog] Cloudflare Tunnel 不存在，重新啟動"
    start_tunnel
  fi
}

watchdog_start_stockgenie() {
  [[ "${STOCKGENIE_ENABLE:-1}" == "1" ]] || return 0
  [[ -f src/adapters/admin_web/stockgenie/app.py ]] || return 0
  if watchdog_pid_alive logs/stockgenie.pid; then
    return 0
  fi
  # 首次 venv 安裝中：不要再同步跑一次 bootstrap，以免看門狗卡住數分鐘。
  if [[ -d run/stockgenie-bootstrap.lock ]]; then
    return 0
  fi
  if [[ -s logs/stockgenie-boot.pid ]]; then
    local boot_pid
    boot_pid="$(cat logs/stockgenie-boot.pid 2>/dev/null || true)"
    if [[ "$boot_pid" =~ ^[0-9]+$ ]] && kill -0 "$boot_pid" 2>/dev/null; then
      return 0
    fi
  fi
  local now last=0
  now="$(date +%s)"
  last="$(cat logs/stockgenie-watchdog.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${STOCKGENIE_RESTART_BACKOFF_SECS:-30} )); then
    return 0
  fi
  printf '%s\n' "$now" > logs/stockgenie-watchdog.last
  echo "[watchdog] 智選 sidecar 不存在，重新啟動（$(date -Iseconds)）" >> logs/stockgenie-watchdog.log
  start_stockgenie >> logs/stockgenie-boot.log 2>&1 &
  echo $! > logs/stockgenie-boot.pid
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

# ─── 來源內容 hash：不受 mtime / 時區影響的變更偵測 ──────────────────────
# 為什麼不用「檔案修改時間（mtime）」比對：透過 zip 上傳並解壓時，會保留
# 打包機器的舊時間戳。若打包機器的時鐘比伺服器慢，來源檔 mtime 就會比 binary
# 還舊，導致 find -newer 永遠偵測不到變更（看起來像「修了沒效」）。
# 因此改比對「內容 hash」：任何 .rs / static / Cargo 檔的內容或增刪都會被偵測。
src_hash() {
  local h
  # STOCKGENIE_SRC_HASH_SKIP_V1：智選 Python／HTML 從磁碟讀、不編進 binary。
  # 只換 stockgenie 時不要觸發 cargo（否則每次 overlay zip 都要等數分鐘才能進面板）。
  h="$(find src Cargo.toml Cargo.lock -type f \
        ! -path 'src/adapters/admin_web/stockgenie/*' \
        -print0 2>/dev/null \
        | xargs -0 sha256sum 2>/dev/null \
        | sort \
        | sha256sum \
        | awk '{print $1}')"
  [[ -n "$h" ]] && printf '%s' "$h" || printf 'empty'
}

# CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：「現在是不是晚上沒人上課的時段」——用容器
# 系統本地時間的小時數判斷，跟專案其餘排課／休息日邏輯一致。支援時段跨過午夜
# （例如 START=22、END=6 代表晚上 10 點到隔天早上 6 點）：
#   START <= END（例如 2～6）：hour 落在 [START, END) 之間才算夜間。
#   START >  END（例如 22～6）：hour >= START 或 hour < END 才算夜間。
in_cargo_auto_update_window() {
  local h start end
  h="$(date +%H)"
  h=$((10#$h))
  start="${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}"
  end="${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}"
  if (( start <= end )); then
    (( h >= start && h < end ))
  else
    (( h >= start || h < end ))
  fi
}

# ─── CARGO_AUTO_UPDATE_V1：定期背景跑 cargo update，自動跟上上游安全性修補 ──
# 只在 Cargo.toml 允許的 semver 範圍內移動版本（不會跳 major、不會破壞既有
# 版本限制），等同於請人定期手動跑一次 `cargo update`。真的更新到東西時，
# Cargo.lock 內容會變 → 下一輪 src_hash() 自然偵測到變化、交給既有的
# hot_rebuild_if_needed() 去重編＋換 binary，不另外寫一套重編流程。
# 若這次更新造成連續編譯失敗，自動把 Cargo.lock 還原成更新前的版本，
# 避免服務卡在「舊 binary 一直在跑、但新版怎麼編都失敗」的無限重試迴圈。
cargo_auto_update_if_due() {
  [[ "${CARGO_AUTO_UPDATE:-1}" == "1" ]] || return 1
  [[ -f Cargo.toml ]] || return 1
  command -v cargo >/dev/null 2>&1 || return 1
  local now last
  now="$(date +%s)"
  last="$(cat logs/cargo-auto-update.last 2>/dev/null || echo 0)"
  [[ "$last" =~ ^[0-9]+$ ]] || last=0
  if (( now - last < ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400} )); then
    return 1
  fi
  # CARGO_AUTO_UPDATE_NIGHT_ONLY_V1：時間到了但還不是夜間時段——先不更新 `last`，
  # 讓下一輪 watchdog（預設 15 秒後）再檢查一次，一進入夜間時段就會立刻觸發，
  # 不會因為這次「檢查但沒動手」而把 24 小時基準往後延。
  if [[ "${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}" == "1" ]] && ! in_cargo_auto_update_window; then
    return 1
  fi
  printf '%s\n' "$now" > logs/cargo-auto-update.last
  echo "[watchdog] 定期 cargo update：檢查依賴套件是否有相容的新版本"
  cp -f Cargo.lock logs/Cargo.lock.before-auto-update 2>/dev/null || true
  if ! cargo update >> logs/cargo-auto-update.log 2>&1; then
    echo "[watchdog] cargo update 本身失敗（可能是網路問題），還原 Cargo.lock"
    [[ -f logs/Cargo.lock.before-auto-update ]] && cp -f logs/Cargo.lock.before-auto-update Cargo.lock
    return 1
  fi
  if cmp -s Cargo.lock logs/Cargo.lock.before-auto-update 2>/dev/null; then
    echo "[watchdog] cargo update：依賴套件已是允許範圍內的最新版，無變更"
    rm -f logs/Cargo.lock.before-auto-update
    return 1
  fi
  echo "[watchdog] cargo update 已更新 Cargo.lock，詳見 logs/cargo-auto-update.log；將於下一輪熱重編套用"
  rm -f logs/cargo-auto-update-rebuild-failures
  touch logs/cargo-auto-update-pending-verify
  return 0
}

# ─── 熱重編：來源內容有變時，背景重新編譯並替換執行中的程式 ─────────────
# 讓使用者在面板上傳新 src/ 後「不用按 Restart」：看門狗會自動偵測、編譯，
# 成功後短暫重啟服務程序（Pterodactyl 本身不重啟）。設 HOT_REBUILD=0 可關閉。
# 回傳值：0 = 已編譯完成、準備替換；1 = 無需編譯 / 冷卻中 / 編譯失敗。
hot_rebuild_if_needed() {
  local bin="$1"
  [[ "${HOT_REBUILD:-1}" == "1" ]] || return 1
  [[ -f "$bin" && "${FORCE_REBUILD:-0}" != "1" ]] || return 1
  local cur saved
  cur="$(src_hash)"
  saved="$(cat logs/src-hash 2>/dev/null || true)"
  [[ -n "$saved" && "$cur" == "$saved" ]] && return 1
  # 編譯失敗時避免每 15 秒就重試一次（編譯動輒數分鐘），設最小重試間隔。
  local now last
  now="$(date +%s)"
  last="$(cat logs/hot-rebuild.last 2>/dev/null || echo 0)"
  if (( now - last < ${HOT_REBUILD_BACKOFF_SECS:-120} )); then
    return 1
  fi
  printf '%s\n' "$now" > logs/hot-rebuild.last
  echo "[watchdog] 偵測到原始碼內容有變，背景重新編譯（服務持續運作）"
  # HOT_REBUILD_LOW_PRIORITY_V1：release 編譯（lto + codegen-units=1）會吃滿所有 CPU 數分鐘，
  # 同一容器裡正在服務的 Dashboard 會被餓死 → /admin/api/status 幾十秒都回不來、
  # Cloudflare 回 502/524，使用者看到「頁面有殼但數字全是 —」。降低編譯優先權
  # （nice 15；有 ionice 再加磁碟低優先權），並限制平行工作數（預設 nproc-1，至少 1），
  # 讓服務程序永遠搶得到 CPU。可用 HOT_REBUILD_JOBS 覆寫工作數。
  local _jobs _nproc _nice=()
  _nproc="$(nproc 2>/dev/null || echo 2)"
  _jobs="${HOT_REBUILD_JOBS:-$(( _nproc > 1 ? _nproc - 1 : 1 ))}"
  command -v nice >/dev/null 2>&1 && _nice=(nice -n 15)
  command -v ionice >/dev/null 2>&1 && _nice+=(ionice -c 3)
  if ${_nice[@]+"${_nice[@]}"} env CARGO_BUILD_JOBS="$_jobs" cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[@]+"${PEN_TEST_FEATURE_ARGS[@]}"}; then
    printf '%s\n' "$cur" > logs/src-hash
    touch "$bin" 2>/dev/null || true
    touch logs/hot-rebuild.pending
    echo "[watchdog] 編譯完成，即將以新 binary 替換（僅服務程序短暫重啟）"
    # 這次重編成功套用了 cargo_auto_update_if_due() 更新過的 Cargo.lock：
    # 不需要再保留回滾備份，清掉驗證旗標與失敗計數。
    if [[ -f logs/cargo-auto-update-pending-verify ]]; then
      echo "[watchdog] cargo update 的新依賴版本編譯成功，保留使用"
      rm -f logs/cargo-auto-update-pending-verify logs/cargo-auto-update-rebuild-failures logs/Cargo.lock.before-auto-update
    fi
    return 0
  fi
  echo "[watchdog] 編譯失敗，維持舊 binary；上傳修正後的新檔會自動重試"
  # CARGO_AUTO_UPDATE_ROLLBACK_V1：如果這次失敗是 cargo_auto_update_if_due()
  # 更新 Cargo.lock 造成的（而不是使用者自己上傳的 src/ 有問題），連續失敗
  # 達 CARGO_AUTO_UPDATE_MAX_FAILURES 次就自動還原成更新前的 Cargo.lock，
  # 避免每 HOT_REBUILD_BACKOFF_SECS 秒就徒勞重編一次、永遠卡住。
  if [[ -f logs/cargo-auto-update-pending-verify && -f logs/Cargo.lock.before-auto-update ]]; then
    local auf
    auf="$(cat logs/cargo-auto-update-rebuild-failures 2>/dev/null || echo 0)"
    [[ "$auf" =~ ^[0-9]+$ ]] || auf=0
    auf=$((auf + 1))
    printf '%s\n' "$auf" > logs/cargo-auto-update-rebuild-failures
    echo "[watchdog] 本次編譯失敗疑似與自動 cargo update 有關（連續 ${auf}/${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次）"
    if (( auf >= ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} )); then
      echo "[watchdog] 已達上限，自動回滾 Cargo.lock 到 cargo update 之前的版本"
      cp -f logs/Cargo.lock.before-auto-update Cargo.lock
      rm -f logs/cargo-auto-update-pending-verify logs/cargo-auto-update-rebuild-failures logs/Cargo.lock.before-auto-update
      # 讓下一輪 watchdog 用還原後的 Cargo.lock 重新觸發一次編譯。
      rm -f logs/hot-rebuild.last
    fi
  fi
  return 1
}


watchdog_loop() {
  local bin="$1" child="$2" started_at now elapsed
  started_at="$(date +%s)"
  while kill -0 "$child" 2>/dev/null; do
    # LOG_DISK_GUARD_V1：rotate_logs 以前只有定義沒有被呼叫，logs/*.log 會一直長大。
    # 每輪檢查一次大小（每檔一個 wc -c，很便宜），超過 LOG_MAX_BYTES（預設 10 MB）
    # 就 copytruncate 輪替，最多保留 LOG_ROTATE_KEEP（預設 5）份 → 每個 log 上限約 60 MB。
    rotate_logs
    watchdog_start_tunnel
    watchdog_start_stockgenie
    # HIYORI_SHOP_V1：商店靜態站（26200）健康檢查，掛掉自動重啟
    if declare -F shop_static_watchdog >/dev/null 2>&1; then shop_static_watchdog; fi
    cargo_auto_update_if_due || true
    if hot_rebuild_if_needed "$bin"; then
      kill "$child" 2>/dev/null || true
      break
    fi
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
  if [[ -f logs/hot-rebuild.pending ]]; then
    echo "[watchdog] 熱重編流程：交回 run_bin 重啟新 binary"
    exit 0
  fi
  echo "[watchdog] $bin 已停止，讓 Pterodactyl 重新啟動整個服務"
  exit 1
}

run_bin() {
  local bin="$1"
  # 熱重編後在此迴圈內直接重啟新 binary，不需退出讓 Pterodactyl 重啟。
  while true; do
    chmod +x "$bin" 2>/dev/null || true
    echo "[ptero-start] 啟動 $bin（watchdog interval=${WATCHDOG_INTERVAL_SECS:-15}s）"
    if [[ "${HOT_REBUILD:-1}" == "1" ]]; then
      echo "[ptero-start] 熱重編監控：已啟用（每 ${WATCHDOG_INTERVAL_SECS:-15}s 檢查 src/ 是否比 binary 新，新則自動重編、免重啟）"
    else
      echo "[ptero-start] 熱重編監控：已關閉（HOT_REBUILD=0）"
    fi
    if [[ "${CARGO_AUTO_UPDATE:-1}" == "1" ]]; then
      if [[ "${CARGO_AUTO_UPDATE_REQUIRE_NIGHT:-1}" == "1" ]]; then
        echo "[ptero-start] cargo update 自動更新：已啟用（每 ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}s 到期檢查一次，但只會在夜間 ${CARGO_AUTO_UPDATE_WINDOW_START_HOUR:-2}:00-${CARGO_AUTO_UPDATE_WINDOW_END_HOUR:-6}:00 這段沒人上課的時段真的更新＋重編，連續編譯失敗 ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次會自動回滾）"
      else
        echo "[ptero-start] cargo update 自動更新：已啟用（每 ${CARGO_AUTO_UPDATE_INTERVAL_SECS:-86400}s 檢查一次依賴套件，未限制時段，連續編譯失敗 ${CARGO_AUTO_UPDATE_MAX_FAILURES:-3} 次會自動回滾）"
      fi
    else
      echo "[ptero-start] cargo update 自動更新：已關閉（CARGO_AUTO_UPDATE=0）"
    fi
    "$bin" &
    local child=$!
    echo "$child" > logs/dashboard.pid
    watchdog_loop "$bin" "$child" &
    local wd=$!
    wait "$child"
    local rc=$?
    kill "$wd" 2>/dev/null || true
    wait "$wd" 2>/dev/null || true
    if [[ -f logs/hot-rebuild.pending ]]; then
      rm -f logs/hot-rebuild.pending
      echo "[ptero-start] 熱重編完成，重新啟動新 binary"
      continue
    fi
    return "$rc"
  done
}

# 已編譯過就直接跑。原本只認 FORCE_REBUILD，改了 .rs / static 檔卻忘了設，
# 就會一直跑舊 binary，看起來像「修了沒效」。改成自動比對「內容 hash」
# （src_hash），不靠 mtime，避免 zip 上傳保留舊時間戳造成誤判。
BIN=./target/release/tronclass_rollcall
_cur_hash="$(src_hash)"
if [[ -f "$BIN" && "${FORCE_REBUILD:-0}" != "1" ]]; then
  _saved_hash="$(cat logs/src-hash 2>/dev/null || true)"
  if [[ -n "$_saved_hash" && "$_cur_hash" == "$_saved_hash" ]]; then
    echo "[ptero-start] binary 已是最新（$(date -r "$BIN" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)）"
    run_bin "$BIN"
    exit $?
  fi
  if [[ -z "$_saved_hash" ]]; then
    echo "[ptero-start] 尚未有來源 hash 基準，重新編譯一次以建立基準"
  else
    echo "[ptero-start] 偵測到原始碼內容有變，將重新編譯"
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
  echo "[ptero-start] cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[*]:-}"
  export TRONCLASS_BUILD_STAMP="$(date '+%Y-%m-%d %H:%M:%S')"
  if cargo build --release --no-default-features ${PEN_TEST_FEATURE_ARGS[@]+"${PEN_TEST_FEATURE_ARGS[@]}"}; then
    mkdir -p logs
    printf '%s\n' "$_cur_hash" > logs/src-hash
    run_bin ./target/release/tronclass_rollcall
    exit $?
  fi
  echo "[ptero-start] 編譯失敗"
  exit 1
fi

echo "[ptero-start] 找不到 target/release/tronclass_rollcall"
exit 1
