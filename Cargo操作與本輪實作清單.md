# Cargo 操作與本輪實作清單

## 先讀：目前並非全部完成

本輪依你原先的限制**沒有執行 Cargo、測試或服務**，以下命令由你決定是否在隔離環境操作。這不是上線簽核。請先使用完整專案副本，不使用正式帳號、正式資料庫、學校 API、正式 master key 或真實使用者資料。不要上傳 Cargo 輸出中的憑證、token、cookie、帳號或私鑰。

完整 src ZIP **不含** `Cargo.toml`、`Cargo.lock` 和 `tools/`；另附的本輪補充檔 ZIP 包含這些更新，須合併到相同版本的完整專案根目錄。注意本專案有多輪未提交修改，**不要把 src 套在不同版本專案**。操作前先備份你的工作樹，並保留現有設定。`Cargo.lock` 的 ddddocr source 本輪依固定 revision 手工同步，只有 Cargo 才能驗證解析一致；不要先在正式環境覆蓋或建置。

## 本輪實作

- Session：帳號整體撤銷回傳資料庫結果；偵測 refresh 重播時不再在 DB 撤銷失敗時宣稱所有裝置已登出。失敗產生不含帳號或 token 的結構化 `security_event`。其他既有呼叫方仍可能忽略返回值；**跨程序／crash-safe／單次使用／session family／原子交易仍未實作**。即使 DB 刪除成功，其他程序的記憶體快取仍可能有效。
- Passkey：有帳號模式找不到 credential 與驗證失敗都使用共同的未授權回應；新增註冊 audit 不再記錄 credential ID，但 API 本身維持回傳 credential ID 以免破壞既有功能。這不能消除時序、狀態碼、帳號枚舉或所有稽核個資；舊日誌也未刪除。相容性未測。
- 輸入／限流：管理節流 SQLite 故障新增固定欄位的本地事件，原本失敗拒絕行為不變。Scanner 的欄位一致上限與 IP 表硬上限**本輪未實作**，避免在缺少合法資料與共享 NAT 相容性證據下擋下已收到的點名。既有 Passkey 輸入驗證與登入鎖保護不代表全面完成。
- VAPID：在原本舊明文加密遷移之外，首次建立設定改成 insert-if-absent，避免兩個初始化者互相覆寫新金鑰；競爭失敗者暫停 Web Push，需重新初始化。獨立金鑰分層、輪換與歷史備份處理**未實作**；這不是跨程序完整同步方案。既有明文 DB 頁面、WAL、備份仍可能含秘密。
- SIEM：本機白名單匯出新增帳號 session 撤銷失敗、管理節流 DB 故障事件，不輸出身份或 token。仍依賴可能過濾、排隊丟失、只保留約 100 筆的 `web_logs`；無可靠游標、持久投遞、完整事件覆蓋或告警。這是雛形，**不是完成的 SIEM**。
- 供應鏈：`ddddocr` 的 Git branch 改成現有 lockfile 所記錄的 commit `cc249cc4d79c6d609d808c540689f0187257a74f`；lock 的 source 字串手工同步，待 Cargo 驗證。固定 commit 不是來源審計。cargo-deny policy、SBOM、簽章、CI、漏洞逐項評估**未完成**；不產生、不保存私鑰。

## 1. 確認工作樹與工具鏈

在隔離副本的專案根目錄：

```sh
pwd
git status --short
rustc --version
cargo --version
```

確認 `Cargo.toml` 和 `Cargo.lock` 的 ddddocr revision 同為上述 commit，`src` 為本次 ZIP 的版本。如果本地已存在未提交修改，不要覆蓋；先人工比對。`cargo` 可能下載依賴／存取 crates.io 和 GitHub；若環境不允許網路，可先試 `--offline`，缺快取時停止並回報，不要修改正式部署。

## 2. 依序執行編譯檢查

```sh
cargo check --locked --no-default-features
cargo check --locked --no-default-features --features ocr
```

OCR 是可選功能，第二項只在你需要 OCR、隔離機器具備對應建置依賴及允許下載該 Git 來源時執行；失敗可能是系統函式庫、工具鏈或版本問題。若 `--locked` 提示 lockfile 須更新，**停止並提供錯誤**；不要直接移除 `--locked` 後拿結果上線。若你在備份後明確選擇重新產生，請在隔離副本依 Cargo 訊息操作，回傳更新前後的 `Cargo.lock` 差異供核對。

## 3. 僅針對本輪相關測試

先跑隔離測試，以下篩選器對應現有測試模組；Cargo 可能先編譯整個 test binary。注意輸出中的 `0 passed` 代表沒有匹配到測試，**不是通過**。

```sh
cargo test --locked --no-default-features fallback_login_lock_capacity_tests -- --nocapture
cargo test --locked --no-default-features setting_compare_exchange_tests -- --nocapture
cargo test --locked --no-default-features passkey_ceremony_input_tests -- --nocapture
cargo test --locked --no-default-features rate_limit_response_header_tests -- --nocapture
```

其中登入鎖與 DB 條件更新測試是前一輪新增，第三項驗證 Passkey ceremony ID。VAPID insert-if-absent 的 SQL 測試位於 `setting_compare_exchange_tests`。本輪尚未有撤銷故障、Passkey 回應、VAPID 真正加密／競爭、匯出功能的完整測試；上述命令無法證明那些行為。**不要直接執行未審查的全套測試**：部分測試可能有網路、環境或資料庫副作用。

## 4. 依賴稽核與 SBOM（可選，需網路）

先人工確認工具來源、版本與組織政策；以下安裝會從網路下載並在執行環境安裝工具。請不要以 root 在正式容器操作。

```sh
cargo install --locked cargo-audit
cargo audit
cargo install --locked cargo-cyclonedx
cargo cyclonedx --help
```

請依當前工具 help 選 JSON／CycloneDX 與適當的 feature/target 後，在隔離副本產出 SBOM。預設 feature 與 OCR feature 的依賴不同，兩種部署都需分別覆蓋。`cargo audit` 的版本命中不等於可利用；請保留結果，逐條檢查路徑、feature 與修補相容性。`cargo-deny` 需要先制定授權／來源／advisory 政策，不能在未審核時盲目生成一份寬鬆配置。簽名要等你決定 CI 與公鑰驗證方案，不要把私鑰交給我或貼入對話。

## 5. 請回傳

請回傳**遮蔽敏感資訊**的每項命令、exit code、完整錯誤與測試數量；若 lockfile 改動，請提供差異。若測試或編譯失敗先停止，不要上線。我會依結果修正後續問題。單靠上述命令仍不能驗證多程序、refresh 競態、DB 故障、Passkey 客戶端、VAPID 遷移或真實點名。

## 本輪我實際執行的檢查

Rust 檔案以 tree-sitter 做語法解析、TOML 用 Python `tomllib` 解析、匯出工具 Python AST 解析，以及 `git diff --check`；未執行上述 Cargo 命令、測試或服務。語法解析不保證型別正確或運行安全。
