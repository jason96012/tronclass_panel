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
    echo "遠端已有新 commit，以本機主機版本優先 rebase"

    # [修復] 防護機制：Rebase 前若有任何未暫存變更，先 stash 避免 rebase 失敗
    local stashed=0
    if ! git diff --quiet || ! git diff --cached --quiet; then
      echo "工作區存在未提交變更，暫存（stash）以利 rebase..."
      git stash push -u -m "ptero-pre-rebase-$(date +%s)" >/dev/null 2>&1 || true
      stashed=1
    fi

    # During rebase, "theirs" means the local commit currently being replayed.
    # Prefer it for overlapping hunks, while retaining unrelated remote commits.
    if ! git rebase -X theirs "$remote_ref"; then
      echo "即使採用本機版本優先仍無法自動整合；已中止，不會覆蓋遠端或遺失本機 commit"
      git rebase --abort 2>/dev/null || true
      [[ "$stashed" -eq 1 ]] && git stash pop >/dev/null 2>&1 || true
      git remote remove github-sync 2>/dev/null || true
      return 1
    fi

    # Rebase 成功後還原 stash
    if [[ "$stashed" -eq 1 ]]; then
      git stash pop >/dev/null 2>&1 || true
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

  # 1. 依照白名單加入新增或修改的檔案/資料夾
  git add -- .gitignore Cargo.toml Cargo.lock src 2>/dev/null || true
  [[ -d tools ]] && git add -A -- tools || true
  # FILEBROWSER_GITHUB_SYNC_V1: File Browser 自訂品牌（CSS/logo 等，不含憑證）隨同步備份
  [[ -d filebrowser-branding ]] && git add -A -- filebrowser-branding || true
  [[ -f config/config.toml.example ]] && git add -- config/config.toml.example || true
  shopt -s nullglob
  extra=( *.sh *.py *.md )
  ((${#extra[@]})) && git add -- "${extra[@]}"
  shopt -u nullglob

  # 2. 強制從暫存區剔除機敏資料
  git rm -r --cached --ignore-unmatch -- .cargo .cache target logs tailscale-state materials \
    config/config.toml config/accounts.db config/cf.token config/tailscale.authkey \
    config/github.env config/github.token .env >/dev/null 2>&1 || true

  # 3. [關鍵修復] 同步已追蹤檔案的刪除與修改（Stage deletions/modifications）
  # -u 只會更新已在版本控制中的檔案狀態，絕不會將未追蹤的敏感檔加進暫存區
  git add -u

  # 4. 再次確保已剔除機敏檔案（雙重保險）
  git rm -r --cached --ignore-unmatch -- .cargo .cache target logs tailscale-state materials \
    config/config.toml config/accounts.db config/cf.token config/tailscale.authkey \
    config/github.env config/github.token .env >/dev/null 2>&1 || true

  # FILEBROWSER_GITHUB_SYNC_V1: fb-data/ 備份判斷
  FBDATA_ALLOW=0
  if [[ -f config/github-fbdata.allow ]]; then
    if command -v curl >/dev/null 2>&1; then
      priv="$(curl -fsS --max-time 15 \
        -H "Authorization: token ${TOKEN}" -H "Accept: application/vnd.github+json" \
        "https://api.github.com/repos/${REPO}" 2>/dev/null \
        | grep -o '"private"[[:space:]]*:[[:space:]]*\(true\|false\)' | head -1)"
      if [[ "$priv" == *true* ]]; then
        FBDATA_ALLOW=1
        echo "fb-data 備份已啟用（repo 已驗證為私有）"
      else
        echo "fb-data 未上傳：${REPO} 不是私有 repo（或無法確認）。fb-data 內含帳密資料庫，僅允許備份到私有 repo。"
      fi
    else
      echo "fb-data 未上傳：找不到 curl，無法驗證 repo 是否為私有。"
    fi
  else
    echo "fb-data 未上傳（預設保護中）。要啟用請建立 config/github-fbdata.allow，且 repo 必須為私有。"
  fi
  if [[ "$FBDATA_ALLOW" == 1 ]]; then
    git add -f -- fb-data 2>/dev/null || true
  else
    git rm -r --cached --ignore-unmatch -- fb-data >/dev/null 2>&1 || true
  fi

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