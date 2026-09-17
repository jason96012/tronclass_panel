#!/usr/bin/env bash
# NOTIFY_DIAGNOSE_V1：新公告／新作業通知沒收到時，一次把每一道「會靜默跳過」的關卡查完。
#
# 只讀不寫：不會改設定、不會動 DB、不會送任何通知（DB 以 read-only 模式開啟）。
# 用法：
#   bash notify_diagnose.sh [部署目錄]        # 預設：目前目錄
#
# 建議直接在服務的工作目錄（config.toml、config/accounts.db 所在處）執行。
set -u

DIR="${1:-.}"
cd "$DIR" 2>/dev/null || { echo "找不到目錄：$DIR"; exit 1; }

CONFIG="config/config.toml"
[ -f "$CONFIG" ] || CONFIG="config.toml"          # 舊版放在根目錄時也吃
DB="config/accounts.db"
UPD="config/update_times.json"
NOTIFIED="config/bulletin_notified.json"

line() { printf '%s\n' "------------------------------------------------------------"; }
head2() { printf '\n== %s ==\n' "$1"; }
say() { printf '  %s\n' "$1"; }

echo "============================================================"
echo " 通知診斷（新公告／新作業 → Discord ＋ 手機推播）"
echo " 目錄：$(pwd)"
echo " 時間：$(date '+%Y-%m-%d %H:%M:%S %Z')"
echo "============================================================"

head2 "1. 檔案在不在"
for f in "$CONFIG" "$DB" "$UPD" "$NOTIFIED"; do
  if [ -f "$f" ]; then
    say "✅ $f（$(wc -c < "$f" | tr -d ' ') bytes，最後修改 $(date -r "$f" '+%Y-%m-%d %H:%M' 2>/dev/null || echo '?')）"
  else
    say "❌ $f 不存在"
  fi
done
[ -f "$UPD" ] || say "   ↳ update_times.json 不存在＝排程可能還沒跑過，或部署目錄不對。"
[ -f "$NOTIFIED" ] || say "   ↳ bulletin_notified.json 不存在＝公告輪詢還沒完成第一輪（首輪只建立基準、不發通知）。"

head2 "2. 設定檔（機密值已遮蔽）"
if [ -f "$CONFIG" ]; then
  python3 - "$CONFIG" <<'PY'
import re, sys, datetime
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
WANT = ("bulletins", "homework", "discord", "web_push", "notifications")
sections = {}
cur = None
for raw in text.splitlines():
    s = raw.strip()
    if s.startswith("[") and s.endswith("]"):
        cur = s.strip("[]")
        sections.setdefault(cur, [])
        continue
    if cur and s and not s.startswith("#"):
        sections[cur].append(s)
SECRET = re.compile(r"(token|secret|key|password|passwd|credential)", re.I)
for name in WANT:
    if name not in sections:
        print(f"  （沒有 [{name}] 段落 → 用預設值）")
        continue
    print(f"  [{name}]")
    for row in sections[name]:
        k = row.split("=")[0].strip()
        print("    " + (re.sub(r"=.*", "= ********", row) if SECRET.search(k) else row))
# 夜間窗判斷（伺服器本地時間）
bul = "\n".join(sections.get("bulletins", []))
def get(key, default):
    m = re.search(rf"^{key}\s*=\s*\"?([0-9:]+\"?)", bul, re.M)
    return (m.group(1).strip('"') if m else default)
if "night_pause" in bul or "bulletins" in sections:
    pause = re.search(r"night_pause\s*=\s*(true|false)", bul)
    start = get("night_pause_start", "00:00")
    end = get("night_pause_end", "08:00")
    now = datetime.datetime.now()
    def hm(t):
        h, m = t.split(":")[:2]
        return datetime.time(int(h), int(m))
    inside = False
    try:
        s_t, e_t, n_t = hm(start), hm(end), now.time()
        inside = (s_t <= n_t < e_t) if s_t <= e_t else (n_t >= s_t or n_t < e_t)
    except Exception:
        pass
    flag = "⚠️ 現在落在夜間暫停窗內＝公告要等窗結束後補推" if (inside and (pause is None or pause.group(1) == "true")) else "現在不在夜間暫停窗"
    print(f"  夜間暫停：{'預設 true' if pause is None else pause.group(1)}／窗 {start}–{end}（{datetime.datetime.now().strftime('%H:%M')}）→ {flag}")
PY
else
  say "找不到設定檔，跳過。"
fi

head2 "3. 排程最後成功／失敗（config/update_times.json）"
if [ -f "$UPD" ]; then
  python3 - "$UPD" <<'PY'
import json, sys, time
data = json.load(open(sys.argv[1], encoding="utf-8"))
now = time.time()
def rel(ts):
    if not ts:
        return "從未"
    d = int(now - ts)
    if d < 90: return f"{d} 秒前"
    if d < 5400: return f"{d // 60} 分鐘前"
    if d < 172800: return f"{d // 3600} 小時前"
    return f"{d // 86400} 天前"
def show(label, stamp):
    ok = stamp.get("last_ok_at", 0); okd = stamp.get("last_ok_detail", "")
    err = stamp.get("last_error_at", 0); errs = stamp.get("last_error", "")
    print(f"  {label:38s} 成功 {rel(ok):>10s} {('（'+okd+'）') if okd else ''}")
    if err:
        print(f"  {'':38s} 失敗 {rel(err):>10s} {errs}")
g = data.get("global") or data.get("globals") or {}
accts = data.get("accounts") or {}
for k in ("bulletin_poll", "homework_prefetch", "homework_reminder_check"):
    if k in g: show(k, g[k])
for key in ("bulletins", "homework", "morning_digest"):
    for acct, kinds in (accts.items() if isinstance(accts, dict) else []):
        if isinstance(kinds, dict) and key in kinds:
            show(f"{key}:{acct}", kinds[key])
print("  （沒列到的項目＝服務啟動後還沒跑過那一輪）")
PY
else
  say "沒有 update_times.json：代表排程還沒跑過，或這不是服務的工作目錄。"
fi

head2 "4. 帳號／綁定／訂閱／通知偏好（config/accounts.db，唯讀）"
if [ -f "$DB" ]; then
  python3 - "$DB" <<'PY'
import sqlite3, sys, time
con = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
q = con.execute
print("  -- 帳號（enabled / viewer / DC 綁定 / LINE 綁定） --")
rows = q("SELECT id, provider, enabled, is_viewer, discord_user_id, line_user_id FROM accounts ORDER BY id").fetchall()
if not rows:
    print("    （DB 裡沒有帳號）")
for r in rows:
    aid, prov, en, viewer, dc, line = r
    flags = []
    if not en: flags.append("未啟用→不輪詢")
    if viewer: flags.append("檢視者→不輪詢")
    if not (dc or "").strip(): flags.append("沒有 DC 綁定→DC 收不到")
    if not (line or "").strip(): pass
    print(f"    {aid:20s} provider={prov or '-':10s} {'✅已啟用' if en else '❌未啟用'} / {'檢視者' if viewer else '一般'} / DC {'有' if (dc or '').strip() else '無'}" + ("   ← " + "、".join(flags) if flags else ""))

print("\n  -- 手機推播訂閱（web_push_subscriptions） --")
rows = q("SELECT account_id, COUNT(*) FROM web_push_subscriptions GROUP BY account_id").fetchall()
if not rows:
    print("    ⚠️ 完全沒有訂閱 → 手機推播一定收不到（要到面板「通知設定」按開啟推播並允許通知）")
for aid, n in rows:
    print(f"    {aid:20s} {n} 台裝置")

print("\n  -- 推播金鑰／健康度（web_push_settings） --")
keys = q("SELECT key, length(value) FROM web_push_settings WHERE key NOT LIKE 'push_health_v1:%'").fetchall()
for k, ln in keys:
    print(f"    {k:28s} value {ln} bytes")
health = q("SELECT key, value FROM web_push_settings WHERE key LIKE 'push_health_v1:%'").fetchall()
for k, v in health:
    print(f"    健康度 {k.split(':',1)[1]:20s} {v[:160]}")

print("\n  -- 通知偏好（只有『被關掉』的才會列出來；沒有列＝預設開啟） --")
rows = q("SELECT account_id, channel, notification_key, enabled FROM notification_preferences ORDER BY account_id, channel").fetchall()
if not rows:
    print("    （沒有任何偏好列 → 全部走預設值＝開啟）")
for aid, ch, key, en in rows:
    if key in ("bulletin", "homework_deadline"):
        print(f"    {aid:20s} {ch:8s} {key:20s} {'✅開' if en else '❌關'}")

print("\n  -- 新作業通知基準（homework_seen；baseline＝已建立基準） --")
rows = q("SELECT account_id, COUNT(*), SUM(homework_id = 'baseline') FROM homework_seen GROUP BY account_id").fetchall()
if not rows:
    print("    ⚠️ 沒有任何紀錄 → 還沒建立基準（首輪只建立基準，不會通知）")
for aid, n, base in rows:
    print(f"    {aid:20s} 已評估 {n - (base or 0):4d} 份，基準 {'✅已建立' if base else '❌未建立'}")

print("\n  -- 作業截止提醒紀錄（homework_reminder_sent） --")
rows = q("SELECT account_id, COUNT(*), MAX(sent_at) FROM homework_reminder_sent GROUP BY account_id").fetchall()
if not rows:
    print("    （尚無任何截止提醒紀錄）")
now = time.time()
for aid, n, last in rows:
    d = int(now - (last or 0))
    when = "從未" if not last else (f"{d // 60} 分鐘前" if d < 5400 else f"{d // 3600} 小時前")
    print(f"    {aid:20s} {n} 筆，最近一次 {when}")

print("\n  -- 最近 6 小時的通知相關日誌（web_logs） --")
rows = q("""SELECT time, level, source, message FROM web_logs
            WHERE timestamp > ? AND (level IN ('WARN','ERROR')
                 OR message LIKE '%公告%' OR message LIKE '%作業%'
                 OR source LIKE '%bulletin%' OR source LIKE '%push%' OR source LIKE '%homework%')
            ORDER BY id DESC LIMIT 40""", (int(now) - 6 * 3600,)).fetchall()
if not rows:
    print("    （沒有相關日誌：可能時間窗內真的沒事，或日誌等級沒記到）")
for t, lv, src, msg in rows:
    print(f"    {t} [{lv}] {src}: {msg[:120]}")
con.close()
PY
else
  say "找不到 config/accounts.db，跳過。"
fi

head2 "5. 已通知的公告清單（config/bulletin_notified.json）"
if [ -f "$NOTIFIED" ]; then
  python3 - "$NOTIFIED" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
keys = data.get("keys") or []
print(f"  已記錄 {len(keys)} 筆（上限 3000，超出會從最舊的開始丟）")
for k in keys[-5:]:
    print("    最近的：" + str(k)[:110])
if not keys:
    print("  ⚠️ 清單是空的 → 公告輪詢還沒抓到任何公告（session 過期？或還沒跑過）")
PY
else
  say "沒有這個檔（首輪只建立基準、不發通知；第二輪之後才會有紀錄）。"
fi

head2 "6. 服務有沒有在跑"
RUNNING=""
for name in tronclass tronclass_panel; do
  if pgrep -x "$name" >/dev/null 2>&1; then RUNNING="$name"; break; fi
done
for unit in tronclass tronclass-panel; do
  if [ -z "$RUNNING" ] && command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "$unit" 2>/dev/null; then
    RUNNING="systemd:$unit"
  fi
done
if [ -n "$RUNNING" ]; then
  say "✅ 服務在跑（$RUNNING）"
  ps -eo pid,etime,args 2>/dev/null | awk -v n="$(basename "${RUNNING#systemd:}")" '$3 ~ n || $2 ~ n {print "    " $0}' | head -3
else
  say "⚠️ 沒看到 tronclass 的 process（若是 panel／systemd 管理，請用 systemctl status 再看一次；"
  say "    或者服務正常、只是排程還沒跑到——先看 §3 的時間戳）"
fi

line
cat <<'TIP'
怎麼讀這份結果（對照程式行為）：

 1) 手機推播完全沒收到
    → 看 §4「手機推播訂閱」有沒有 0 筆，或某帳號完全沒有列。
      沒有訂閱＝服務端送不出去（不是壞掉），要到面板「通知設定」按「開啟推播」並允許通知。
 2) Discord 沒收到、手機有收到
    → 看 §4 帳號的「DC 綁定」是否為「無」；沒綁定就只會走手機推播。
      再看 §2 [discord] enabled 是否 false、bot_token 是否為空。
 3) 兩個都沒收到（公告）
    → 看 §3 bulletin_poll 最後成功時間：太久沒更新或一直失敗＝學校端讀不到（session 過期最常見）。
      看 §2 是否正落在夜間暫停窗（00:00–08:00 預設）。
      看 §5 清單：空的話代表輪詢還沒抓到公告；剛部署的第一輪只建立基準、不通知。
 4) 兩個都沒收到（新作業）
    → 看 §4「新作業通知基準」是否已建立（❌＝首輪，不發通知）。
      教材／連結／影片不算作業，不會通知；已繳交的不通知；一輪上限 5 則，其餘下一輪再發。
 5) 只有某幾個帳號收不到
    → 看該帳號是否「未啟用／檢視者」（不輪詢），或 §4 偏好列顯示 ❌關。
 6) 偏好的預設值
    → 沒有偏好列＝開啟；只有「❌關」才會擋掉（本工具會把被關掉的列出來）。
TIP
echo
echo "（本工具只讀不寫；把上面整份輸出貼回即可協助判斷）"
echo "============================================================"
