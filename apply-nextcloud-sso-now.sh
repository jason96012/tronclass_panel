#!/usr/bin/env bash
# 登入 Nextcloud 之後跑：啟用 SSO + 全班共用 10GB「教材」。
# 不會把密碼印出來。
#
# 用法：
#   chmod +x apply-nextcloud-sso-now.sh
#   ./apply-nextcloud-sso-now.sh
#   ./apply-nextcloud-sso-now.sh /home/container
set -uo pipefail

ROOT="${1:-}"
if [[ -z "${ROOT}" ]]; then
  if [[ -d "nextcloud" && -x "./php" ]]; then
    ROOT="$(pwd)"
  elif [[ -d /home/container/nextcloud ]]; then
    ROOT="/home/container"
  elif [[ -f "$(dirname "$0")/ptero-start.sh" ]]; then
    ROOT="$(cd "$(dirname "$0")" && pwd)"
  else
    echo "找不到專案。請把此檔放到伺服器根目錄再跑。"
    exit 1
  fi
fi
cd "$ROOT"

if [[ ! -x ./php || ! -f nextcloud/occ ]]; then
  echo "缺少 ./php 或 nextcloud/occ"
  exit 1
fi

occ() {
  (cd nextcloud && ../php -c ../nextcloud-runtime/php.ini occ "$@")
}

ADMIN="${NEXTCLOUD_ADMIN_USER:-admin}"
ISSUER="${DASHBOARD_ISSUER:-https://dashboard.jasoncloudserver.me}"
ISSUER="${ISSUER%/}"
PUBLIC="${NEXTCLOUD_PUBLIC_URL:-https://files.jasoncloudserver.me}"
PUBLIC="${PUBLIC%/}"
CLIENT_ID="${OIDC_CLIENT_ID:-nextcloud}"
CLIENT_SECRET=""
if [[ -f config/nextcloud-oidc.secret ]]; then
  CLIENT_SECRET="$(tr -d '\r\n' < config/nextcloud-oidc.secret)"
fi
if [[ -z "$CLIENT_SECRET" ]]; then
  echo "缺少 config/nextcloud-oidc.secret。先重啟一次 ptero-start.sh 讓它產生。"
  exit 1
fi

echo "[1/5] 安裝應用"
occ app:install user_oidc || true
occ app:enable user_oidc || true
occ app:install groupfolders || true
occ app:enable groupfolders || true
echo "    user_oidc：$(occ app:list 2>/dev/null | grep -i user_oidc || echo 未看到)"
echo "    groupfolders：$(occ app:list 2>/dev/null | grep -i groupfolders || echo 未看到)"

echo "[2/5] 群組 classroom + 額度 10GB"
occ group:add classroom >/dev/null 2>&1 || true
occ group:adduser classroom "$ADMIN" >/dev/null 2>&1 || true
occ config:app:set files default_quota --value="10 GB" >/dev/null 2>&1 || true
occ user:setting "$ADMIN" files quota "10 GB" >/dev/null 2>&1 || true

echo "[3/5] 共用資料夾 教材"
if occ groupfolders:list 2>/dev/null | grep -q 教材; then
  echo "    已有 教材"
else
  occ groupfolders:create 教材 || echo "    建立失敗：到 Nextcloud 設定 → 管理 → 群組資料夾 手動新增「教材」"
fi
ID="$(occ groupfolders:list 2>/dev/null | awk '/教材/{print $1}' | tr -dc '0-9' | head -c 8)"
[[ -n "$ID" ]] || ID=1
occ groupfolders:quota "$ID" "$((10*1024*1024*1024))" >/dev/null 2>&1 || true
occ groupfolders:group "$ID" classroom write >/dev/null 2>&1 || true
occ groupfolders:group "$ID" admin write >/dev/null 2>&1 || true

echo "[4/5] OpenID 連到 Dashboard"
if occ user_oidc:provider 2>/dev/null | grep -qi Dashboard; then
  echo "    provider Dashboard 已存在"
else
  occ user_oidc:provider "Dashboard" \
    --clientid="$CLIENT_ID" \
    --clientsecret="$CLIENT_SECRET" \
    --discoveryuri="${ISSUER}/admin/oidc/.well-known/openid-configuration" \
    --unique-uid=0 \
    --mapping-uid="preferred_username" \
    --mapping-groups="groups" || \
    echo "    occ 加 provider 失敗：到應用程式確認已啟用 OpenID Connect user backend，再跟我說"
fi

echo "[5/5] 確認 Dashboard config"
if [[ -f config/config.toml ]] && grep -qE '^[[:space:]]*\[oidc\]' config/config.toml; then
  echo "    config.toml 已有 [oidc]"
else
  echo "    config.toml 還沒有 [oidc]，請把 config/nextcloud-dashboard.snippet.toml 貼到最下面"
fi

echo
echo "接下來在 Pterodactyl："
echo "  1. 刪 target/release/tronclass_rollcall"
echo "  2. 重啟"
echo "  3. Dashboard 硬重新整理，先登入再點「教材」"
echo "  4. 第一次 Nextcloud 若問是否信任登入提供者 → 允許"
echo
echo "驗證 JSON："
echo "  ${ISSUER}/admin/oidc/.well-known/openid-configuration"
echo
echo "不要把密碼或 secret 貼到聊天。"
