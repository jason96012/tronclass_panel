#!/usr/bin/env bash
# 驗證 Cloudflare 邊緣限流是否生效（fix62）。
#
# 為什麼需要這支腳本：Cloudflare 的規則只有你自己能建（我沒有你的 API Token），
# 但「規則有沒有真的攔下來」可以從外面實測。這支腳本只做低量探測。
#
# 用法：
#   bash tools/verify_edge_limits.sh                          # 測登入端點（預設 12 次）
#   bash tools/verify_edge_limits.sh --path / --get --count 60
#   bash tools/verify_edge_limits.sh --path /admin/api/passkey/login/start --count 12
#
# ⚠️ 重要：打「登入端點」的失敗嘗試會觸發我方程式的自動黑名單
#    （15 分鐘內同一 IP 超過 10 次失敗登入 → 該 IP 被封鎖，需在面板「黑名單」頁解除）。
#    所以：
#      · 預設只送 12 次（剛好能觀察，但很容易踩到門檻）。
#      · 想純驗證邊緣層，建議用 --path / --get（一般 GET 不計入登入失敗）。
#      · 真的把自己鎖住了：登入面板 → 黑名單頁 → 找到自己的 IP → 解除封鎖。
#
# 判讀（腳本會自動分類）：
#   cf-mitigated 標頭        → 邊緣層（Cloudflare）攔截 ✅  ← 這才是我們要驗的
#   {"error":"access_denied"} → 應用層自動黑名單（不是邊緣層）
#   429                       → 應用層限流（登入端點第 6 次就會出現）
#   401 / 200                 → 沒被攔

set -uo pipefail

HOST="dashboard.jasoncloudserver.me"
PATH_TO_HIT="/admin/api/login"
COUNT=12
METHOD="POST"
SLEEP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --path) PATH_TO_HIT="$2"; shift 2 ;;
    --count) COUNT="$2"; shift 2 ;;
    --get) METHOD="GET"; shift ;;
    --sleep) SLEEP="$2"; shift 2 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "未知參數：$1（用 --help 看用法）" >&2; exit 2 ;;
  esac
done

URL="https://${HOST}${PATH_TO_HIT}"
echo "目標：${METHOD} ${URL}"
echo "次數：${COUNT}（間隔 ${SLEEP}s）"
echo

edge_blocked=0
app_blocked=0
app_limited=0
declare -A codes=()

for i in $(seq 1 "$COUNT"); do
  if [[ "$METHOD" == "POST" ]]; then
    out="$(curl -s -D /tmp/_edge_h -o /tmp/_edge_b -w '%{http_code}' -m 10 \
      -X POST -H "content-type: application/json" -H "origin: https://${HOST}" \
      --data '{"username":"zz_edge_probe_nonexistent","password":"x"}' "$URL" || echo 000)"
  else
    out="$(curl -s -D /tmp/_edge_h -o /tmp/_edge_b -w '%{http_code}' -m 10 "$URL" || echo 000)"
  fi
  code="$out"
  codes["$code"]=$(( ${codes["$code"]:-0} + 1 ))
  if grep -qi '^cf-mitigated:' /tmp/_edge_h; then edge_blocked=$((edge_blocked + 1));
  elif grep -q 'access_denied' /tmp/_edge_b 2>/dev/null; then app_blocked=$((app_blocked + 1));
  elif [[ "$code" == "429" ]]; then app_limited=$((app_limited + 1)); fi
  [[ "$SLEEP" != "0" ]] && sleep "$SLEEP"
done

echo "狀態碼分佈："
for code in "${!codes[@]}"; do printf '  %s × %s\n' "$code" "${codes[$code]}"; done
echo
printf '  邊緣層攔截（cf-mitigated）：%s 次\n' "$edge_blocked"
printf '  應用層黑名單（access_denied）：%s 次\n' "$app_blocked"
printf '  應用層限流（429）：%s 次\n' "$app_limited"
echo

if (( edge_blocked > 0 )); then
  echo "✅ 邊緣限流生效（Cloudflare 有回應 cf-mitigated 標頭）。"
  exit 0
fi
if (( app_blocked > 0 )); then
  echo "⚠️  你（或這個 IP）已經被「應用層」自動黑名單擋住了——這代表防護正常運作，"
  echo "    但這樣就測不到邊緣層。請先到面板「黑名單」頁解除封鎖，再重測（或換個網路）。"
  exit 2
fi
echo "⚠️  沒有觀察到邊緣攔截。可能原因："
echo "   1. 規則還沒建立／還沒生效（Cloudflare 設定通常 30 秒內生效）。"
echo "   2. 門檻比這次測試量高（例如 100 次/分鐘，而你只送了 ${COUNT} 次）。"
echo "   3. 規則排除了 Verified Bot／你的 IP。"
echo "   4. 你只看得到應用層的反應（429）→ 請到 Cloudflare 後台 Security Events 確認邊緣判定。"
exit 1
