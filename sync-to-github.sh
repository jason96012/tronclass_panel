#!/bin/bash
# 把程式碼推到 GitHub，絕不上傳個資／token／資料庫。
# 設定（擇一，都不要 commit）：
#   config/github.env 內容例如：
#     GITHUB_TOKEN=ghp_xxxxxxxx
#     GITHUB_REPO=jason96012/tronclass_rollcall
#     GITHUB_BRANCH=main
# 或環境變數同名。
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

LOG=logs/github-sync.log
mkdir -p logs

if [[ -f config/github.env ]]; then
  # shellcheck disable=SC1091
  set -a
  # 只讀 KEY=VALUE，避免執行惡意內容
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ "$line" =~ ^(GITHUB_TOKEN|GITHUB_REPO|GITHUB_BRANCH)=(.*)$ ]]; then
      export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
    fi
  done < config/github.env
  set +a
fi

TOKEN="${GITHUB_TOKEN:-}"
REPO="${GITHUB_REPO:-}"
BRANCH="${GITHUB_BRANCH:-main}"

{
  echo "===== $(date -Iseconds) ====="
  if ! command -v git >/dev/null 2>&1; then
    echo "沒有 git，略過"
    exit 0
  fi
  if [[ -z "$TOKEN" || -z "$REPO" ]]; then
    echo "未設定 GITHUB_TOKEN / GITHUB_REPO（config/github.env），略過推送"
    exit 0
  fi

  if [[ ! -d .git ]]; then
    echo "初始化 git"
    git init
    git checkout -B "$BRANCH" 2>/dev/null || git branch -M "$BRANCH" || true
  else
    git checkout -B "$BRANCH" 2>/dev/null || true
  fi

  git config user.email "ptero-sync@local"
  git config user.name "ptero-sync"

  # 只加程式檔，不要 git add -A（否則會掃到 .cargo / tailscale-state）
  git add -- .gitignore Cargo.toml Cargo.lock src 2>/dev/null || true
  [[ -d tools ]] && git add -- tools || true
  [[ -f config/config.toml.example ]] && git add -- config/config.toml.example || true
  shopt -s nullglob
  extra=( *.sh *.py *.md )
  ((${#extra[@]})) && git add -- "${extra[@]}"
  shopt -u nullglob

  git rm -r --cached --ignore-unmatch -- .cargo .cache target logs tailscale-state materials \
    config/config.toml config/accounts.db config/cf.token config/tailscale.authkey \
    config/github.env config/github.token .env >/dev/null 2>&1 || true

  bad="$(git diff --cached --name-only | grep -E '(^|/)(\.cargo|\.cache|target|logs|tailscale-state|materials)(/|$)|(^|/)config/config\.toml$|accounts\.db|(^|/)cf\.token$|tailscale\.authkey|github\.(token|env)|^\.env$' || true)"
  if [[ -n "$bad" ]]; then
    echo "拒絕推送，偵測到個資檔仍在暫存區："
    echo "$bad"
    git reset -q
    exit 1
  fi

  if git diff --cached --quiet; then
    echo "沒有可提交的程式變更"
    exit 0
  fi

  echo "即將提交："
  git diff --cached --name-only

  git commit -m "ptero auto-sync $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  git remote remove github-sync 2>/dev/null || true
  git remote add github-sync "https://x-access-token:${TOKEN}@github.com/${REPO}.git"
  if git push -u github-sync "HEAD:${BRANCH}"; then
    echo "已推到 github.com/${REPO} ${BRANCH}"
  else
    echo "push 失敗"
  fi
  git remote remove github-sync 2>/dev/null || true
} >>"$LOG" 2>&1
