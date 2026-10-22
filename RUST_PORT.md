# Rust 搬遷計畫（rust-port 分支）

追蹤 issue：https://github.com/markliou/codex-provider/issues/33

這份文件是 `rust-port` 分支的工作說明。在這個分支工作的 agent 開始前必須讀完本文件、`AGENTS.md` 與 `SPEC.md` 的相關章節。`AGENTS.md` 的所有規則在這個分支一樣有效。

## 1. 目標

把 pool 從「Go + Rust」逐步搬成「只有 Rust」，對外行為不變：

- 對上游送出的內容與原生 Codex（`upstream/Cargo.toml` 釘住的版本）完全一致。
- 路由、帳號、額度、儀表板的行為與現在的 `main` 相同，所有既有測試涵蓋的行為都要保留。
- 正式環境切換時，狀態檔與帳號資料不需要重建。

全部改成 Rust 的主要收益：直接呼叫 Codex 自己的程式碼（SSE 解析、錯誤分類、重試判斷、backend client），讓「與 Codex 一致」由同一份程式碼保證，而不是靠照 Codex 原始碼整理出來的樣本（`cmd/codex-pool/testdata/codex-upstream-failures.json`）。

## 2. 角色分工與環境

| | 營運（main） | 搬遷（rust-port） |
|---|---|---|
| 資料夾 | `/home/markliou/asustor5202tNFS/codex-provider` | `/home/markliou/asustor5202tNFS/codex-provider-rust` |
| 分支 | `main` | `rust-port` |
| 負責 | 正式 provider 的修正、部署、帳號狀態 | 本計畫的開發與測試 |

搬遷 agent **絕對不可以**：

- 進入或修改 `main` 的資料夾，或在 `rust-port` 以外的分支 commit。
- 執行 `scripts/deploy.sh`（它只接受乾淨的 `main`，在這裡會拒絕；不要想辦法繞過）。
- 對正式容器 `codex-pool`、port `8317`、volume `codex-pool-data`、映像 `codex-pool:local` 做任何操作（停止、重啟、`docker exec`、讀寫、重新標籤）。
- 讀取、複製或使用任何正式帳號資料：`.env`、`/data/accounts/*/auth.json`、`config.json`、`runtime.json`。OpenAI 的 refresh token 用一次就換新，測試環境用正式 `auth.json` 的複本刷新一次，正式那份就會失效（`token_revoked`），只能請帳號主人重新登入。
- 在主機上安裝任何軟體；所有建置、測試、格式化都在 Docker 內進行（`AGENTS.md`）。
- force-push，或改寫 `rust-port` 已推送的歷史。

合併與部署流程：

1. 搬遷 agent 完成一個步驟，所有檢查通過後在 `rust-port` commit 並 push。
2. 由使用者通知營運端，把 `rust-port` 合併進 `main`、重新跑檢查、部署。部署只在營運端進行。
3. 搬遷 agent 定期把 `main` 合併進 `rust-port`（`git merge main`，不要 rebase），以取得營運端的修正。衝突以 `main` 的行為為準。

## 3. 現況

- **Go**（`cmd/codex-pool/`）：約 1.2 萬行 `main.go`，加上 `responses_ws.go`、`native_codex.go`、`codex_own.go`、`codex_prime.go`、`websocket.go`、`logfile.go`、`admin_assets.go` 與 `web/`；測試約 1.1 萬行。負責 API 金鑰、路由（放置、嚴格 lineage 綁定、搬家、冷卻、重試、Use Pro last、紅框）、額度更新、狀態檔、管理介面與儀表板。
- **Rust**（`upstream/src/main.rs`，約 570 行）：`codex-upstream`，本機回環的傳輸層。Go 把要送的請求交給它的 `/forward`（以及 WebSocket 中繼），它用 Codex 的 `codex-http-client`、`codex-websocket-client` 送出。檔頭的註解寫著「Keep it a transport」；第 3 步之後這個意圖會改變，必須同時更新該註解與 `SPEC.md`（`AGENTS.md`：不可默默移除意圖註解）。
- **pool 自己的上游呼叫**（refresh、usage、帳號資料、reset credits）：目前由 Go 依照從原生 Codex 量到的檔頭順序組好（`sendCodexOwnRequest`），再經由 Rust 的 `/forward` 送出。priming 用內建的 Codex CLI。
- **啟動**：`docker-entrypoint.sh` → `supervisor.sh` 先啟動 `codex-upstream`，再啟動 `codex-pool`。
- **狀態**：`/data/state/runtime.json`（綁定、冷卻、額度、健康度、事件）、`/data/config.json`（帳號與設定）、`/data/accounts/<id>/auth.json`（憑證）、`/data/logs/service.log`。

## 4. 必須守住的條件

1. **狀態檔單一寫入者**：`runtime.json`、`config.json`、`auth.json` 任何時候都只能由一個程式寫入。拆分以「誰擁有哪份資料」來切。第 4 步之前一律由 Go 寫入；Rust 需要的資料透過本機介面向 Go 取得。
2. **雜湊與格式完全相容**：
   - 必須逐位元相同：放置用的 rendezvous 雜湊（`stickyBalanceScore`、`placementScoreLocked`）、`operationalIdentifierHash`、lineage／thread／response 雜湊、`shortHash`。先用 Go 產生測試向量，Rust 對同一組輸入要得到相同輸出。
   - 狀態檔與設定檔的 JSON 必須能互相讀寫：欄位名稱、時間格式（Go 的 RFC 3339 奈秒精度）、`omitempty` 的省略規則、未知欄位要保留。測試資料用 Go 測試產生的假資料，不使用正式檔案。
3. **測試先行**：搬一段程式碼前，先把對應的 Go 測試翻成 Rust 測試並通過，才能刪除或停用 Go 那段。這些測試記錄了嚴格綁定、搬家、重試、Use Pro last、紅框等行為，是規格的一部分；不可以為了讓測試通過而改測試的預期。
4. **Codex 一致性**（`AGENTS.md` 的 Codex Wire Conformance）：每個步驟都要通過 Go 測試、Rust 測試、`sh scripts/conformance/codex-conformance.sh check` 與 `sh scripts/conformance/wire-capture.sh`（約 15–20 分鐘，在背景執行）。原生端點 `/k/<key>/backend-api/codex/` 必須原樣轉發，不得改寫檔頭或內容。改到與上游溝通的程式碼時，要在附近註解引用對應的 Codex 原始碼位置。
5. **測試只用假上游與假帳號**：`scripts/conformance/harness/fake-upstream.mjs`、`mkauth.mjs`、`test/mock-upstream/`。
6. **測試環境與正式環境隔離**：測試用的映像標籤、容器名稱、port、volume 都不能與正式環境相同（見第 0 步）。
7. **行為改變要寫進 SPEC**：任何對外行為的改變都要同步更新 `SPEC.md`；程式碼註解與 `SPEC.md` 維持英文，與現有內容一致。

## 5. Rust 實作注意事項

- **鎖**：狀態用一把鎖保護，只在同步區段內持有，絕不跨 `.await` 持有。需要網路 I/O 時，先在鎖內取出需要的資料、放開鎖、做 I/O，再取鎖寫回，並處理期間狀態已被其他請求改變的情況（Go 的 `selectAccountForRoute` 每次重試都重新讀 lineage 帳號，就是這個用意）。
- **panic**：`std::sync::Mutex` 在持有期間 panic 會中毒。改用不會中毒的鎖（例如 `parking_lot`），或明確處理中毒；單一請求出錯不可以讓整個 pool 停擺。
- **Codex crate**：它們是 Codex 的內部介面，每次升版都可能改動。只依賴真正需要的 crate，並在 `codex-conformance.sh diff` 的升版流程中，把編譯錯誤視為必讀的變更清單。
- **編譯速度**：用 Docker volume 快取 cargo registry 與 `target`，避免每次重新編譯 Codex 的依賴。
- **相依版本**：與 Codex workspace 使用相同的版本與 features（`upstream/Cargo.toml` 已有範例），避免 TLS／HTTP 行為與 Codex 不同。

## 6. 搬遷步驟

每個步驟完成後都要能單獨合併進 `main` 並部署。

### 第 0 步：測試隔離（必須最先完成）— 已完成

整合測試改用 `codex-pool:test`，並由 `scripts/test/isolation.sh` 拒絕正式環境的名稱；Rust 檢查為 `scripts/test/rust-check.sh`（第 7 節）。`codex-conformance.sh` 修正為可在 worktree 中執行。以下是原本的要求：

- `scripts/test/integration-*.sh` 目前執行 `docker build -t codex-pool:local`，會覆蓋正式映像的標籤；下一次 `docker compose up` 就會把測試版部署上線。**在完成這一步之前，不可以執行任何 `integration-*.sh`。**
- 改成使用 `codex-pool:test`（或可由環境變數指定、預設不是 `codex-pool:local` 的標籤）。`wire-capture.sh` 已經使用 `codex-pool:conformance`，確認沒有其他腳本用到 `codex-pool:local`（`scripts/build-local-image.sh` 與 `scripts/deploy.sh` 除外）。
- 建立 Rust 的測試指令（Docker 內的 `cargo test`、`cargo fmt --check`、`cargo clippy`）與快取 volume，並記錄在本文件第 7 節。
- 這一步也適用於 `main`；完成後告訴使用者，讓營運端一併合併。

### 第 1 步：pool 自己的上游呼叫（順序改到第 2 步之後）

查證結果（2026-10-21）：
- usage、帳號資料（accounts/check）、reset credits：可以直接用 Codex 的 `codex-backend-client`。它有 `with_user_agent()`，所以每個帳號仍可帶「最後使用該帳號的 Codex」的 User-Agent（SPEC 2.3.2）。Codex 的型別保留了方案（含 ProLite、ProMax）和組織名稱（`accounts/check` 的 `name`）。差別：Codex 不認識的新方案名稱會變成 `Unknown`。開工前要先用正式資料（只看欄位名稱）確認沒有 pool 在用、但 Codex 型別會丟掉的欄位。
- token refresh：不能用 Codex 的 `AuthManager`。它的 HTTP client 的 originator 與 User-Agent 是整個 process 共用的全域值，作業系統部分取自容器本身，無法每個帳號不同。refresh 改為在 Rust 照 Codex 原始碼（`login/src/auth/manager.rs` `request_chatgpt_token_refresh`）組請求，行為與現在相同。
- Go 透過本機介面取得結果；狀態與 `auth.json` 仍由 Go 寫入（單一寫入者）。`wire-capture.sh` 的 own-call 比對必須仍然相同。
- 被拒的憑證不再使用（main c0d0d93，`cmd/codex-pool/refused_credential.go`）：refresh 被 Codex 判定為永久失敗（`classify_refresh_token_failure`），或 usage 回 401/403 `token_revoked`／`token_invalidated` 時，記下該帳號目前 token 的雜湊。在 `auth.json` 換成新的 token（重新登入）之前，refresh 不連網直接失敗，背景額度輪詢和 Refresh all 都跳過這個帳號。refresh 和 usage 搬到 Rust 時，這個行為要一起搬，Go 的 `refused_credential_test.go` 要翻成 Rust 並通過。
- 額度輪詢沒有固定節拍（main `cmd/codex-pool/quota_schedule.go`）：每個帳號各自排程。30 分鐘內有請求的帳號，下一次在上一次結束後隨機 4～6 分鐘；閒置的帳號隨機 20～30 分鐘，一有請求就回到 4～6 分鐘。啟動後的第一輪分散在 2 分鐘內，一次只問一個帳號，之間隨機停 3～12 秒。預熱（prime）預設關閉，由管理頁面的 Prime idle accounts 開關決定（`quotaPrime`），搬到 Rust 時預設值不可改回開啟。開啟時，看到時間窗重置後先隨機等 5～30 分鐘（`codex_prime.go` quota prime grace），等待結束時排程提前查一次（`poll_by`），時間窗仍未開始才預熱；期間有請求就不預熱。Rust 的排程已寫好（`upstream/src/pool/quota_schedule.rs`，測試對應 Go 的 `quota_schedule_test.go`）；輪詢搬到 Rust 時直接用它，不要改回共用計時器或一次掃完所有帳號。

### 第 2 步：串流解析與錯誤分類

查證結果：Codex 把串流解析與錯誤分類放在 crate 私有的模組（codex-api `sse/`、`endpoint/responses_websocket.rs` 的私有函式），外部呼叫不到。做法是把這些檔案**原樣複製**進來編譯（`upstream/vendor/codex`，`scripts/conformance/codex-vendor.sh` 保證逐位元相同），連 Codex 自己的測試一起跑。Codex 的分類是單一帳號的判斷（同帳號重不重試），而且分類後丟掉錯誤代碼；pool 的換帳號與冷卻是多帳號的判斷，仍由 pool 自己讀代碼決定。所以分兩層：Codex 層決定「原生 Codex 會怎麼做」，pool 層決定「換不換帳號、冷卻多久」。

分三段，每段都能單獨合併與部署：
- **2a 複製與驗證**：複製檔案、編譯、跑 Codex 的測試、`codex-conformance.sh check` 納入逐位元比對。尚未接上 pool，行為不變。
- **2b 影子模式**：Go 遇到上游失敗（串流中的終止事件、WebSocket 錯誤訊息、HTTP 非 2xx）時，把那一筆交給 Rust 的 `/classify`，Rust 用 Codex 的程式碼回傳 Codex 的判斷（`CodexErr::retry_delay` 是否可重試、`to_codex_protocol_error` 的類別）。Go 照舊決定，只在兩邊判斷不一致時記 log。部署後用真實流量觀察。
- **2c 切換**：同帳號是否重試改以 Codex 層為準；換帳號與冷卻仍由 pool 層決定。`codex-upstream-failures.json` 中已由 Codex 程式碼保證的部分可以縮減，但每個移除的樣本都要有對應的 Rust 測試。
- 儀表板的 Unrecognized failures 與 log 的 `class=unknown` 行為不變。

進度：2a、2b 已完成。2b 的 log 欄位：串流終止失敗那一行加上 `codex=<kind>/<api> retry=yes|no`（Codex 本身不處理該訊息時加 `codex_skips`）；HTTP 401/403/429/5xx 新增 `upstream http failure: ... pool=<pool 的理由> ... codex=...`。

已找到的差異（2c 要逐一決定，部署 2b 後再用正式流量確認頻率）：
1. fixture 的「stream overload」其實是 Codex 刻意做壞的樣本，Codex 讀成一般串流錯誤。已修正 fixture，並補上正常的 overload 樣本（`upstream/src/classify.rs` 的測試會讓 fixture 的 `codex` 欄永遠符合 Codex 的程式碼）。
2. `server_is_overloaded`：Codex 只有在上游給 `retry-after` 時才重試（`retry_delay` 只看 `server_retry_delay`）；pool 目前不論有沒有 `retry-after`，都會在同帳號重試數次，程式碼註解卻寫「as a native Codex does」。
3. SSE 中獨立的 `error` 事件：Codex 除了 `flex_unavailable` 都不立即處理，等串流結束以「stream closed before response.completed」（可重試）收尾；pool 依事件中的代碼分類。
4. 串流中的 `organization_usage_limit_exceeded`：Codex 讀成一般可重試錯誤（只有 HTTP 429 的 body 才讀成 QuotaExceeded）；pool 一律當額度用完。
5. WebSocket 包裝的 429 `rate_limit_exceeded`：Codex 讀成 `retry_limit`，依上游的 retry-after 重試。

### 第 3 步：Rust 成為對外入口

- Rust 接收所有對外請求（port 8317 的角色），還沒搬的功能反向轉給 Go（改為只在容器內監聽）。
- 對 Codex 而言檔頭、內容、WebSocket 行為都不能改變；`wire-capture.sh` 必須維持相同。
- 安裝 ID（main `cmd/codex-pool/installation_id.go`，SPEC 2.3.2）：每個帳號在自己的 `.codex/installation_id` 有一組固定的 UUID，送上游時只替換 `client_metadata` 裡和 `x-codex-turn-metadata` 檔頭裡的安裝 ID，其他位元組不動；WebSocket 的握手和每則訊息用該 socket 綁定帳號的 ID，prime 的暫時 home 也放同一組。Rust 的函式已寫好（`upstream/src/pool/installation_id.rs`，測試對應 Go 的 `installation_id_test.go`）；轉送搬到 Rust 時直接用它，`wire-capture.sh` 的 installation id 檢查必須仍然通過。
- 更新 `upstream/src/main.rs` 開頭「Keep it a transport」的意圖註解、`supervisor.sh` 與 `SPEC.md`。

### 第 4 步：路由與狀態檔

- Rust 一次整塊接手：API 金鑰、放置、嚴格 lineage 綁定、搬家、冷卻、重試、Use Pro last、紅框、額度更新與 `runtime.json`／`config.json`／`auth.json` 的寫入。這一步完成後，Go 不再寫入任何狀態。
- 這是最大的一步。開始前，`main_test.go`、`responses_ws_test.go`、`native_codex_test.go` 等對應的路由測試必須全部翻成 Rust 並通過。
- 切換時要用 Go 版產生的狀態檔做相容測試：Rust 讀入後寫出，Go 仍能讀回，反之亦然。

前置工作（已完成，`upstream/src/pool`）：
- 雜湊與身分字串：`pool/hash.rs` 對照 Go 產生的 `cmd/codex-pool/testdata/hash-vectors.json`。測試資料由正式的 Go 函式算出，包含 1,024 組放置分數（逐位元比對）、Unicode 空白／控制字元／大小寫的邊界情況，以及帳號欄位組成身分字串的各種組合。放置分數的 `ln()` 照 Go amd64 的 `math.Log` 運算順序移植。放置權重（`placementWeightLocked`）還沒有移植，目前直接用 Go 算好的權重。
- 狀態檔：`pool/state.rs` 對照 `cmd/codex-pool/testdata/state-vectors.json`。Go 用反射把 `config`、`state` 的每個欄位都填上值（full），另外一份全部是零值（zero）。Rust 讀入後寫出必須和原文件相同；不認得的欄位會保留，但測試裡只要出現 `future*` 以外的未知欄位就失敗，表示 Rust 少了那個欄位。
- Go 型別或雜湊改了，Go 測試會要求重新產生（`CODEX_POOL_GENERATE_VECTORS=1`），重新產生後 Rust 測試再要求 Rust 跟上。
- 還沒做：讀 `auth.json`；Go 讀回 Rust 寫出的檔案，目前靠「Rust 寫出的內容和 Go 的原文件相同」間接保證。

### 第 5 步：管理介面

- 管理介面 API、公開儀表板 API、登入與 session 移到 Rust；`web/` 的前端資源不變。
- 帳號語意（身分、額度、配額）在路由與儀表板之間共用同一個定義，並用測試綁在一起（`AGENTS.md`）。
- 舊的登入 cookie 會失效，使用者重新登入一次即可；在 `SPEC.md` 註明。

### 第 6 步：移除 Go

- 刪除 Go 程式碼、`go.mod`、Dockerfile 的 Go 建置階段；mock upstream 與整合測試腳本改用 Rust 或 Node 實作。
- 更新 `SPEC.md`、`README.md`、`AGENTS.md`（包括 Go 測試指令），並刪除本文件或改為歷史紀錄。

## 7. 指令

Go 測試（約 4 分鐘）：

```sh
docker run --rm -v "$PWD:/src" -w /src golang:1.24-bookworm sh -c 'test -z "$(/usr/local/go/bin/gofmt -l ./cmd ./test)" && /usr/local/go/bin/go vet ./... && /usr/local/go/bin/go test -p 1 -buildvcs=false ./...'
```

一致性檢查：

```sh
sh scripts/conformance/codex-conformance.sh check
sh scripts/conformance/wire-capture.sh   # 15–20 分鐘，在背景執行
```

網頁資源：`sh scripts/test/render-web-assets.sh`

Rust 測試（rustfmt、clippy、cargo test；第一次要編譯 Codex 的依賴，之後用 Docker volume `codex-pool-rust-registry`、`codex-pool-rust-target` 的快取）：

```sh
sh scripts/test/rust-check.sh
```

原始碼以唯讀方式掛載，檢查不會改檔案。要格式化時：

```sh
docker run --rm -u "$(id -u):$(id -g)" -v "$PWD/upstream:/src" -w /src codex-pool-rust-check:test cargo fmt
```

新增 Rust crate 時，把它加進 `rust-check.sh`。

整合測試（建置 `codex-pool:test`，不會動到 `codex-pool:local`；`scripts/test/isolation.sh` 會拒絕正式環境的映像、容器與 volume 名稱）：

```sh
sh scripts/test/integration-codex-config.sh
sh scripts/test/integration-device-auth-failover.sh
sh scripts/test/integration-codex-subagent.sh
sh scripts/test/integration-routing-cache-observability.sh
```

## 8. Commit 規則

每次 commit 前（`AGENTS.md` 的 Codex Account Security）：

1. `sh scripts/security/precommit-security-audit.sh --check`
2. 檢查 `git diff --cached`，確認沒有帳號識別資訊、email、token、auth 狀態、上游金鑰或 `/data` 產生的內容。
3. 確認沒有 stage `.codex/`、`auth.json`、`.env`、帳號資料、`config.json`、OAuth 回應、API 金鑰、私鑰或 session cookie。不要 stage `.claude/`。
4. 審查完成後才用 `CODEX_ACCOUNT_SECURITY_REVIEWED=yes git commit ...` 提交。不可以覆寫或繞過 hook。

## 9. 進度

完成一個步驟，就在 issue #33 勾選對應項目並留言摘要：做了什麼、測試結果、還沒完成的部分。
