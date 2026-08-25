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
