# Changelog

本檔案記錄**對外可見**的變更。格式參考 [Keep a Changelog](https://keepachangelog.com/zh-TW/1.1.0/)，
版號依[語意化版本](https://semver.org/lang/zh-TW/)。

## 這個專案的記法

* **`fixNN`／`#NN`＝交付批次編號**：每個批次是一份 zip（`src_YYYYMMDD_fixNN.zip`）＋一份報告，
  對應一次「改檔 → 檢查 → 打包 → 部署端建置」的循環。批次號只出現在這裡，
  程式碼裡不留批次號（程式碼裡用的是下面那種設計標記）。
* **設計標記**（例如 `RADAR_SCHOOL_BLOCK_V1`）＝程式碼裡 `SCREAMING_SNAKE_CASE_V1` 的註解標記，
  代表「這一段是為了某個設計決定而寫的」。用它在程式碼裡搜尋，就能從一行註解追到這裡的說明。
  取名原則：**描述功能**，不要用日期或批次號（那些會過期，功能不會）。
* 版本號還沒升到 0.2.0 的原因：`env!("CARGO_PKG_VERSION")` 會出現在 `/healthz` 與啟動訊息，
  屬於對外可見的變更，等真的有對外不相容的變更時再一起升（見 [Unreleased] 的說明）。

---

## [Unreleased]

### 修正 — QR 掃碼頁「坐遠掃不到」與「解不到碼卻毫無反應」（fix78，2026-09-18）

使用者回報：手機鏡頭對準課堂 QR Code，但掃碼頁完全沒有任何訊息，就像沒對到一樣。
根因（實測）：頁面只在 `jsQR` 成功解碼時才有反應，解不到時是完全靜默的；
而原先的「鏡頭縮放」滑桿只在相機回報支援 `zoom` 時才出現，多數手機（含 iPhone）等於沒有拉桿可用。

* **QR_ZOOM_V1 — 放大拉桿一律可用**：兩段式放大，鏡頭變焦優先（真的光學／感光變焦），
  不足的倍率用數位裁切補；拉桿值＝總倍率，預覽同步放大（畫面看到的就是實際送去解碼的範圍），
  位置存在 `localStorage`。新增 −／＋／回到 1x 按鈕，手機單手可操作。
* **QR_ZOOM_V1 — 高解析度擷取**：相機改要求 3840×2160。這是「坐遠」真正的關鍵：
  感光元件多取到的像素才會讓遠處的 QR 有足夠模組像素被解出來（實測 100 px 的 QR 在
  1080p 下解不到，4K 下解得動）。
* **QR_ZOOM_V1 — 解碼迴圈重寫**：只解「中央 1/倍率」那一塊、最多每 150 ms 解一次、
  同一時間只有一個解碼在跑；每秒一次走完整模式（含反相 QR），其餘走快速模式。
  以前每一幀都解整張 1920×1080（桌機 110–160 ms／次），手機上只有 1–3 fps，看起來像當掉。
* **QR_ZOOM_V1 — 解碼回饋**：新增 `#scanHint`：解到碼立刻顯示「✅ 已解出 QR」，
  連續 2.5 秒沒解到就提示「還沒看到 QR：把 QR 放進畫面正中央／往右拉放大」。
  頁面下方並顯示「相機解析度／解碼區大小／目前解碼器」，距離與拉桿該不該再調一目了然。
* **QR_ZOOM_V1 — 解碼器**：優先用瀏覽器內建 `BarcodeDetector`（Android Chrome；對小碼與模糊
  容忍度較好），否則用 jsQR。
* **QR_DECODER_LOCAL_V1**：jsQR 改由本機 `/scanner/jsqr.js` 載入（與原本 CDN 那份 sha256 相同），
  移除對 jsdelivr 的相依；並移除兩個頁面檔尾被 Cloudflare 注入的殘留腳本。
* **QR_SUBMIT_TIMEOUT_V1**：`POST /scanner/submit` 加上 8 秒逾時與 `AbortController`。
  以前 fetch 沒有逾時，手機換網路／休眠時 promise 可能不結束，`submitInFlight` 會永久卡在 true，
  之後每一次掃到的碼都被靜默丟掉；現在逾時會明說並解鎖。

守則：`checks/qr_zoom_sim.mjs`（新，33 項）、`checks/deploy_preflight.sh`（新增 6 條 QR_ZOOM_V1 內容檢查）。
檢查套件總數 49 → 50，全綠。


### 新增 — 工程表面（2026-09-18，不影響執行檔行為）

把「本來靠人記」的流程變成檔案：`Makefile`、`rustfmt.toml`、`clippy.toml`、`rust-toolchain.toml`、
`.editorconfig`、`.github/workflows/ci.yml`、`CONTRIBUTING.md`、`tools/release.sh`、
`tools/git-hooks/commit-msg`、`.gitmessage`。

* 建置參數統一為 `cargo build --release --no-default-features`（與 `ptero-start.sh` 一致）。
* 交付流程從「手打指令」變成 `make verify`（排版／clippy／測試／49 支檢查／preflight）＋ `make pack`。
* 打包腳本會產生 `SHA256SUMS` 並只保留最新一份 zip（原本是人工刪舊檔）。
* **檢查套件（`checks/`，49 支）正式進版控**：以前它住在專案外面，只有本機某個路徑有；
  現在與程式碼同一個 repo，並改成從自己的位置推算專案根目錄——
  換機器、換目錄、CI 都能跑（已用「複製到別的位置再故意改壞」驗證過）。
* 交付 zip 內容調整：`src/ config/ tools/ checks/` ＋ 工程表面檔案，並附上 `SHA256SUMS`
  （部署端可 `sha256sum -c` 確認傳輸過程沒有壞檔）。

### 修正 — 編譯（2026-09-18，fix77 修正批次）

* **E0425 ×8**：`src/api/rollcall.rs` 的兩個模組層級輔助函式被誤放進 `impl ApiClient` 區塊，
  變成關聯函式而方法裡用裸名呼叫 → 已移回模組層級。
* 新增 `checks/rust_scope_check.py`（`RUST_SCOPE_V1`）＋反向測試，專門抓「函式被包進 impl」這類錯誤
  （語法與其他檢查都看不到，只有 rustc 會報）。
* 移除 `src/api/mod.rs` `is_auth_error()` 開頭重複的 `let`（會產生 `unused_variables` 警告）、
  清掉停手推播內文多餘的空白；`checks/rust_lint_static.py` 新增 R3／R4 兩條規則防止再犯。

### 新增 — 自動點名（2026-09-18，fix77）

* **距離探測**（`RADAR_DISTANCE_PROBE_V1`）：雷達只剩下 1 筆距離量測時，系統會以那筆量測為錨點，
  往 0°／120°／240° 各送一個 150 公尺外的探測點（間隔 1.2 秒、10 公尺內不重複送、
  整理場點名每個帳號只做一輪），湊到 4 筆量測後直接用圓交會／最小平方解出教室位置，
  不必再從棋盤格一格一格掃。
  離線模擬：1.8 公里外的教室由「1,186 個請求／39 分鐘」變成「6 個請求／約 12 秒」。
* **學校端 429／403 停手＋推播**（`RADAR_SCHOOL_BLOCK_V1`）：
  學校回 429（限流）或 403（禁止）時，同一場點名的**所有帳號**一起停手
  （預設 30 分鐘；學校有給 `Retry-After` 就照它，並限制在 60–3600 秒），
  同時**推播管理員一次**（每場、每個狀態碼只推一次）。403 不再被誤判成「session 過期」。
  停手時間到會自動恢復重試；頁面會顯示停手橫幅並鎖住送出鍵。

## [0.1.1] — 2026-09-18

### 自動點名與雷達

* 新增雷達點名的「系統演算」路徑：空答案優先、輸入頁座標優先、圓交會、棋盤格掃描
  （`RADAR_SYSTEM_ONLY_V1`、`RADAR_INPUT_PAGE_V1`、`RADAR_RELAY_V1`）。
* 底圖讀不到圖磚時自動退回格線圖並說明原因；有底圖時不再疊格線
  （`BASEMAP_FALLBACK_V1`、`GRID_ONLY_WITHOUT_BASEMAP_V1`、`GRID_STEP_LADDER_V1`）。
* 雷達地圖預設一般地圖、可切衛星影像；地圖上畫出「教室位置是怎麼推出來的」
  （量測圓／候選點／推估值／棋盤格）（`RADAR_BASEMAP_DEFAULT_STREET_V1`、`RADAR_DEDUCE_TRACE_V1`）。
* 座標來源以 provider 為單位記錄、手機與電腦同一頁、室內誤差處理（fix70／fix71）。
* 雷達超過班上 15% 門檻立刻重試（fix72）；每位送出之間固定間隔（`GROUP_SUBMIT_RADAR_*`）。

### 點名送出

* 班上出席率超過 15% 時自動送出，且**每個送出之間有間隔**、送出前重新取得最新 payload、
  3 秒冷卻不可移除、不得改用不同 IP 送出（fix73、#49）。
* 數字碼點名：不重抓數字碼、等待上限只給 QR、400／422 才救援重抓一次（fix69）。
* QR 點名：游標只前進、重試不推播、QR jitter 小於數字碼（fix67／fix68）。
* 自主報到也走同一條排隊（`SELF_REGISTRATION_RELAY_V1`）。

### 管理面板

* 點名紀錄清單不再無限延伸（一次 20 筆＋「顯示更多」＋固定高度可捲動）（fix76）。
* 手機版出席統計改成「一列一張卡」，每一欄（含最右邊）都看得到（fix76）。
* 點名頁上方欄位顯示「目前班上已點人數」（`CLASS_COUNT_HEADER_V1`）。
* 統計：只有管理員能查全部、查他人一律 401、檢視者遮罩、`days`／`limit` 收斂（fix61／fix62／fix66）。

### 教材與作業

* 教材匯入：上傳不限檔名／類型（作業 50 MiB）；公告附件顯示規則（fix59／fix64）。
* 作業頁採方案 B；繳交可重繳、管理員可代繳（fix55）。

### 通知

* 推播管理員（`notify_admins_*`）；通知 3 分鐘除重（fix68）。

### 其他

* SEO：公開頁（`/`、`/landing`）可收錄、登入與面板頁不收錄；`/healthz` 保留（fix47／fix48）。
* 黑名單、休假／寒暑假行事曆、評分同步（`tku_credits`）等既有功能修正。

### 修正

* **E0063／E0369**（fix76b）：`RadarStatusResult.probe` 補進結構體初始化；
  `RadarProbeTrace`／`RadarProbeGrid`／`RadarProbeMeasurement` 補 `#[derive(PartialEq)]`。
* 教材匯入在無 `libstdc++` 容器（`--no-default-features`）下的相依問題（fix59b）。

## [0.1.0] — 2026-09-14（初始版）

* Rust 2021 單一 binary：多帳號 Tronclass 自動點名、作業截止追蹤與階梯式提醒。
* 三種介面：Web Dashboard、Discord Bot、LINE Bot（`/line/webhook`）。
* 內建休假行事曆、推播（Web Push）、統計頁、PWA。

---

（各版本的比較連結刻意不寫在這裡：本專案的正式交付是 zip ＋ 部署端建置，
GitHub 只是鏡像，網址寫死反而會誤導。）
