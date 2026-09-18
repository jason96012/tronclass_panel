#!/usr/bin/env bash
# release.sh — 產生交付用的 zip（＝`make pack` 背後的實作）。
#
# 為什麼要有這支：交付流程本來是幾件事散在各處（「記得跑檢查」「記得更新 SHA」
# 「記得刪舊 zip」）。這支把它變成一個指令，而且**每一步都會驗**：
#
#   ① preflight：交付檔身分（SHA-256 前 16 碼＋bytes）與樹上一致
#   ② 檢查套件：守則／反向／離線模擬全綠（可用 --skip-checks 略過，僅限趕時間時）
#   ③ 打包：src/ config/ tools/ checks/ ＋工程表面檔案
#   ④ SHA256SUMS：把每個檔的校驗碼寫進 zip 內（部署端不必信任傳輸過程）
#   ⑤ 只留最新：舊的 src_*.zip 會刪掉（避免部署到舊包）
#
# 用法：
#   tools/release.sh                     # 完整流程（輸出到 /home/user）
#   tools/release.sh --label fix78        # 檔名 src_YYYYMMDD_fix78.zip
#   tools/release.sh --out /path --name src_20260101_fix99.zip
#   tools/release.sh --skip-checks        # 跳過 ②（仍然會跑 ①）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="/home/user"
ZIP_NAME=""
LABEL=""
SKIP_CHECKS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT_DIR="$2"; shift 2 ;;
    --name) ZIP_NAME="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --skip-checks) SKIP_CHECKS=1; shift ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "不認識的參數：$1（用 --help 看用法）" >&2; exit 2 ;;
  esac
done

[ -n "$ZIP_NAME" ] || ZIP_NAME="src_$(date -u +%Y%m%d)${LABEL:+_$LABEL}.zip"
ZIP_PATH="$OUT_DIR/$ZIP_NAME"
mkdir -p "$OUT_DIR"

echo "=============================================================="
echo " 打包交付：$ZIP_NAME"
echo " 來源：$ROOT"
echo " 時間：$(date '+%F %T %Z')"
echo "=============================================================="

# ── ① 交付檔身分（不符就不准出貨）────────────────────────────────
echo
echo "── ① preflight（交付檔身分）─────────────────────────────────"
if [ -f "$ROOT/checks/deploy_preflight.sh" ]; then
  PREFLIGHT_OUT="$(bash "$ROOT/checks/deploy_preflight.sh" "$ROOT")"
  echo "$PREFLIGHT_OUT" | grep -E "相符|不符" || true
  if ! echo "$PREFLIGHT_OUT" | grep -q "不符 0 檔"; then
    echo "❌ 有交付檔與 preflight 期望值不符——先修好再打包（不要出貨一個對不上的包）" >&2
    exit 1
  fi
else
  echo "  （找不到 checks/deploy_preflight.sh，略過）"
fi

# ── ② 檢查套件 ─────────────────────────────────────────────────
if [ "$SKIP_CHECKS" -eq 0 ] && [ -f "$ROOT/checks/run_all_checks.sh" ]; then
  echo
  echo "── ② 檢查套件（循序）──────────────────────────────────────"
  bash "$ROOT/checks/run_all_checks.sh" | tail -4
else
  echo
  echo "── ② 檢查套件：略過 ──────────────────────────────────────"
fi

# ── ③ 打包 ─────────────────────────────────────────────────────
echo
echo "── ③ 打包 ─────────────────────────────────────────────────"
cd "$ROOT"

PAYLOAD=(src config tools)
[ -d checks ] && PAYLOAD+=(checks)
for extra in Makefile rustfmt.toml clippy.toml rust-toolchain.toml .editorconfig \
             CHANGELOG.md CONTRIBUTING.md .gitmessage .github; do
  [ -e "$extra" ] && PAYLOAD+=("$extra")
done

rm -f "$ZIP_PATH"
zip -qr "$ZIP_PATH" "${PAYLOAD[@]}" -x '*/__pycache__/*' -x '*.pyc' -x '*.log'

# ── ④ SHA256SUMS（先寫在樹外，再加進 zip）──────────────────────
echo "── ④ 產生 SHA256SUMS ──────────────────────────────────────"
MANIFEST_DIR="$(mktemp -d)"
MANIFEST="$MANIFEST_DIR/SHA256SUMS"
find "${PAYLOAD[@]}" -type f ! -path '*/__pycache__/*' ! -name '*.pyc' -print0 \
  | sort -z | xargs -0 sha256sum > "$MANIFEST"
( cd "$MANIFEST_DIR" && zip -q "$ZIP_PATH" SHA256SUMS )
echo "  已記錄 $(wc -l < "$MANIFEST" | tr -d ' ') 個檔案"
rm -rf "$MANIFEST_DIR"

# ── ⑤ 只留最新一份 ─────────────────────────────────────────────
echo "── ⑤ 清理舊交付包（只留最新）──────────────────────────────"
for old in "$OUT_DIR"/src_*.zip; do
  [ -e "$old" ] || continue
  if [ "$(basename "$old")" != "$ZIP_NAME" ]; then
    echo "  刪除：$(basename "$old")"
    rm -f "$old"
  fi
done
[ -f "$ZIP_PATH" ] || { echo "❌ 找不到剛產生的 zip" >&2; exit 1; }

# ── 摘要 ───────────────────────────────────────────────────────
SIZE=$(wc -c < "$ZIP_PATH" | tr -d ' ')
ENTRIES=$(unzip -Z1 "$ZIP_PATH" | wc -l | tr -d ' ')
FILES=$(unzip -Z1 "$ZIP_PATH" | grep -vc '/$' || true)
SHA=$(sha256sum "$ZIP_PATH" | cut -c1-16)

echo
echo "=============================================================="
echo " 完成：$ZIP_PATH"
echo "   大小 $SIZE bytes／項目 $ENTRIES（檔案 $FILES）"
echo "   zip sha256 前 16 碼：$SHA"
echo
echo " 部署端步驟："
echo "   unzip -o $(basename "$ZIP_PATH") -d <專案目錄>"
echo "   cd <專案目錄> && sha256sum -c SHA256SUMS | tail -3    # 確認每個檔都沒壞"
echo "   bash checks/deploy_preflight.sh .                     # 應為「相符 N 檔／不符 0 檔」"
echo "   find src -name '*.rs' -exec touch {} +"
echo "   cargo build --release --no-default-features"
echo "=============================================================="
