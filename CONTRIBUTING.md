# 貢獻指南

這份文件給「要改這個專案的人」看——包含未來的你自己。目標是：**任何一次改動，都有固定的做法、
固定的檢查、固定的交付方式**，不需要靠記憶。

---

## 1. 這個專案怎麼運作（先懂這件事，其他才看得懂）

不像一般專案「push 上去、CI 部署」，本專案的流程是：

```
改檔（這個 repo）
  ↓  make verify        追蹤／排版／測試／檢查套件／preflight 全過
  ↓  make pack          產生 src_YYYYMMDD_fixNN.zip（含 SHA256SUMS，只留最新一份）
  ↓  上傳 zip 到部署端（Pterodactyl 容器 /home/container）
  ↓  bash checks/deploy_preflight.sh .    確認解開的是「這一份」交付（檔案身分相符）
  ↓  cargo build --release --no-default-features
  ↓  watchdog 重啟並維持舊 binary（編譯失敗不會把服務打掛）
```

* **GitHub 是鏡像**：`sync-to-github.sh` 會自動 fetch + rebase 後推上去（不用 force push，也不會覆蓋遠端）。
* **版本控制不是交付手段**：部署端拿到的永遠是 zip。所以「改檔 → 打包」是**同一個動作**，
  沒有「先 commit 再說」這種事。

## 2. 環境需求

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Rust | stable（含 `rustfmt`、`clippy`） | 建置；`rust-toolchain.toml` 有裝 rustup 會自動採用 |
| Python | 3.11+（建議 3.13） | 檢查套件（`checks/`）、`make check` |
| Node.js | 20+ | 4 支前端離線模擬（`checks/*_sim.mjs`） |
| git | 任一 | `make hooks` 會裝 commit hook |

第一次拿到這個 repo：

```bash
make hooks                                  # 安裝 commit-msg hook 與訊息範本（每台機器一次）
pip install tree_sitter tree_sitter_rust     # checks/rust_syntax_check.py 需要
make help                                    # 看有哪些指令
```

## 3. 常用指令

```bash
make help          # 全部指令與變數
make fmt           # 依 rustfmt.toml 排版（請單獨一個 commit）
make clippy        # 靜態檢查（門檻在 clippy.toml）
make test          # 601 個單元測試
make test-one FILTER=radar
make build         # 與部署端相同的參數
make check         # 檢查套件（49 支：守則＋反向＋離線模擬，循序跑，約 2 分鐘）
make preflight     # 交付檔身分（SHA-256 前 16 碼＋bytes）與關鍵內容行
make verify        # 上面全部跑一遍 ← 交付前一定要過
make pack          # 打包 zip（含 SHA256SUMS、只留最新）
```

## 4. 檢查套件（`checks/`）的三層結構

| 層 | 檔案 | 回答的問題 | 什麼時候加 |
| --- | --- | --- | --- |
檢查套件住在 `checks/`（與程式碼同一個 repo），**路徑自己算**——不管你把專案放在哪裡、
還是 CI checkout 到暫存目錄，它掃的都是「自己所在的那棵樹」。要驗證這件事：
`cp -r . /tmp/anywhere && cd /tmp/anywhere && python3 checks/system_relay_guard.py`。

| **守則（guard）** | `*_guard.py`、`rust_*.py` | 「這個設計還在嗎？」 | 每新增一個功能 |
| **反向測試（reverse）** | `reverse_*.py` | 「如果把它改壞，守則抓得到嗎？」 | 每新增一條守則 |
| **離線模擬（sim）** | `*_sim.mjs`、`radar_rollcall_sim.py` | 「行為對不對？」不連網、不發真實請求 | 流程有分支時 |

> 只加守則、不加反向測試，等於「自己說自己合格」。反向測試才是守則的證據。

新增一條守則的最小步驟：

1. 在對應的 `*_guard.py` 加一個檢查項（每項都要能在**訊息裡說出它為什麼存在**）。
2. 在 `reverse_*.py` 加一個突變（把正確程式改壞），確認守則會抓到。
3. 跑 `make check`，全綠才算數。

`deploy_preflight.sh` 是給**部署端**用的最後一道：它比對交付檔的 SHA-256 與 bytes。
只要動到 `src/`，就一定要同步更新那張表（`make pack` 之後跑一次 `make preflight` 就知道有沒有漏）。

## 5. 標記（marker）規範

程式碼裡 `// SOMETHING_V1：…` 這種註解是**設計標記**，不是施工日誌：

* **寫「為什麼」**，不要寫「什麼時候、第幾批改的」。
  * ✅ `// RADAR_SCHOOL_BLOCK_V1：學校回 429／403 代表它在擋我們，繼續送只會更糟——整場停手並通知管理員。`
  * ❌ `// fix77：新增停手`
* **名字描述功能**（`RADAR_SCHOOL_BLOCK_V1`），不要用日期或批次號（`FIX77_V1`）。
* 批次號只寫在 `CHANGELOG.md`；使用者想追歷史就從標記名去 CHANGELOG 搜。
* 標記同時是檢查套件的錨點：**改註解文字前先跑 `make check`**，守則會告訴你哪些地方靠它辨識。

## 6. Commit 訊息

採 [Conventional Commits](https://www.conventionalcommits.org/)：

```
<type>(<scope>): <一句話說明做了什麼>

<為什麼這麼做、有什麼取捨>
<影響範圍：動了哪些檔／需要重編譯嗎／要不要更新 preflight>

Refs: fix77
```

| type | 用途 |
| --- | --- |
| `feat` | 新功能 |
| `fix` | 修 bug |
| `perf` | 效能 |
| `refactor` | 不改行為的整理 |
| `docs` | 文件 |
| `test` | 測試（含 `checks/`） |
| `build` | 建置設定（`Cargo.toml`、`Makefile`） |
| `ci` | CI |
| `chore` | 其他（格式、工具） |

* `make hooks` 裝的 hook 會擋下不合格式的訊息；**格式錯誤會直接拒絕 commit**。
* 例外：`ptero auto-sync <時間戳>`（`sync-to-github.sh` 自動產生的同步 commit）由 hook 放行——
  那是鏡像同步紀錄，不是人類的 commit。

## 7. 改動前後的自檢清單

交付前逐項確認（`make verify` 會幫你跑大部分）：

- [ ] 有動 `src/` 嗎？→ 一定要 `make pack`，並更新 `checks/deploy_preflight.sh` 的身分表。
- [ ] 有動 `src/**/static/`（HTML／CSS／JS）嗎？→ 記得提醒部署端重編譯（資產是 `include_str!` 進 binary 的）。
- [ ] 新增守則了嗎？→ 對應的反向測試呢？
- [ ] `CHANGELOG.md` 補了嗎？（用 `[Unreleased]`，標記名要對得上程式碼）
- [ ] 這次改動有沒有動到「對外可見的東西」（`/healthz`、公開頁、推播文案）？

## 8. 已知工程債（誠實列出，歡迎收拾）

| # | 內容 | 影響 | 收拾方式 |
| --- | --- | --- | --- |
| ~~1~~ | ~~`checks/` 內多數工具預設掃描 `/home/user/tronclass_panel`~~ **已解決（2026-09-18）**：工具改為從自己的位置推算 repo 根（`<repo>/checks` → `<repo>`），並隨程式碼一起進版控 | — | — |
| 2 | 尚未全面 `cargo fmt` | `cargo fmt --check` 目前不擋 CI | 單獨一次「只做排版」的 commit，之後把 CI 的 `continue-on-error` 拿掉 |
| 3 | clippy 警告尚未歸零 | 同上 | 分批收斂，然後改成 `-D warnings` |
| 4 | 601 個測試都在 `src/` 的 `#[cfg(test)]` 裡 | 沒有 integration test 目錄，跨模組流程只能靠 `checks/` 的模擬 | 把 `checks/` 的關鍵情境搬進 `tests/` |
| 5 | 版本號停在 0.1.1 | `env!("CARGO_PKG_VERSION")` 會出現在 `/healthz` 與啟動訊息 | 真的有對外不相容變更時再升 0.2.0，並在 CHANGELOG 說明 |
