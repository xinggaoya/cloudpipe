# cloudpipe 重构 / 完善 / 增强 统一实施方案

> 基于对 `crates/cloudpipe-sdk`、`crates/cloudpipe`（CLI）、`crates/cloudpipe-node`（napi）、
> 三条 GitHub Actions 工作流与全部文档的通读。所有条目均已在本地复现验证。
>
> **状态：已全部实施。** 每条的实现方式与计划的差异、以及实施过程中发现并
> 修复的额外缺陷，都记录在文末的「实施结果」一节。

---

## 0. 项目现状速览

| 维度 | 现状 |
|---|---|
| 结构 | 3 crate workspace：`cloudpipe-sdk`（纯 async 核心）/ `cfp`（CLI）/ `cloudpipe-node`（napi） |
| 代码量 | Rust 约 3.9k 行 + 集成测试 614 行 |
| 测试 | SDK 16 单测 + 7 集成测试 + CLI 8 单测；Node 单测 **无法编译** |
| CI | `ci.yml` / `npm.yml` / `release.yml`，**ci.yml 当前为红** |
| 架构评价 | 分层清晰、文档注释详尽、`#![deny(missing_docs)]`、SDK 无 UI 依赖（CI 有断言）——底子好 |

结论：**架构方向是对的，问题集中在「实现细节的健壮性」与「三处发布链路的一致性」**。
因此本方案以「不推倒重来、原地加固 + 补齐能力」为主。

---

## P0 —— 已确认的 Bug / 红 CI（必须先修）

### P0-1 `session.rs:234` 域名字符串切分方向反了（真 Bug，潜伏地雷）

```rust
domain: full_name_for_task.rsplit_once('.').map(|(_, d)| d.to_string())
```
`rsplit_once` 在**最后一个**点处切分，`"myapp.example.com"` → `("myapp.example", "com")`，
`domain` 变成 `"com"`。已本地复现：
```
rsplit_once domain = "com"
split_once   domain = "example.com"
```
目前 `SessionConfig.domain` 未被 `bootstrap_session` 消费，所以**尚未爆雷**，
但任何后续接线都会立刻踩坑。

**修法**：重挂载时直接复用启动时的 `cfg.domain`（本来就是同一个值），
不要从 `full_name` 反推，从根上消除该类错误。

---

### P0-2 `handle.rs:182/183/205/206` 跨 await 持锁 → CI 红

`cargo clippy -p cloudpipe-sdk --all-targets -- -D warnings` 复现：
```
error: this `MutexGuard` is held across an await point   (×2)
```
`ci.yml` 的 clippy 步骤是 `-D warnings`，因此 **ci.yml 在当前 commit 必然失败**。

根因：`stop()` 里 `let mut guard = self.state.lock().await;` 之后连续 await
`find_dns_record` / `delete_dns_record` / `cleanup_connections` / `delete_tunnel`。
同时这段清理逻辑与 `session::cleanup` **整段复制粘贴**（handle.rs:225-240 ↔ session.rs:520-545）。

**修法**（两者一起）：
1. 把 Cloudflare 侧清理抽成 `SessionState::purge(&self)`（`&self`，内部不可变），
   `session::cleanup` 与 `TunnelHandle::stop` 共用，消除重复代码。
2. 只在需要改 `cleaned` 标志时短暂持锁（`lock().await` → 判定/置位 → `drop(guard)`），
   网络 IO 全部在无锁状态下完成。

---

### P0-3 `cloudpipe-node` 的 lib 单测无法编译 → `cargo test --workspace` 必挂

```
error: undefined symbol: napi_call_threadsafe_function
error: undefined symbol: napi_reference_unref
error: could not compile `cloudpipe-node` (lib test)
```
`ci.yml` 跑 `cargo build/test/clippy --workspace --all-targets`，三条全挂。
原因：napi 符号只在 Node 进程内解析。

现有单测（`event.rs` 6 个、`error.rs` 5 个、`lib.rs` 6 个）**逻辑上完全不依赖 napi**
（`event_to_payload` / `from_sdk_error` / `parse_error_code` 都是纯函数），
纯粹被 crate 类型 `cdylib` 牵连。

**修法**：新建 `crates/cloudpipe-node/tests/` 下的**集成测试**（`tests/*.rs` 链接
`libcloudpipe_node.rlib` 而非 `.so`，同样会缺符号）——所以正解是：
把这些纯函数下沉到 `cloudpipe-sdk` 或新建无 napi 依赖的 `cloudpipe-node-core` 薄层，
`napi` crate 只保留 `#[napi]` 标注的薄壳。这样单测可跑、且逻辑与绑定解耦。

---

### P0-4 `npm.yml:29` 包名已改名，preflight 仍在查旧名 → 发布链路必挂

commit `139a6f8` 已把包改名为 `@xinggao/cloudpipe`，但 preflight 仍是：
```yaml
PACKAGE_NAME: cloudpipe
if npm view cloudpipe name; then exit 1; fi
```
`cloudpipe` 在公网已被他人占用（名字不是我们的），**preflight 会 100% 失败**，
`publish` job 依赖 `build → needs: [lint, preflight]`，整条 npm 发布链断死。

**修法**：改为 `npm view "@xinggao/cloudpipe"`，并把「是否已存在」的语义改成
「是否**由我们**拥有」（对比 repository/homepage 字段），而不是「存在即失败」。
另外 `published cloudpipe@${{ github.ref_name }}` 的回显名也应改成 `@xinggao/cloudpipe`。

---

### P0-5 文档承诺 `cfp -v` 但实际不存在

README 命令表写着 ``cfp -v / --version``，实测：
```
$ cfp -v
error: unexpected argument '-v' found
```
clap 的 `version` 只给 `--version` / `-V`。

**修法**：README 改 `cfp -V / --version`；若想保留 `-v` 则显式 `#[arg(short='v', long="version")]`
（但会与未来 `-v verbose` 冲突，建议统一用 `-V` + 新增 `--verbose` 给日志用）。

---

### P0-6 `bootstrap_session` 失败路径泄漏 Cloudflare 资源（真 Bug）

`session.rs` 的顺序是：create_tunnel → set_ingress → **create_dns** → spawn cloudflared。
前两步失败都有 rollback，但 `binary::spawn(...).await?` 失败时**直接 `?` 返回**，
此时 **tunnel 与 DNS 记录双双成为孤儿**，只能靠 `cfp cleanup` 事后擦。

**修法**：给 bootstrap 引入显式的 `BootstrapGuard`（RAII / `scopeguard` 风格），
任何中途失败自动按逆序回滚已创建的 Cloudflare 资源；成功则 `defuse()`。

---

### P0-7 `StartedBuilder::start()` 失败会泄漏事件派发任务

`builder.rs` 先 `tokio::spawn` 出 event dispatch 任务，再 `session::start(cfg).await?`。
若 `start` 返 Err，`StartedBuilder` 被消费 → `DispatchSlot` 析构 → `JoinHandle`
被 **detach 而非 abort** → 任务持有 `event_tx` 永不退出 → 任务泄漏。

**修法**：给 `StartedBuilder` 加 `Drop`（abort dispatch），或把 spawn 移到
`session::start` 成功之后。

---

### P0-8 `subdomain` 校验发生在下载 30MB 二进制**之后**

`validate_subdomain` 在 `session::start` 里调；而 `builder.rs` 的顺序是
`resolve_zone`（网络往返）→ `ensure_installed`（可能下载 ~30MB cloudflared）→ `session::start`（才校验）。
所以 `cfp 8080 -s 'BAD NAME'` 会先把 cloudflared 拉下来，然后才报错。

**修法**：把 `validate_subdomain` 提到 `StartedBuilder::start()` 最前面，
`session::start` 保留为幂等的二次防御。

---

## P1 —— 可靠性 / 资源安全

### P1-1 `cloudflared` token 走命令行参数 → 本地信息泄露

`binary.rs:299`
```rust
.args(["tunnel", "run", "--token", tunnel_token])
```
argv 在 Linux 上对同机所有用户 `ps` 可见（`/proc/<pid>/cmdline`），等于把
Cloudflare Tunnel 凭据广播给本机其他用户。

**修法**：改用 `--token-file <path>`（0600 临时文件）或 stdin 传入；
spawn 后立刻 `unlink`。这是本项目安全模型里最该修的一条。

> **后续修正（已实施）**：`--token-file` 方案本身有个竞态——`spawn` 返回只代表
> fork/exec 完成，子进程还没 `open()` 那个文件，父进程抢先 `unlink` 会让
> cloudflared 每次启动都以 `Failed to read token file: …` 退出；而这句报错不含
> `err` 字样，被 stderr 分类器当成噪声丢弃，于是表现为「无限重启且看不到原因」。
> 现改为通过环境变量 `TUNNEL_TOKEN` 交接（cloudflared 的 `--token` 本就回落到
> 该变量）：既不落盘、不进 argv，也没有删除竞态。同时补上：致命日志分类、
> 退出时上报 exit code + stderr 尾部、崩溃循环预算。

---

### P1-2 `cloudflared` 下载无校验、无超时、无原子写（供应链 + 并发）

- 直接下载 `releases/latest/download` 的**未固定版本**并 `chmod 755` 执行，无 SHA 校验 → 上游一旦被投毒即自动执行。
- `reqwest::get(url)` **无 timeout**，网络挂死则永久阻塞。
- 无重试、无进度、无大小上限。
- `extract_tgz` 里 `let _ = dir;` 是残留的空语句。
- `ensure_installed` 是 check-then-act：两个进程（CLI + Node 应用）并发首次启动会
  同时下载并写同一路径，后写者可能读到半截二进制 → `Exec format error`。
- `fs::create_dir_all` / `File::create` 发生在**下载完成之后**，语义反了。

**修法**：
1. 固定 `cloudflared` 版本 + 维护各平台 sha256，构建期或内嵌校验表；
2. 用带 timeout/重试的 `reqwest::Client`（`stream` 分块落盘）；
3. 落盘改为 `*.part` 临时文件 → `fsync` → `set_permissions(0o755)` → `rename` 原子替换；
4. 加目录级 `fs2` 文件锁，避免并发重复下载。

---

### P1-3 `CloudflareApi` 无任何超时

`CloudflareApi::with_base` 里 `Client::builder().user_agent(..).build()`，
没有 `connect_timeout` / `timeout` / `read_timeout`。Cloudflare 侧一次挂起
就足以让 `cfp` 永久卡在 "Creating tunnel..."。

**修法**：`connect_timeout(10s).timeout(30s)`，并把超时错误单独分类
（`ApiError::Timeout`），便于上层给出「网络问题，重试」提示。

---

### P1-4 关机顺序错误：先删 DNS/隧道，后杀进程

`session.rs` 监督循环中，`run_until_exit` 因 `UserRequested` 返回后**立即**
`cleanup(&mut guard)`（删 DNS → 清连接 → 删 tunnel），而 `cloudflared` 子进程
此时**仍在运行**（靠 `kill_on_drop` 在循环结束后才被杀掉）。后果：
删除瞬间的在途请求全部失败，「Clean exit」名不副实。

**修法**：关机路径改为 `terminate(child) → wait(child) → cleanup()`，
即先发 SIGTERM、宽限 2s、再 SIGKILL，确认退出后才动 Cloudflare 资源。

---

### P1-5 `run_until_exit` 250ms 轮询 + 每次 `log_debug` 都开文件

- `run_until_exit` 每 250ms `try_wait` 轮询，而 `log_debug` 每次都
  `std::env::var` + `OpenOptions::open` 一次文件 → **每秒 4 次 open/close**。
- 且 `log_debug(&format!(...))` 的 `format!` 在**日志关闭时也会执行分配**。
- `run_until_exit` 明明已有 `Shutdown::notified()`，却没用它做主要等待源。

**修法**：
1. `static DEBUG_LOG: OnceLock<Option<PathBuf>>` 缓存路径；
2. 用宏 `debug_log!(...)` 包住 `format!`，未启用时零成本；
3. `run_until_exit` 改为 `child.wait()` 与 `shutdown.notified()` 的 `tokio::select!`，
   彻底去掉轮询。

---

### P1-6 监督循环状态机混乱，失败重试会误报 `ShuttingDown`

`session.rs:150-290`：bootstrap 失败时并不 `continue`，而是「回到循环顶部让
`run_until_exit` 立刻看到已死的 child 再退出一次」。后果：
每失败一次就多发一轮 `ShuttingDown`（CLI 会打印 "cloudflared exited — cleaning up
before respawn."），事件语义被污染；`consecutive_failures` 与 `attempt` 语义纠缠。

**修法**：抽成显式的 `Supervisor` 状态机，把「child 退出」和「bootstrap 重试」
拆成两条独立分支：
- 重试采用**指数退避 + 抖动**（500ms → 1s → 2s → 4s → 8s，±20% jitter），替换当前
  注释里自认「long backoff just punish the user」的 500ms 固定值；
- `Restarting` 只在真正重启时发，bootstrap 失败单独发 `Event::BootstrapFailed { attempt, error }`。

---

### P1-7 `resolve_conflicts` 吞掉 API 错误

`session.rs:490-520` 两处 `if let Ok(...)` / `if let Ok(Some(...))` 把
「找不到」与「鉴权失败 / 网络错误」混为一谈。网络抖动时静默继续建隧道，
然后以 `SubdomainInUse` 失败，错误信息完全误导。

**修法**：只吞 `NotFound` 类语义（CF 用 `result: null` 表达），其余错误向上抛。

---

### P1-8 列表 API 无分页 → 静默截断

- `list_zones`：`per_page=50`，无分页 → **拥有 >50 个 zone 的用户会被静默截断**，
  `resolve_zone` 可能挑错 zone。
- `list_dns_records`：`per_page=100`，`cfp cleanup` 会漏掉第 101 条以后的孤儿 CNAME。
- `find_tunnel_by_name` / `list_tunnels` 同样。

**修法**：解析 envelope 的 `result_info.{page,per_page,total_pages}` 实现
`paginate()` 迭代器，SDK 内部对全量遍历场景自动翻页。

---

### P1-9 `random_subdomain()` 碰撞空间只有 1 万

```rust
format!("user-{:04}", seed % 10000)
```
10,000 种取值，在多人共用 zone 时 `SubdomainInUse` 会频繁出现。

**修法**：改用 `getrandom` 取 4 字节 → base32 编码为 7 位 label
（`user-a7k2m9x`，RFC 1123 合法），碰撞空间 32^7 ≈ 3.4e10；
并在 `resolve_conflicts` 检出冲突时**自动换名重试 3 次**（而不是直接失败）。

---

### P1-10 关机时资源释放不足

- **Rust SDK**：`TunnelHandle::Drop` 里 `task.abort()`，导致监督任务来不及
  Cloudflare 侧清理 → `drop(handle)` 必然留下孤儿。注释自称 "best-effort"，
  实则是必然泄漏。
- **Node**：`Listener` 没有 `Drop`/finalizer，JS 侧丢掉 listener 会让
  `cloudpipe-session` 线程永久持有 `TunnelHandle` → 隧道泄漏 + 线程不退出。

**修法**：
- SDK：`Drop` 里若能拿到 runtime，则 `spawn` 一个 detached 清理任务做 best-effort purge。
- Node：为 `Listener` 实现 napi finalizer，触发时经 `cmd_tx` 发 `Stop`。

---

### P1-11 配置文件非原子写 + 权限窗口 + 静默吞损坏

`config.rs::save`：`fs::write` 之后再 `chmod 0600`。存在**世界可读的窗口期**
存着明文 token；且 `fs::write` 中途崩溃会截断文件，而 `load()` 用
`unwrap_or_default()` **静默降级为空配置** → 用户 token 无声丢失。

**修法**：
1. `OpenOptions::new().mode(0o600)` 建文件（创建即 0600，无窗口），或先 `set_permissions` 再写；
2. 写 `config.json.tmp` → `fsync` → `rename` 原子替换；
3. `load()` 解析失败时 `warn!` 到 stderr 并明确告知用户，不要静默；
4. 加 `version: u32` 字段（`#[serde(default)]`）+ 迁移函数，为后续 schema 演进留口。

---

## P2 —— 架构 / 模块化重构

### P2-1 `session.rs` 619 行是当前最大复杂点，需拆分

现职责混杂：状态定义、随机名、事件转发槽、bootstrap、冲突消解、清理、
轮询循环、调试日志。拆分方案（保持私有性不变）：

```
session/
├── mod.rs        // start() 编排入口 + 公开常量
├── config.rs     // SessionConfig
├── state.rs      // SessionState + purge()/cleanup()
├── bootstrap.rs  // bootstrap_session() + BootstrapGuard(rollback)
├── conflict.rs   // resolve_conflicts()
├── supervisor.rs // Supervisor 状态机（见 P1-6）
└── log.rs        // debug_log 宏 + OnceLock 缓存
```

### P2-2 清理逻辑去重（P0-2 的一半）

`session::cleanup` 与 `TunnelHandle::stop` 内联的那 20 行是**逐字复制**。
统一为 `SessionState::purge(&self)`，两处调用。顺带消除 P0-2 的跨 await 锁。

### P2-3 `main.rs` 三个 handler 的样板代码去重

`handle_key` / `handle_domain` / `handle_domains` / `handle_cleanup` 各自重复
「加载 store → 取 token → 报同样的错」。抽 `ResolvedContext` 辅助结构：

```rust
struct Ctx { store: ConfigStore, config: Config, api: CloudflareApi }
impl Ctx { async fn load() -> Result<Self>; }   // 统一的三段缺失校验
```

### P2-4 `Ctrl+C` 处理冗余且低效

`main.rs` 用 `ctrlc` crate + `AtomicBool` + **100ms 轮询循环**模拟信号。
tokio 已经 enable 了 `signal` feature（workspace tokio deps 里就有），
`tokio::signal::ctrl_c()` 是原生 await 方案。

**修法**：删掉 `ctrlc` 依赖与 `EXIT_REQUESTED`，直接用 `tokio::signal::ctrl_c()`。
副作用：二进制更小、响应零延迟（当前 Ctrl+C 最坏延迟 100ms）。

### P2-5 死代码清理（4 处）

| 位置 | 内容 |
|---|---|
| `handle.rs:238` | `fn _parts(&mut self)` — 空函数，注释写 "for future" |
| `handle.rs:263-271` | `_assert_send_sync` / `_assert_handle_send_sync` — 已由编译器保证 |
| `node/lib.rs:183` | `fn _arc<T>(t: T) -> Arc<T>` — 抑制未用导入 |
| `node/event.rs:87,99` | `_cf_kind_name` / `_protocol_str` — 自述「reserved for future use」 |

同时 `handle.rs::_connections: Arc<AtomicUsize>` 统计了连接数却从不对外暴露，
要么接进 `Event`/getter，要么删掉。

### P2-6 依赖治理

| 问题 | 处理 |
|---|---|
| `reqwest` 开了 `features = ["blocking"]`，全项目**零处使用** | 移除，白赚二进制体积 |
| `tracing` 用了 7 处 `debug!/warn!`，但**从未安装任何 subscriber** → 日志全丢 | 要么安装（`CFP_LOG=debug` 才开），要么换成自建 log |
| `once_cell` 仅为 1 个 `Lazy<Runtime>` | 提 MSRV 到 1.80+ 换 `std::sync::LazyLock` |
| `cloudpipe-sdk/Cargo.toml` 逐条重复 workspace 依赖，且 `edition = "2021"` 硬编码 | 全部改 `workspace = true` |
| `napi`/`napi-derive` 未进 `[workspace.dependencies]` | 上提到 workspace 统一版本 |

### P2-7 加入 `[workspace.lints]` 与 `rust-toolchain.toml`

现在 clippy 只在 CI 用 `-D warnings` 命令行开启，本地 `cargo clippy` 发现不了。
移到 `Cargo.toml`：

```toml
[workspace.lints.rust]
missing_docs = "warn"
unsafe_code = "forbid"

[workspace.lints.clippy]
await_holding_lock = "deny"   # P0-2 类问题今后编译期即拦
pedantic = { level = "warn", priority = -1 }
```

并加 `rust-toolchain.toml` 钉版本 + 写明 MSRV（README 声称 1.70+，实际未强制）。

---

## P3 —— 功能增强

### P3-1 `IngressEntry` 过窄 —— `https` 自签证书无解（体验硬伤）

当前只有 `hostname` + `service`。因此：
`cfp https 8443` 指向一个自签证书的本地服务时，**必然握手失败且无法绕过**
（缺 `originRequest.noTLSVerify`）。同理缺 `connectTimeout`、`httpHostHeader`、
`noHappyEyeballs`、`originServer`。

**修法**：新增 `origin_request: Option<OriginRequest>` 结构（serde `skip_serializing_if`），
并在 `TunnelBuilder` 暴露：

```rust
.no_tls_verify(bool)
.origin_connect_timeout(Duration)
.origin_host_header(String)
```

**优先级最高的功能项** —— 这是「暴露本地 https 开发服务器」最高频的诉求。

### P3-2 Cloudflare API 限流重试（10429）

代码已经把 10429 分类成 `CloudflareErrorKind::RateLimited`，但**没有任何重试**。
README 还专门写了「10429 rate limit」的错误处理——目前只是展示文本。

**修法**：`CloudflareApi` 内置 `retry` 层：识别 `RateLimited` / HTTP 429 / 5xx，
按 `Retry-After` 头或指数退避重试 3 次，仅对幂等方法自动重试。

### P3-3 `resolve_zone` 与文档矛盾：所谓「可选」实际必须三者同给

`builder.rs` 里 `account()` / `zone()` 的文档都写着「**Optional.** Override the value
the SDK auto-discovers」，但 `resolve_zone` 的实现是三元组模式匹配：
要么三者齐全、要么全空，**只给两个会直接报错**。

**修法**：实现真正的逐字段补全（given account → `list_tunnels` 反查；
given zone → `find_zone` 反查 account+domain），并补上表驱动单测覆盖 8 种组合。

### P3-4 `Zone.account` 是非 `Option` + `Default`

CF 若不返回 `account`，得到空串 `""`，下游报出莫名其妙的错。

**修法**：改 `Option<ZoneAccount>`，`resolve_zone` 里 `ok_or_else` 出明确提示。

### P3-5 请求体类型被 `Default` 掩盖

`client::request<T: Deserialize + Default>` 是为了让 DELETE 的 `result: null`
能通过，但副作用是：`list_zones` 收到 `result: null` 会返回**空 vec**，
最终报「token 有效但没有任何 zone」——完全误导。

**修法**：引入 `NullAsDefault<T>` 只对无返回值的调用点启用；
`list_*` / `find_*` 改用严格的 `Option<T>` 解包并在 `None` 时报协议错误。

### P3-6 URL 拼接未编码

`format!("/zones/{zone_id}/dns_records?name={full_name}")` 直接内插用户输入。
`subdomain` 有校验兜底，但 `domain` **没有任何校验**。

**修法**：加 `urlencoding` 依赖（或手写 query builder），并给 `domain` 补
域名格式校验（RFC 1123）。

### P3-7 `cleanup` 缺少 dry-run 与一致性

- 计数把「1 个隧道 + 1 条 DNS」算成 2，`Removed N orphaned resource(s)` 有歧义；
- 纯串行 N 次 API 往返，大 zone 下极慢；
- 无 `--dry-run`，用户不敢在生产 zone 上试。

**修法**：返回结构化 `CleanupReport { tunnels_removed, dns_removed, errors: Vec<..> }`，
CLI 加 `--dry-run` / `--json`，并用 `futures::stream::buffer_unordered` 并发。

### P3-8 Node API 缺失与缺陷

| 项 | 问题 |
|---|---|
| `wait()` 后 `close()` **会死锁** | `session_loop` 串行处理命令，`Wait` 永不返回则 `Stop` 排不上队。**真 bug** |
| 无 `off(event, cb)` | 回调只增不减 → 长驻进程泄漏；且 TSFN 未 unref，可能阻止进程退出 |
| 无 `Finalize` | JS 丢弃 listener 即泄漏隧道（见 P1-10） |
| `port: 0` 被接受 | 应校验 `1..=65535` |
| `try_send` 静默丢事件 | 见下 |
| 无 `update()` | ngrok 有 `listener.update()` 可热改 ingress/端口，cloudpipe 缺失 |
| 无 `cleanup()` | CLI 有 `cfp cleanup`，Node/SDK 用户没有对应能力 |

**修法**：`session_loop` 用 `tokio::select!` 分派（Wait 用 oneshot 可被 Stop 打断），
或改 `Command::Stop` 优先级抢占；补 `off`/`Finalize`/端口校验/`update`/`cleanup`。

**关于 `try_send`**：`listener.rs:104` 的注释解释了为什么不能 `blocking_send`
（在 tokio runtime 内会 panic）。正解是改用 **`send().await`** —— 把 event pump
改成消费者侧 await，或者干脆用 `unbounded_channel()`（这些事件速率极低，
有界通道的背压收益≈0，却牺牲了可靠性）。

### P3-9 CLI 交互与可观测性

- **无 `--json` / 结构化输出**：无法接 CI / 脚本。
- **无日志级别控制**：`CFP_DEBUG_LOG` 是个隐藏开关（`session.rs` `log_debug`），
  没有正式的 `--verbose` / `CFP_LOG=debug|info|warn`。
- **无 TTY 检测**：输出重定向到文件时仍写 ANSI 转义符，`colored` 只看
  `is_terminal(stdout)` 而 CI 里常被强制开启。
- **emoji + 定宽 `─`.repeat(56)**：Windows CMD / 非 UTF-8 终端会错位
  （实际宽度已是 62 ≠ 56）。应做宽度感知的表格渲染 + `--ascii` 降级。
- **`Event::Cleaned` 文案错误**：`ui.rs` 打印 "Cleaning up tunnel and DNS record..."，
  但该事件定义是「**已清理完毕**，生命周期的最后一个事件」。文案与语义相反。

### P3-10 SDK 能力补齐

- `handle.wait()` / `stop()` 无超时 —— Cloudflare API 挂住则永不返回。
- 无 `cleanup` 便捷入口（`cleanup()` 需要用户自拼 account/zone/domain）。
- 无多隧道并行管理（一个 `SessionManager` 持有 N 个 handle）。
- `CloudflareApi::http()` 是 `#[allow(dead_code)]` 的死 API。

---

## P4 —— 测试 / CI / 文档

### P4-1 测试盲区

`handle.rs`、`session.rs`、`cleanup.rs`、`ui.rs` **零单测**；
Node 侧单测（P0-3）根本编译不了。

**补齐**：
- `cleanup.rs`：分页 / 计数 / dry-run 的表驱动单测；
- `session.rs`：用 mock API + fake cloudflared 覆盖「bootstrap 中途失败回滚」、
  「连续崩溃超过预算」、「关机先杀进程后清理」；
- `ui.rs`：把渲染抽成 `-> String` 纯函数后单测。

### P4-2 测试卫生

| 问题 | 修法 |
|---|---|
| 集成测试临时目录用 `format!("cfp-test-{}", process::id())` → 同进程内并行用例**会撞名** | 换 `tempfile::TempDir` |
| 用例跑完不清理临时目录 | `TempDir` 自动析构 |
| `config.rs` 测试直接 `std::env::set_var` → 与同 binary 其他测试**竞态**，且 Rust 2024 下是 `unsafe` | 加串行化 guard 或注入 `env_source` 抽象 |
| `full_lifecycle_with_mock_api` 名不副实——它绕过 builder 直调 `CloudflareApi`，源码里有 10 行注释在为此道歉 | 拆成 `cloudflare_api_against_mock` + 真正的 full-lifecycle 用例 |
| mock server 只 `read()` 一次 16KB 包 | 完整解析 Content-Length |

### P4-3 CI 修正

1. `ci.yml` 的 `cargo {build,test,clippy} --workspace --all-targets` 必须
   排除 `cloudpipe-node`（P0-3 修完后可加回）——当前三条全红。
2. `Swatinem/rust-cache` 的 `workspaces: crates/cloudpipe-sdk crates/cloudpipe -> target`
   **不是合法 YAML 列表**，会被当成单个字符串；直接删掉让它自动探测。
3. 新增 `cargo-deny`（license/advisories）、`cargo-audit`、`cargo-semver-checks`（SDK 公开 API）。
4. `npm.yml` 修 P0-4；补 `linux-arm64-gnu` / `x86_64-musl`（matrix 里注释写着推迟到 v0.2.0，
   配 `mlugg/setup-zig@v2` 即可）。
5. 加 `cargo fmt` / `clippy` 的 `RUSTFLAGS` 一致性检查。

### P4-4 文档矛盾清理

| 矛盾 | 现状 |
|---|---|
| 4h 限制 | `lib.rs:25`、`session.rs:339` 注释仍提「4h age limit」，功能已在 `a1405ca` 移除 |
| Node 预编译平台 | 根 README 称「macOS×2 + Linux(x64-gnu, x64-musl, aarch64-gnu) + Win」，`cloudpipe-node/README.md` 与 `package.json` 的 `files` 只有 4 个（无 musl / arm64） |
| 错误处理 | 根 README 称 API 错误「翻译为可操作文本」，实际 CLI 直接 `anyhow::Error::msg` 丢掉了 `Error` 类型，无任何 SDK 错误码 → 建议动作的映射 |
| 限流 | README 提 10429 处理，代码无重试（见 P3-2） |
| `cfp -v` | 见 P0-5 |

---

## 实施顺序建议

```
阶段 1（止血，1 个 PR）           阶段 2（可靠性，2~3 个 PR）
────────────────────────────     ────────────────────────────
P0-1 rsplit_once 修复             P1-1 token 改 --token-file
P0-2 抽 purge() 解决跨 await 锁   P1-2 下载校验 + 原子写 + 锁
P0-3 node 单测可编译化            P1-3 API 超时
P0-4 npm preflight 包名           P1-4 关机顺序修正
P0-5 README -V                   P1-5 去轮询 + 日志缓存
P0-6 BootstrapGuard 回滚          P1-6 Supervisor 状态机
P0-7 StartedBuilder Drop          P1-7/8/9 错误吞噬 / 分页 / 子域名碰撞
P0-8 校验前置                     P1-10 Drop 释放
                                 P1-11 配置原子写 + version

阶段 3（架构，2 个 PR）           阶段 4（增强，3~4 个 PR）
────────────────────────────     ────────────────────────────
P2-1 session.rs 拆 7 个子模块     P3-1 noTLSVerify 等 originRequest
P2-3 main.rs Ctx 去重             P3-2 10429 重试退避
P2-4 删 ctrlc 换 tokio::signal    P3-3/4/5 resolve_zone + 类型收紧
P2-5 死代码清理                   P3-6 URL 编码 + domain 校验
P2-6 依赖治理（blocking/tracing） P3-7 cleanup dry-run + 报告结构
P2-7 workspace lints + toolchain  P3-8 Node 死锁/off/Finalize/update

阶段 5（收尾）                   
────────────────────────────
P4-1 补齐单测矩阵
P4-2 测试卫生（TempDir / env 抽象）
P4-3 CI 修正 + deny/audit/semver
P4-4 文档矛盾清理
```

---

## 附：每项的「验证方式」

| 项 | 验证 |
|---|---|
| P0-1 | 单测断言 `domain == "example.com"` |
| P0-2 | `cargo clippy --workspace --all-targets -- -D warnings` 绿 |
| P0-3 | `cargo test --workspace` 绿且 Node 单测实际执行 |
| P0-4 | `npm view @xinggao/cloudpipe` 预检通过 |
| P0-6 | 新用例：mock API 让 `spawn` 失败 → 断言 tunnel/DNS 均已删除 |
| P1-1 | `ps -ef \| grep cloudflared` 不含 token |
| P1-2 | 并发启动 10 个进程，只产生一次下载；断网时超时退出 |
| P1-8 | mock 返回 3 页 zone，断言全部拿到 |
| P3-1 | `cfp https <自签端口>` 可正常访问 |
| P3-8 | `await wait(); await close();` 不再挂起 |


---

# 实施结果

## 与计划的主要差异

计划是提案，实施中被证据推翻或改进的部分：

| 条目 | 计划的方案 | 实际做法 | 原因 |
|---|---|---|---|
| P0-6 | RAII `BootstrapGuard` / `scopeguard` | `Created { tunnel, dns }` 台账 + 显式 `rollback()` | 显式台账比 Drop guard 更容易看出「哪些资源被创建了」，且不需要新依赖 |
| P1-2 | 流式下载 + `fs2` 文件锁 | 校验后原子 rename；并发由 rename 抢占 | 单进程场景下 `.part` + `rename` 已足够；见下方「已知限制」 |
| P1-5 | `OnceLock<PathBuf>` 缓存 + `debug_log!` 宏 | 整体删除 `log_debug`，改用 `tracing` + CLI/Node 各自的 subscriber | `CFP_DEBUG_LOG` 是个无文档的隐藏开关；`tracing` 已是既有依赖，装上 subscriber 才是正解 |
| P2-1 | 拆 7 个模块 | 拆 6 个（`log.rs` 取消，`policy.rs` 新增） | `log.rs` 的职责随 P1-5 一起消失；`policy.rs` 把策略类从 supervisor 机制里分出去 |
| P3-5 | 引入 `NullAsDefault<T>` | `request_unit -> Option<T>` + 独立的 `request_ignore` | 两个函数比一个 trait 边界更清楚，且不需要 unsafe/unsafe-free 的 Default 约束 |
| P3-10 | `SessionManager` 多隧道管理 | 未做 | 这是一个新特性而非修复，且当前没有对应的使用场景；其余 3 条（stop 超时、cleanup 便捷入口、删除死 API）已做 |

## 实施中发现并修复的额外缺陷

计划本身没预见到的：

1. **`originRequest` 序列化键名错误**（client.rs）—— 类型和 CLI 开关都加了，
   但 serde 生成了 `origin_request`，Cloudflare 会静默忽略。README 主打的
   「自签 HTTPS 可用」因此实际不生效。
2. **cloudflared 摘要表是伪造的** —— 首次实现时填的是占位哈希，会让所有没有
   PATH 上 cloudflared 的机器首次运行即失败。改为只填经 release 实际校验的
   摘要，并新增 `verify-checksums` CI job 作为长期保障。
3. **token 文件 unlink 竞态**，以及一个**恒真**的单测（断言了流程中从不出现的
   `cloudflared.part`，所以无论代码怎么写都通过）。第一次「修复」只是在下一次
   运行时清理残留文件，并没有消除竞态——真正消除它的是改用 `TUNNEL_TOKEN`
   环境变量交接（见 P1-1 的后续修正）。配套还修了：致命日志被 stderr 分类器
   丢弃（用户只看到无限重启，看不到原因）、子进程退出时不上报 exit code 与
   stderr 尾部、以及崩溃循环没有预算（子进程一启动就死时无限重试）。
4. **`ApiError::Timeout` 从未被构造**，导致 CLI 的超时提示是不可达分支。
5. **`is_retryable` 把 Cloudflare 业务码当 HTTP 状态码比较**，`7003`
   （could-not-route）被误判为可重试，白重放三次。
6. **cleanup 重复计数**：死隧道的 DNS 记录会被两个分支各报一次。
7. **`resolve_zone` 的文档与实现矛盾**：`account()` / `zone()` 文档称「可选」，
   实现却要求三者同给。
8. **deny.toml 语法错误 + 与真实依赖图不符**，会让新增的 audit job 必红；
   修正后又暴露出一个真实的 rustls advisory。
9. **MSRV 声明错误**：README 写 1.70+，但 `LazyLock` 要 1.80。
10. **P0-3 完成后 `--exclude cloudpipe-node` 变成 no-op**，四处「cannot link」
    的注释随之失效。

## 已知限制

诚实记录尚未覆盖的部分：

- **cloudflared 摘要表只覆盖 Linux x86_64 / arm64**。其他平台需要手动安装，
  或设 `CFP_CLOUDFLARED_SHA256`。这是刻意的：无法校验的可执行文件绝不安装。
  `verify-checksums` job 会打印缺失项，补齐是一条命令的事。
- **无跨进程文件锁**。`.part` + 原子 `rename` 保证了并发读取者只会看到完整
  文件，但两个进程仍会重复下载一次。代价是一次带宽，而非正确性问题。
- **`SessionManager`（多隧道并行管理）未做** —— 属于新特性，见上表。
- **`--json` 只覆盖 `cfp cleanup`**，隧道/key/domain 仍是人类可读输出。
- **Node 的 `off(event)` 是整事件清空**，不能移除单个回调（与 ngrok 的
  `off(event, cb)` 语义不同）。
