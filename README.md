# Tronclass Rollcall

> Tronclass 自動點名與作業截止追蹤系統

以 Rust 撰寫的多帳號 Tronclass 自動化工具。持續監控點名活動並自動完成簽到，同時追蹤各科作業截止時間並在期限前發送階梯式提醒。提供 Web Dashboard、Discord Bot、LINE Bot 三種操作介面。

**版本** `0.1.1` ｜ **語言** Rust 2021 ｜ **規模** 51 個原始檔、約 36,000 行、330 個單元測試

---

## 目錄

- [主要功能](#主要功能)
- [支援學校](#支援學校)
- [快速開始](#快速開始)
- [設定檔](#設定檔)
- [CLI 指令](#cli-指令)
- [Web Dashboard](#web-dashboard)
- [Discord 指令](#discord-指令)
- [作業截止追蹤](#作業截止追蹤)
- [架構](#架構)
- [安全性](#安全性)
- [部署](#部署)
- [開發](#開發)
- [免責聲明](#免責聲明)

---

## 主要功能

### 自動點名

支援 Tronclass 四種點名類型：

| 類型 | 處理方式 |
|---|---|
| **雷達簽到** | 依設定座標自動送出；失敗時用圓交叉點演算法（三邊測量）推算教室位置並重試 |
| **QR Code** | 產生本機掃碼頁，一次掃描可供同一堂課的多個帳號共用 |
| **數字簽到** | 等待使用者從 Web / Discord 輸入 4 位數字碼（**爆破已停用**） |
| **自主報到** | 偵測到即自動送出 |

多帳號同時處理同一堂點名時，會依序錯開送出時間（預設每個帳號間隔 0.5 秒），避免同時湧入。

### 作業截止追蹤

- 自動彙整各科作業與測驗的截止時間
- **只顯示當前學期**的課程（六層學期判斷邏輯，詳見下方）
- 截止前 **24 小時 / 6 小時 / 1 小時** 三階段提醒
- 老師延期時自動重置提醒（僅在延期超過 6 小時時觸發，避免微調造成重複通知）

### 通知管道

- **Discord** — DM 推送，附互動按鈕
- **LINE** — Flex Message 卡片
- **Web Push** — 手機背景推播（PWA）

所有通知皆可在 Web 通知設定頁個別開關。

### 其他

- 課表管理與上課前提醒
- 教材雲端硬碟（WebDAV）
- 登入紀錄、檔案操作稽核、IP 黑名單
- Passkey（WebAuthn）登入

---

## 支援學校

| Provider | 學校 | 登入方式 |
|---|---|---|
| `fju` | 輔仁大學 | CAS + 驗證碼 OCR |
| `tku` | 淡江大學 | NEAI SSO |
| `tronclass` | 通用 Tronclass | 尚未內建登入流程 |

> 輔大的驗證碼辨識需要 `ocr` feature（預設啟用），該功能依賴 `ddddocr`，需要 `libstdc++`。精簡容器請改用 `--no-default-features` 編譯，但如此便無法自動登入輔大。

---

## 快速開始

### 1. 編譯

```bash
# 一般環境（含驗證碼 OCR）
cargo build --release --features ocr

# 精簡容器（無 libstdc++／g++）
cargo build --release --no-default-features
```

### 2. 產生設定檔

```bash
./tronclass_rollcall init
cp config/config.toml.example config/config.toml
```

### 3. 新增帳號

```bash
# 建議：密碼從 stdin 讀取，不留在 shell 歷史紀錄
echo '你的Tronclass密碼' | ./tronclass_rollcall account add my-account \
  -p tku \
  -u 414850098 \
  --password-stdin \
  --discord-user-id 123456789012345678 \
  --admin
```

帳號資料存於 SQLite（`config/accounts.db`），首次啟動自動建立。

服務執行中也可以直接從 Web 管理頁（管理員 → 新增帳號）或 Discord `/add-account`／申請核准流程新增帳號：寫入資料庫後會立即登入並開始監控，**不需要重啟服務**。CLI `account add` 是離線寫入資料庫，新增後仍需重啟（或在管理頁／Discord 再新增一次相同帳號會因 id 重複而失敗，請改用重啟）。

### 4. 驗證與啟動

```bash
./tronclass_rollcall --validate   # 只檢查設定，不啟動
./tronclass_rollcall              # 正式啟動
```

---

## 設定檔

`config/config.toml`：

```toml
[providers.tku]
kind = "tku"
base_url = "https://iclass.tku.edu.tw"

[providers.tku.api]
poll_interval_secs = 10        # 點名輪詢間隔
request_timeout_secs = 30

[providers.tku.radar]
default_coords = [
    [25.1745, 121.4490],       # 預設雷達座標
]
accuracy = 35

[providers.tku.qrcode]
scan_timeout_secs = 60

[providers.tku.schedule]
periods = ["07:10~17:30"]      # 只在這些時段輪詢
rest_weekdays = ["sun"]
# 註：週六、週日、臺灣國定假日（內建 2026–2027 年）以及寒暑假（內建淡江 115 學年度：
# 9/14 開學、1/18–2/21 寒假、2/22 開學、6/28 起暑假）整段不自動輪詢，開學日自動恢復。
# 要加校慶／颱風假、補課日或自訂停課區間，請建立 config/holidays.json：
#   { "extra": ["2026-11-20"], "workdays": ["2026-04-06"],
#     "breaks": [{"from":"2027-06-28","to":"2027-09-12","name":"暑假"}],
#     "disable_builtin_breaks": false }
# （每 5 分鐘自動重讀；手動「立即檢查」不受休息日限制）
# 實際生效的假日／寒暑假／月曆可在「設定 → 其他設定 → 休息日與寒暑假」（/admin/rest-days）查看。

[adapters.discord]
enabled = true
bot_token = "..."
admin_user_id = "..."
public_base_url = "https://your-domain.example"

[adapters.admin_web]
enabled = true

[time]
timezone = "Asia/Taipei"

[logging]
level = "info"

[monitor]
startup_delay_secs = 3
retry_interval_secs = 30
max_failures_before_reauth = 5
```

環境變數 `TRONCLASS_ROLLCALL__PROVIDERS__<name>__BASE_URL` 可覆蓋設定值；`RUST_LOG` 控制日誌等級。

---

## CLI 指令

```
tronclass_rollcall [選項] [設定檔路徑]
tronclass_rollcall init [--force]
tronclass_rollcall account <子命令>
```

### 帳號管理

| 指令 | 說明 |
|---|---|
| `account list` | 列出所有帳號 |
| `account show <id>` | 顯示帳號詳情 |
| `account add <id>` | 新增帳號（`-p` provider、`-u` 帳號為必填；密碼用 `-P` 或 `--password-stdin` 擇一提供，後者不留 shell 歷史紀錄） |
| `account bind <id>` | 更新 Discord / LINE 綁定 |
| `account admin <id>` | 授予管理員權限 |
| `account unadmin <id>` | 移除管理員權限 |
| `account dashboard-password <id> --stdin` | 設定獨立 Dashboard 密碼 |
| `account enable\|disable <id>` | 啟用／停用 |
| `account remove <id>` | 刪除 |
| `account reencrypt` | 把所有既有帳號的密碼／憑證重新寫入（啟用 at-rest 加密） |
| `account rotate-secret [--dry-run]` | Master key 輪替：把所有密文一次收斂到目前 `TRONCLASS_MASTER_KEY`，並列出解不開的帳號；`--dry-run` 只檢查不寫入。無錯誤結束後即可移除 `TRONCLASS_MASTER_KEY_OLD` |

### 選項

| 選項 | 說明 |
|---|---|
| `-c, --config <PATH>` | 設定檔路徑（預設 `config/config.toml`） |
| `-a, --accounts <PATH>` | 帳號資料庫（預設 `config/accounts.db`） |
| `--validate` | 只驗證設定，不啟動 |
| `-v, --version` / `-h, --help` | 版本／說明 |

---

## Web Dashboard

### 第一次進入網站（新手導覽）

1. 打開 `https://<你的網域>/admin`，帳號輸入**帳號 ID（或學號）**，密碼輸入 **Tronclass 密碼**（首次登入專用）。
2. 系統會要求你設定一組**Dashboard 專用密碼**（至少 10 字），之後登入這個網站只能用它，不再接受 Tronclass 密碼。
3. 設定完成後會自動彈出**新手導覽**，依序帶你看：帳號與監控狀態 → 點名怎麼進行（雷達全自動／QR Code 要掃描／數字碼要輸入，兩者都有倒數計時）→ 接通知（Discord 綁定、手機推播）→ 填課表 → 通行密鑰與外觀設定。每一步都會標示你「已完成／尚未」，並有「前往」按鈕直接跳到對應頁面。
4. 導覽每個瀏覽器＋帳號只自動出現一次；之後可從頂欄「?」、手機「更多 → 新手導覽」或在網址加 `#guide` 隨時重看。

啟用 `[adapters.admin_web]` 後，於 `webhook_port`（預設 8080）提供服務。

| 路徑 | 頁面 |
|---|---|
| `/admin` | 登入 |
| `/admin/dashboard` | 總覽、帳號、點名、課表、教材 |
| `/admin/homework` | **作業截止追蹤** |
| `/admin/timetable` | 課表編輯 |
| `/admin/files` | 教材雲端硬碟 |
| `/admin/activity` | 登入與檔案紀錄（管理員） |
| `/admin/blacklist` | IP 黑名單（管理員） |
| `/admin/health` | 系統健康狀態 |
| `/admin/passkey` | 通行密鑰設定 |
| `/admin/push-status` | 手機推播狀態 |

**首次登入**：使用 Tronclass 密碼，系統會強制要求設定一組獨立的 Dashboard 密碼。之後 Tronclass 密碼僅供背景監控使用，無法再用於網站登入。

---

## Discord 指令

| 指令 | 說明 |
|---|---|
| `/status` | 查看監控狀態 |
| `/start` `/stop` | 開始／暫停監控 |
| `/force` | 立即檢查一次點名 |
| `/reauth` | 重新登入 Tronclass |
| `/num <code>` | 送出數字點名碼 |
| `/qr <url>` | 送出 QR Code 內容 |
| `/bindings` | 查看帳號綁定 |
| `/admin` | 管理員操作 |
| `/help` | 說明 |

---

## 作業截止追蹤

### 資料來源

依序查詢並去重：

1. `GET /api/todos` — 全域待辦
2. `GET /api/my-courses` — 修課清單
3. 每門課併發查詢 `homework-activities` 與 `activities`（並行上限 8）

### 當前學期判斷

各校欄位差異極大，採六層推斷，任一層命中即採用；全部無法判斷時**保留**該課程（寧可多顯示也不漏掉）：

1. 封存旗標（`is_archived` / `is_ended` / `is_current` …）
2. 狀態字串（`archived` / `ongoing` …）
3. 課程結束日期已過
4. `course_code` 前綴學期碼（如 `1151…` = 民國 115 學年第 1 學期）
5. 學年／學期欄位（支援巢狀物件如 `{"name":"115"}`）
6. 開課日仍在未來

第二道防線：截止時間早於**本學期起點**的作業一律不顯示。

### 效能設計

| 機制 | 說明 |
|---|---|
| **背景預抓** | 每 4 分鐘更新一次（短於 5 分鐘快取 TTL），確保開頁即命中 |
| **共享快取** | 網頁與排程器共用，single-flight 確保併發請求只打一次上游 |
| **SQLite 快照** | 程式重啟後仍能立即顯示上次結果 |
| **stale-while-revalidate** | 先顯示既有資料，背景靜默更新 |

### 提醒時機

剩餘 **24 小時 / 6 小時 / 1 小時** 各發一次，超過 24 小時不發。以 `(帳號, 作業, 階段)` 為鍵記錄於 SQLite，重啟不會重複發送。精度約 4 分鐘。

---

## 架構

```
src/
├── main.rs                  進入點、CLI、排程器啟動
├── config.rs                設定載入與驗證
├── account.rs               帳號模型
├── db.rs                    SQLite（16 張表）
├── monitor.rs               主監控循環
│
├── auth/                    認證
│   └── providers/           fju（CAS+OCR）、tku（NEAI SSO）
│
├── api/                     Tronclass API 客戶端
│   ├── rollcall.rs          點名 API 與資料結構
│   ├── todos.rs             作業／待辦解析
│   └── profile.rs
│
├── rollcalls/               點名處理
│   ├── radar.rs             雷達（含圓交叉點演算法）
│   ├── qrcode.rs            QR Code 解析
│   ├── number.rs            數字碼
│   └── group_submit.rs      多帳號送出協調
│
├── adapters/                對外介面
│   ├── admin_web/           Web Dashboard（axum）
│   ├── discord.rs           Discord Bot（serenity）
│   ├── line/                LINE Bot
│   ├── scanner.rs           共用 QR 掃碼伺服器
│   └── events.rs            adapter 無關的訊息模型
│
├── homework_reminders.rs    作業提醒排程 + 預抓器
├── homework_cache.rs        作業清單共享快取
├── class_reminders.rs       上課前提醒
├── detection_schedule.rs    課表驅動的偵測排程
├── web_push_notifications.rs  Web Push（純 Rust 加密）
├── dashboard_auth.rs        Dashboard 密碼雜湊
└── logs.rs                  記憶體 + SQLite 日誌
```

### 主要依賴

`tokio` · `axum` · `reqwest` · `sqlx` · `serenity` · `chrono` · `serde` · `miette` · `p256` / `aes-gcm` / `hkdf`（Web Push，避免 OpenSSL 依賴）

---

## 安全性

- **密碼分離** — Dashboard 密碼與 Tronclass 密碼各自獨立，PBKDF2-HMAC-SHA256（100,000 輪、128-bit salt）
- **CLI 密碼輸入** — `account add --password-stdin` 與 `account dashboard-password --stdin` 從 stdin 讀取密碼，不留在 shell 歷史紀錄與程序引數清單
- **速率限制** — 單帳號 5 次、單 IP 20 次失敗即限流
- **自動封鎖** — 連續失敗達門檻自動加入 IP 黑名單並發出安全警報
- **Passkey** — 支援 WebAuthn 免密碼登入
- **檔案權限** — 程式啟動時設定 `umask 0077`，確保新建的資料庫與密鑰檔為 0600
- **敏感資料隔離** — `config.toml`、`accounts.db`、各類 token 皆列於 `.gitignore`
- **靜態加密與 Master Key 輪替** — 上游密碼與 session cookie 以 AES-256-GCM 加密後才寫入資料庫（`TRONCLASS_MASTER_KEY`，64 hex）。支援免停機輪替：

  ```bash
  # 1. 產生新 key（Pterodactyl 部署：新 key 寫入 config/master.key）
  openssl rand -hex 32
  # 2. 舊 key 放進環境（多把以逗號分隔；或 ptero 部署寫入 config/master.old.keys，一行一個）
  export TRONCLASS_MASTER_KEY=<新 key>
  export TRONCLASS_MASTER_KEY_OLD=<舊 key>
  # 3a.（建議）一次到位：先 dry-run 檢查，再實際輪替；結束碼 0 = 全部成功
  tronclass_rollcall account rotate-secret --dry-run
  tronclass_rollcall account rotate-secret
  # 3b.（或）直接重啟服務——讀取帳號時自動把舊密文背景重加密為新 key（背景靜默執行，
  #    不發 info 通知；需要排查時可開 debug 層級檢視「已重加密」細節）
  # 4. rotate-secret 回報無錯誤（或所有帳號都出現過該 log）後，移除 _OLD 環境變數／master.old.keys 再重啟
  ```

  新寫入的密文為 `enc:v2:<key_id>:…` 信封（key id＝SHA-256(key) 前 8 bytes，無法反推 key）；舊 `enc:v1:` 信封在新週期內逐步重加密，完成後舊 key 即可退役。

> ⚠️ 帳號資料庫依設計會儲存 Tronclass 上游密碼（背景登入所需）。請確保 `config/` 目錄權限正確，且**絕不上傳至公開儲存庫**。

---

## 部署

### Pterodactyl 面板

專案內附 `ptero-start.sh`，於面板一鍵完成：

- 自動編譯並啟動主程式
- 啟動 Cloudflare Tunnel（讀取 `config/cf.token`）
- 自動備份／還原 `accounts.db`
- Watchdog 監控程序與 Tunnel 連線狀態

搭配 `sync-to-github.sh` 可將程式碼推送至 GitHub（自動排除個資與密鑰）。

### 反向代理

Dashboard 預設綁定 `127.0.0.1`，建議透過 Cloudflare Tunnel 或 Caddy 對外。若需直接對外，請在設定中明確指定 `webhook_bind_addr = "0.0.0.0"`。

---

## 開發

```bash
cargo test                            # 執行 330 個測試
cargo test --lib db                   # 只測資料庫
cargo clippy --all-targets            # Lint
cargo build --release --features ocr  # Release
```

前端頁面以 `include_str!` 編譯進二進位檔，**修改 HTML/CSS/JS 後必須重新編譯**（`theme.css` 例外，支援執行期覆寫）。

---

## 免責聲明

本專案僅供**技術研究與學習用途**。

自動化點名可能違反學校的學術誠信規範或資訊系統使用條款。使用者需自行承擔一切後果，作者不對任何濫用行為負責。請遵守所屬學校規定。

---

## Credits

點名邏輯部分參考 [KrsMt-0113/XMU-Rollcall-Bot](https://github.com/KrsMt-0113/XMU-Rollcall-Bot)。
