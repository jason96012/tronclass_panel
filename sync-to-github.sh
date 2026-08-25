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

  # The remote may have commits made from GitHub/the other deployment. Fetch
  # and rebase first; never use force-push for automatic sync.
  if ! git fetch github-sync "$BRANCH"; then
    echo "無法讀取 GitHub 分支：請確認 token 仍有效、可讀取 ${REPO}，且分支 ${BRANCH} 存在"
    git remote remove github-sync 2>/dev/null || true
    return 1
  fi
  local remote_ref="github-sync/${BRANCH}"
  if ! git merge-base --is-ancestor "$remote_ref" HEAD; then
    echo "遠端已有新 commit，嘗試 rebase 本機自動同步 commit"
    if ! git rebase "$remote_ref"; then
      echo "rebase 發生衝突；已中止，不會覆蓋遠端或遺失本機 commit"
      git rebase --abort 2>/dev/null || true
      git remote remove github-sync 2>/dev/null || true
      return 1
    fi
  fi

  if git push -u github-sync "HEAD:${BRANCH}"; then
    echo "已推到 github.com/${REPO} ${BRANCH}"
    git remote remove github-sync 2>/dev/null || true
    return 0
  fi
  echo "push 失敗：請檢查 GitHub token 的 Contents Read and write 權限，或查看是否有新的遠端 commit"
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
