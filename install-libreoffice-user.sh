#!/usr/bin/env bash
# ============================================================================
# install-libreoffice-user.sh
#
# 在 Pterodactyl 容器内「免 root、免 sudo、不动 egg/映像/面板設定」安装 LibreOffice，
# 让云端硬碟的 PPT/Word 预览可以转 PDF。
#
# 做法：
#   1. 下载 LibreOffice 官方 Linux x86-64 .deb 套件压缩包
#   2. 用 `dpkg-deb -x`（不需要 root）解压到使用者家目录 ~/.local/libreoffice
#   3. 直接执行那里的 soffice 转换
#
# 之后把本专案重新部署（files.rs 已会自动搜寻 ~/.local/libreoffice ... 下的 soffice），
# 预览 PPT/Word 就会走 LibreOffice 转 PDF。
#
# 使用：在 Pterodactyl 面板的 Console 贴上任一后；或
#       bash install-libreoffice-user.sh
# ============================================================================
set -uo pipefail

ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
  echo "[LO-install] 只支援 x86_64（目前：$ARCH）。此脚本不处理其它架构。"
  exit 1
fi

PREFIX="${LIBREOFFICE_PREFIX:-$HOME/.local/libreoffice}"
echo "[LO-install] 安装到：$PREFIX"

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
cd "$workdir" || exit 1

download() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$out" "$url" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -O "$out" "$url" && return 0
  fi
  return 1
}

# 多试几个版本，避免单一 URL 404 就失败。
# 目录名 = 前三个版本号（如 7.6.7.x -> 7.6.7；24.2.x -> 24.2）。
candidates=(
  "24.2.0"
  "7.6.7.2"
  "7.6.6.3"
  "7.6.4.1"
)

TARBALL=""
for VER in "${candidates[@]}"; do
  dir="${VER%.*}"                 # 去掉最后一段 => stable 目录名
  URL="https://download.documentfoundation.org/libreoffice/stable/${dir}/deb/x86_64/LibreOffice_${VER}_Linux_x86-64_deb.tar.gz"
  echo "[LO-install] 尝试下载版本 $VER ..."
  if download "$URL" "lo.tar.gz"; then
    echo "[LO-install] 已下载 $VER"
    TARBALL="lo.tar.gz"
    break
  else
    echo "[LO-install] 下载 $VER 失败，换下一个版本。"
    rm -f "lo.tar.gz"
    continue
  fi
done

if [[ -z "$TARBALL" ]]; then
  echo "[LO-install] 无法从 documentfoundation 下载，请检查网路。"
  exit 1
fi

echo "[LO-install] 解压 .deb 压缩包..."
tar -xzf "$TARBALL"
DEBS_DIR="$(find . -maxdepth 2 -type d -name DEBS | head -n 1)"
if [[ -z "$DEBS_DIR" ]]; then
  echo "[LO-install] 找不到 DEBS 目录。"
  exit 1
fi

echo "[LO-install] 用 dpkg-deb -x 解压套件到 $PREFIX（不需 root）..."
mkdir -p "$PREFIX"
COUNT=0
for deb in "$DEBS_DIR"/*.deb; do
  [[ -f "$deb" ]] || continue
  dpkg-deb -x "$deb" "$PREFIX" || { echo "[LO-install] 解压 $deb 失败，终止。"; exit 1; }
  COUNT=$((COUNT + 1))
done
echo "[LO-install] 已解压 $COUNT 个 .deb 套件。"

# 找 soffice 的位置
SOFFICE=""
for candidate in \
  "$PREFIX/usr/lib/libreoffice/program/soffice" \
  "$PREFIX/opt/libreoffice/program/soffice" \
  "$PREFIX/usr/lib/libreoffice/program/soffice.bin"; do
  if [[ -x "$candidate" ]]; then
    SOFFICE="$candidate"
    break
  fi
done
if [[ -z "$SOFFICE" ]]; then
  SOFFICE="$(find "$PREFIX" -type f -name soffice -perm -u+x 2>/dev/null | head -n 1)"
fi

if [[ -z "$SOFFICE" ]]; then
  echo "[LO-install] 找不到 soffice 执行档。"
  exit 1
fi

chmod +x "$SOFFICE" 2>/dev/null || true
echo ""
echo "=============================================================="
echo "[LO-install] 安装完成。soffice = $SOFFICE"
echo "=============================================================="

# 自检：能否以 headless 启动（不真正转档）。此步会试跑 soffice --version。
if "$SOFFICE" --headless --version >/tmp/lo-version.txt 2>&1; then
  echo "[LO-install] 自检 OK："
  cat /tmp/lo-version.txt
else
  echo "[LO-install] 警告：soffice --version 失败，输出如下（多半缺系统函式库）："
  cat /tmp/lo-version.txt 2>/dev/null || true
  echo "[LO-install] 若显示 'error while loading shared libraries: libXXX...'，"
  echo "[LO-install] 表示此容器缺少该函式库，需要在 egg/映像层面补（与本脚本无 root 安装无关）。"
fi

echo ""
echo "[LO-install] 若要程式改用这个路径，可设定环境变数："
echo "    export TRONCLASS_LIBREOFFICE=$SOFFICE"
echo "[LO-install] 或直接重新部署（files.rs 会自动搜寻 ~/.local/libreoffice 下的 soffice）。"
echo "[LO-install] 你也可随时用下列指令手动测试转换："
echo "    $SOFFICE --headless --convert-to pdf --outdir . <你的 .pptx/.docx>"
