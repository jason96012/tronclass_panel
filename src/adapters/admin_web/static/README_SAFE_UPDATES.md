# Admin Web 靜態頁更新規範

這個目錄存放 Admin Web 的 HTML 頁面。

## 重要規則

1. 後續更新畫面請優先修改這裡的 `.html` 檔。
2. 不要再把大量 HTML / JS / CSS 塞回 Rust raw string。
3. Rust 端應只保留：

```rust
const ADMIN_XXX_HTML: &str = include_str!("static/xxx.html");
```

或舊 monolith：

```rust
const ADMIN_XXX_HTML: &str = include_str!("admin_web/static/xxx.html");
```

4. 每次 patch 前要備份，patch 後請跑：

```sh
./check_admin_web_static.sh /home/jason/tronclass_rollcall
```

5. 修改 Dashboard / 教材 / 手機 UI 時，請使用唯一 marker，例如：

```html
<!-- MY_PATCH_V1_START -->
...
<!-- MY_PATCH_V1_END -->
```

同一個 marker 更新前先移除舊區塊，避免 JS 重複綁定。

6. Service Worker 不要加入 fetch interception / cache / redirect，除非非常確定。

## 常見檔案

- `admin.html`：Dashboard 主入口
- `dashboard.html`：Dashboard 主畫面
- `files.html`：教材雲端硬碟
- `health.html`：健康狀態或導向維護分頁
- `activity.html`：登入與檔案操作紀錄
- `logs.html`：系統紀錄頁
- `me.html`：一般使用者我的頁面

## 手機鏡頭／瀏海安全區（SAFE_AREA_TOP_V1）

所有頁面的 viewport 都帶 `viewport-fit=cover`（畫面延伸到系統列底下），
因此**每頁**都必須把頂部內容推回安全區，否則會被鏡頭／瀏海蓋住。三種手法：

1. **sticky 頂欄頁**（dashboard/activity/files/logs/bulletins/blacklist/timetable）：
   對頂欄加 `border-top:env(safe-area-inset-top,0px) solid transparent`
   （dashboard 在 `css/dashboard.css` 的 `.topbar`；其餘在頁內 `SAFE_AREA_TOP_V1` style 區塊）。
   用 border 而非 padding 的原因：媒體查詢裡的 `padding:...` 簡寫不會蓋掉 border，
   不必逐個斷點重算。順帶加 left/right border 處理橫向劉海。
2. **`.shell` 容器頁**（detection_schedule/homework，同 devices/rest_days/update_times 舊模式）：
   mobile media query 裡 `.shell{padding:calc(原值 + env(safe-area-inset-top,0px)) ...}`。
3. **靜態頂欄／無頂欄頁**（notifications/settings/push_status/health/blocked/shares/login
   與 `adapters/line/{scanner,number}.html`）：`body{padding-top:env(safe-area-inset-top,0px)}`
   （health=calc(18px+env)、blocked=calc(24px+env)，因原 body padding-top 非零）。

已由前輪修好、勿重複加：ai、passkey、password、devices、rest_days、update_times。
無劉海裝置 `env()`=0 → 外觀完全不變；新頁面請沿用同一 pattern。
