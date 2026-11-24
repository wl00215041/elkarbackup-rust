# ElkarBackup Rust 改寫計畫

## 目標

把 PHP/Symfony 版 ElkarBackup（rsnapshot 為基礎的集中式備份系統）從頭以 Rust 重寫，資料庫改用 PostgreSQL。
不需相容舊資料庫 schema 或舊部署。

### 保留的功能

- 使用者（admin / 一般使用者，一般使用者只能看到自己擁有的 client）
- Client、Job、Policy（Hourly/Daily/Weekly/Monthly/Yearly 保留規則與排程）、Script（pre/post client/job）、Backup Location
- 排程器：依 policy 排入佇列 → client pre scripts → job pre scripts → rsnapshot → job post scripts → client post scripts
  - 全域 / 每 client / 每 backup location 的並行上限
  - 中止（abort）執行中的工作
  - 以 token 匿名觸發工作（`POST /client/{c}/job/{j}/run?token=...`）
- 瀏覽備份、下載檔案、下載目錄（tar.gz）、還原到遠端 client（rsync）
- 活動紀錄（log records）與 email 通知（依最低通知等級篩選）
- SSH 金鑰產生與公鑰下載
- JSON REST API（`/api/*`，HTTP Basic auth）
- Web 管理介面（伺服器端渲染）

### 刻意不做（非目標）

- 多語系（先只提供英文介面，模板集中，日後可加）
- 在 Web 上編輯系統參數（改由設定檔 / 環境變數）
- Dojo 前端與 Repository backup script 產生器

## 技術棧

| 用途 | 選擇 |
|------|------|
| async runtime | tokio |
| Web | axum 0.8 + axum-extra（cookie） |
| 模板 | askama（編譯期型別檢查） |
| DB | sqlx 0.8（PostgreSQL、runtime query，內建 migrations） |
| 密碼 | argon2 |
| CLI | clap 4（derive） |
| 錯誤 | thiserror（library）、anyhow（binary） |
| 設定 | serde + toml，環境變數覆寫 |
| 郵件 | lettre（SMTP，可選） |
| 日誌 | tracing |

## 架構（Cargo workspace）

```
crates/
  elkar-core     純領域邏輯：型別、驗證、policy 排程計算、rsnapshot 設定產生（無 I/O）
  elkar-storage  PostgreSQL repositories + migrations（sqlx）
  elkar-engine   執行層：ProcessRunner trait、scripts、備份執行、排程器、通知、還原
  elkar-web      axum：HTML UI、JSON API、認證/授權
  elkarbackup    binary：serve / tick / migrate / create-admin / generate-keypair
```

依賴方向：`web → engine → storage → core`，`core` 不依賴任何外部 I/O。

### 主要設計決定

1. **單一 daemon**：`elkarbackup serve` 同時提供 web 與排程器（每分鐘排入佇列，持續派工）。
   `elkarbackup tick` 提供一次性執行（cron 相容），兩者共用同一個 `Scheduler`。
   以 PostgreSQL advisory lock 保證同時只有一個排程器在跑。
2. **取代 fork/pcntl**：每個 job 是一個 tokio task；中止使用 `CancellationToken`，直接 kill 子行程，
   不再依賴 pid 檔。
3. **外部指令抽象**：`ProcessRunner` trait，正式實作用 `tokio::process`，測試用假的 runner，
   讓排程器狀態機可以在沒有 rsnapshot 的環境下測試。
4. **型別化排程**：舊版 policy 把排程存成 regex 字串（`"08:00|12:00"`）；新版以
   `Schedule { hours, days_of_week, days_of_month, months }` 表示，DB 用陣列欄位與 `policy_retains` 子表。
5. **狀態以 enum 表示**：`ClientState`、`QueueState`、`RetainLevel`、`NotificationLevel`、`Role` 等，
   DB 存成 TEXT + CHECK constraint。
6. **Session**：自建 `sessions` 表 + HttpOnly cookie（隨機 token），不額外引入 session framework。
7. **路徑安全**：瀏覽/下載備份時以 canonicalize 後的前綴比對防止 path traversal。

## 驗證方式

- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`
- `cargo test`：core 單元測試、storage 整合測試（`#[sqlx::test]`）、engine 排程器測試（假 runner）、
  web 路由測試（`tower::ServiceExt::oneshot`）
- 實際啟動 `serve`，以 curl 登入、建立 client/job，觸發一次以本機路徑為來源的備份（需 rsnapshot）

## 執行結果（2026-10-29）

- 全部 crate 完成；`cargo test --workspace` 65 個測試通過，`clippy -D warnings`、`fmt --check` 乾淨。
- 端對端煙霧測試：實際啟動 `serve`，經 Web 建立 client/job、手動觸發、排程器執行（以自寫的
  rsnapshot 替身呼叫真實 rsync）、瀏覽/下載檔案與 tar.gz、還原、script 上傳與 pre/post 執行、
  `tick` 在 daemon 持有鎖時正確略過、SIGTERM 優雅關閉。
- 安全性修正：一般使用者不可設定 ssh/rsync 參數；job path 必須為絕對路徑；client url 不可以 `-` 開頭。
- 尚未以真正的 rsnapshot 執行驗證（環境未安裝）。

## Docker 部署（2026-10-29）

- `Dockerfile`（多階段，runtime 為 debian:bookworm-slim + rsnapshot/rsync/ssh/tini）、`deploy/docker-compose.yml`（PostgreSQL 17 + 應用）。
- 新增 `GET /healthz`（檢查資料庫連線）與 `generate-keypair --if-missing`（entrypoint 首次啟動產生金鑰）。
- 修正：SSH 私鑰未以 `-i` 傳給 ssh（備份與還原都會用錯金鑰）。
- 容器內以**真正的 rsnapshot** 驗證：本機備份、snapshot 輪替、經 SSH 備份遠端 client、重啟後金鑰保留、TZ 生效、SIGTERM 正常停止。
