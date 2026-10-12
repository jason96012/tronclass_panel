#!/usr/bin/env bash
# =============================================================================
#  日和選物所 HIYORI — 靜態站啟動器（Pterodactyl 容器內用）
#
#  為什麼需要這支：你的面板是 Rust 單一執行檔（靜態資產用 include_str! 內嵌），
#  要塞進商店就得改 Rust 並重新編譯。這支改用「第二個來源」：
#  在同一個容器裡用 Caddy 服務純靜態資料夾，再由 Cloudflare Tunnel 用新的
#  Public Hostname（shop.jasoncloudserver.me）指進來，完全不用動 Rust。
#
#  與你既有做法的關係（讀 ptero-start.sh 後對齊）：
#    - 你已經有 ./caddy（filebrowser 用的），本腳本直接沿用，不重複下載
#    - Caddyfile 用 heredoc 產生（和你 nextcloud-runtime/Caddyfile.fb 同一個風格）
#    - 背景啟動用 nohup ... & disown，PID 寫到 logs/（和你 cloudflared/filebrowser 一致）
#    - 你已占用的埠：26000（主程式）、26001/26011/26012/26013/26021/26101~26103
#      （filebrowser＋Caddy）、8080（LINE webhook）、18765（智選 sidecar）
#      → 本腳本預設使用沒被占用的 26200
#
#  用法（在 ptero-start.sh 裡，放在啟動 filebrowser 的那一段附近）：
#      source ./shop-static-server.sh
#      shop_static_start
#  然後在你既有的 watchdog 迴圈裡加一行：
#      shop_static_watchdog
#
#  也可以直接執行：./shop-static-server.sh [run|start|stop|status|selftest]
# =============================================================================
set -u

_SHOP_BASE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHOP_DIR="${SHOP_DIR:-$_SHOP_BASE/shop}"
SHOP_PORT="${SHOP_PORT:-26200}"
SHOP_LOG_DIR="${SHOP_LOG_DIR:-$_SHOP_BASE/logs}"
SHOP_RUNTIME_DIR="${SHOP_RUNTIME_DIR:-$_SHOP_BASE/nextcloud-runtime}"
SHOP_PID_FILE="$SHOP_LOG_DIR/shop-static.pid"
SHOP_LOG_FILE="$SHOP_LOG_DIR/shop-static.log"
SHOP_WD_LOG="$SHOP_LOG_DIR/shop-static-watchdog.log"
SHOP_CADDYFILE="$SHOP_RUNTIME_DIR/Caddyfile.shop"

_shop_dl() {  # 與 ptero-start.sh 相同的下載策略：curl → wget
    local url="$1" out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --output "$out" "$url" && return 0
    fi
    if command -v wget >/dev/null 2>&1; then
        wget -c -O "$out" "$url" && return 0
    fi
    return 1
}

shop_static_write_caddyfile() {
    mkdir -p "$SHOP_RUNTIME_DIR" "$SHOP_LOG_DIR"
    if [[ -f "$SHOP_DIR/Caddyfile" ]]; then
        # 若你自行在 shop/ 放了 Caddyfile 就以你的為準（記得 root 要指到 $SHOP_DIR）
        SHOP_CADDYFILE="$SHOP_DIR/Caddyfile"
        return 0
    fi
    cat > "$SHOP_CADDYFILE" << EOF
{
    admin off
    auto_https off
    persist_config off
}

:${SHOP_PORT} {
    bind 127.0.0.1
    root * ${SHOP_DIR}
    encode zstd gzip

    # 乾淨網址：/shop → /shop.html、/ → /index.html
    try_files {path} {path}.html {path}/index.html =404

    file_server {
        hide .git
    }

    # _headers 是給 Cloudflare Pages 用的檔案，這裡不需要公開
    respond /_headers 404

    # HTML 不快取（改版立即生效）；靜態資產長快取
    @html path *.html /
    header @html Cache-Control "public, max-age=0, must-revalidate"
    @assets path *.jpg *.jpeg *.png *.svg *.webp *.ico *.css *.js
    header @assets Cache-Control "public, max-age=2592000, immutable"

    # 找不到頁面時回品牌化的 404（而不是空白頁）
    handle_errors {
        @notfound expression {err.status_code} == 404
        rewrite @notfound /404.html
        file_server
    }

    header {
        -Server
        Content-Security-Policy "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'self'; object-src 'none'"
        X-Content-Type-Options "nosniff"
        Referrer-Policy "strict-origin-when-cross-origin"
        X-Frame-Options "SAMEORIGIN"
        Permissions-Policy "geolocation=(), microphone=(), camera=(), payment=(), usb=()"
        Cross-Origin-Opener-Policy "same-origin"
    }

    log {
        output file ${SHOP_LOG_DIR}/shop-access.log {
            roll_size 5MiB
            roll_keep 3
        }
    }
}
EOF
}

shop_static_ready() {  # 0 = 已在跑
    [[ -f "$SHOP_PID_FILE" ]] || return 1
    local pid; pid="$(cat "$SHOP_PID_FILE" 2>/dev/null || echo)"
    [[ -n "$pid" ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    return 0
}

shop_static_port_busy() {  # 0 = 埠已被別人用（避免撞到你自己的服務）
    if command -v ss >/dev/null 2>&1; then
        ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${SHOP_PORT}\$"
    else
        return 1
    fi
}

shop_static_start() {
    mkdir -p "$SHOP_LOG_DIR"
    if [[ ! -d "$SHOP_DIR" ]]; then
        echo "[shop] 找不到網站資料夾 $SHOP_DIR（把 hiyori-deploy zip 解到這裡）"
        return 1
    fi
    if [[ ! -f "$SHOP_DIR/index.html" ]]; then
        echo "[shop] $SHOP_DIR 沒有 index.html，這不是網站根目錄"
        return 1
    fi
    if shop_static_ready; then
        echo "[shop] 已在執行（pid $(cat "$SHOP_PID_FILE")，port $SHOP_PORT）"
        return 0
    fi
    if shop_static_port_busy; then
        echo "[shop] 注意：port $SHOP_PORT 已被占用，請改 SHOP_PORT 後再試"
        return 1
    fi

    # 優先用你容器裡既有的 caddy（filebrowser 用同一個 binary）
    local bin=""
    if [[ -x "$_SHOP_BASE/caddy" ]]; then
        bin="$_SHOP_BASE/caddy"
    elif command -v caddy >/dev/null 2>&1; then
        bin="$(command -v caddy)"
    fi
    if [[ -z "$bin" ]]; then
        echo "[shop] 找不到 caddy，嘗試下載到 $_SHOP_BASE/caddy ..."
        _shop_dl "https://caddyserver.com/api/download?os=linux&arch=amd64" "$_SHOP_BASE/caddy" || true
        chmod +x "$_SHOP_BASE/caddy" 2>/dev/null || true
        [[ -x "$_SHOP_BASE/caddy" ]] && bin="$_SHOP_BASE/caddy"
    fi

    if [[ -n "$bin" ]]; then
        shop_static_write_caddyfile
        echo "[shop] 以 Caddy 啟動 $SHOP_DIR → 127.0.0.1:$SHOP_PORT（$SHOP_CADDYFILE）"
        nohup "$bin" run --config "$SHOP_CADDYFILE" --adapter caddyfile >> "$SHOP_LOG_FILE" 2>&1 &
        echo $! > "$SHOP_PID_FILE"
        disown || true
    else
        if ! command -v python3 >/dev/null 2>&1; then
            echo "[shop] 沒有 caddy 也沒有 python3，無法啟動"
            return 1
        fi
        echo "[shop] 以 python3 http.server 啟動（備援模式：沒有壓縮與安全標頭）"
        nohup python3 -m http.server "$SHOP_PORT" --bind 127.0.0.1 --directory "$SHOP_DIR" >> "$SHOP_LOG_FILE" 2>&1 &
        echo $! > "$SHOP_PID_FILE"
        disown || true
    fi

    sleep 1
    if shop_static_ready; then
        echo "[shop] 啟動完成（pid $(cat "$SHOP_PID_FILE")）"
        return 0
    fi
    echo "[shop] 啟動失敗，請看 $SHOP_LOG_FILE"
    return 1
}

shop_static_watchdog() {  # 放進你既有的 watchdog 迴圈
    if shop_static_ready; then
        return 0
    fi
    echo "[shop-watchdog] $(date -Is) 靜態站不在，重新啟動" >> "$SHOP_WD_LOG" 2>/dev/null || true
    shop_static_start >/dev/null 2>&1 || true
}

shop_static_stop() {
    if [[ -f "$SHOP_PID_FILE" ]]; then
        local pid; pid="$(cat "$SHOP_PID_FILE" 2>/dev/null || echo)"
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
        rm -f "$SHOP_PID_FILE"
        echo "[shop] 已停止"
    else
        echo "[shop] 沒有在執行"
    fi
}

shop_static_status() {
    if shop_static_ready; then
        echo "[shop] 執行中：pid $(cat "$SHOP_PID_FILE")、port $SHOP_PORT、目錄 $SHOP_DIR"
    else
        echo "[shop] 未執行"
    fi
}

shop_static_selftest() {  # 快速自我檢查（HTTP 200 + 安全標頭 + 404 頁）
    local code csp
    code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${SHOP_PORT}/" 2>/dev/null || echo 000)"
    csp="$(curl -sI "http://127.0.0.1:${SHOP_PORT}/" 2>/dev/null | grep -ci 'content-security-policy' || true)"
    echo "[shop] / → $code（200 才算正常）"
    echo "[shop] Content-Security-Policy 標頭：$csp（1 = 有）"
    echo "[shop] http://127.0.0.1:${SHOP_PORT}/404.html → $(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${SHOP_PORT}/404.html" 2>/dev/null || echo 000)"
}

# 被 ptero-start.sh source 時不跑 CLI（避免每次開機多印一行用法）；
# 直接執行（./shop-static-server.sh run）時才有 CLI。
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        run)      shop_static_start; shop_static_selftest ;;
        start)    shop_static_start ;;
        stop)     shop_static_stop ;;
        status)   shop_static_status ;;
        selftest) shop_static_selftest ;;
        *)        echo "用法：$0 [run|start|stop|status|selftest]（或 source 後呼叫 shop_static_start）" ;;
    esac
fi
