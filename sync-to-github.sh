#!/bin/bash
# 把程式碼推到 GitHub，絕不上傳個資／token／資料庫。
# config/github.env：
#   GITHUB_TOKEN=ghp_或_github_pat_
#   GITHUB_REPO=jason96012/tronclass_panel
#   GITHUB_BRANCH=main
set -uo pipefail
cd /home/container 2>/dev/null || cd "$(dirname "$0")"

LOG=logs/github-sync.log
mkdir -p logs

if [[ -f config/github.env ]]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ "$line" =~ ^(GITHUB_TOKEN|GITHUB_REPO|GITHUB_BRANCH)=(.*)$ ]]; then
      val="${BASH_REMATCH[2]}"
      val="${val#\"}"
      val="${val%\"}"
      val="${val#\'}"
      val="${val%\'}"
      val="${val#"${val%%[![:space:]]*}"}"
      val="${val%"${val##*[![:space:]]}"}"
      export "${BASH_REMATCH[1]}=${val}"
    fi
  done < config/github.env
fi

TOKEN="${GITHUB_TOKEN:-}"
REPO="${GITHUB_REPO:-}"
BRANCH="${GITHUB_BRANCH:-main}"
REPO="${REPO#https://github.com/}"
REPO="${REPO#git@github.com:}"
REPO="${REPO%.git}"
REPO="${REPO%/}"

do_push() {
  git remote remove github-sync 2>/dev/null || true
  git remote add github-sync "https://x-access-token:${TOKEN}@github.com/${REPO}.git"
  if git -c http.extraHeader="AUTHORIZATION: bearer ${TOKEN}" push -u github-sync "HEAD:${BRANCH}"; then
    echo "已推到 github.com/${REPO} ${BRANCH}"
    git remote remove github-sync 2>/dev/null || true
    return 0
  fi
  echo "bearer 失敗，改試 x-access-token URL"
  if git push -u github-sync "HEAD:${BRANCH}"; then
    echo "已推到 github.com/${REPO} ${BRANCH}"
    git remote remove github-sync 2>/dev/null || true
    return 0
  fi
  echo "push 失敗：token 無效或沒有 ${REPO} 的寫入權限"
  echo "請用 jason96012 開 Fine-grained token，Contents = Read and write，只選 tronclass_panel"
  git remote remove github-sync 2>/dev/null || true
  return 1
}

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
  echo "倉庫=${REPO} 分支=${BRANCH} token長度=${#TOKEN} 開頭=${TOKEN:0:7}..."

  if [[ ! -d .git ]]; then
    echo "初始化 git"
    git init
  fi
  git checkout -B "$BRANCH" 2>/dev/null || git branch -M "$BRANCH" || true
  git config user.email "ptero-sync@local"
  git config user.name "ptero-sync"

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
    echo "沒有新的檔案變更，仍嘗試推送既有 commit"
  else
    echo "即將提交："
    git diff --cached --name-only
    git commit -m "ptero auto-sync $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  fi

  do_push
} >>"$LOG" 2>&1
