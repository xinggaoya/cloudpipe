# 已发布产物的一致性与可用性修复 — 设计文档

- 日期：2026-11-01
- 状态：已批准，待实施
- 影响范围：`crates/cloudpipe-sdk`、`crates/cloudpipe`、`crates/cloudpipe-node-core`、`crates/cloudpipe-node`、CI、README

## 1. 背景与动机

`cfp` 与 `@xinggao/cloudpipe` 都已公开发布（npm v0.1.1、GitHub Releases 有公开安装方式）。
审计发现：**已经对外承诺的能力有一半没有兑现**，且缺陷集中在"用户第一次使用"就会撞上的路径上。

| 编号 | 现象 | 影响面 |
|---|---|---|
| D1 | cloudflared 自动安装在**所有平台**上都失败：摘要表只有 2 项，且两项摘要都**与上游不符** | 所有用户 |
| D2 | Node 启动期生命周期事件全部丢失 | 所有 npm 用户 |
| D3 | npm 包发布的 `on()` 回调签名与运行时不一致 | 所有 npm 用户 |
| D4 | 手写 tracing subscriber 丢弃所有数字字段 | CLI + Node |
| D5 | `LogLevel::Info` / `Warn` 是永不产生的死承诺 | SDK + Node 类型 |
| D6 | 协议大小写敏感性三个端不一致 | CLI vs SDK vs Node |
| D7 | `cfp key <token>` 把密钥放 argv | CLI |
| D8 | `cfp key` 先干活后失败，凭证可能丢失 | CLI |
| D9 | `key_status` 硬编码路径且谎报 token 来源 | CLI |
| D10 | 事件渲染依赖魔法数字；协议经字符串 round-trip | CLI |
| D11 | Node 错误码表三处字面量重复；缺 `INVALID_DOMAIN`；5xx 被降级 | Node |
| D12 | Node `off(event)` 清空整事件；`wait()` 单槽；无 `cleanup()` | Node |
| D13 | 两份 `.d.ts` 会漂移；`npm/index.js` 的 optionalDependencies 回退链为空 | Node |
| D14 | CLI `main.rs`、`ui.rs::render_event`、Node 端到端路径均无测试 | 全栈 |

本 spec 的范围是**修复 D1–D14**，不引入新特性。
新特性（多路由、Cloudflare Access 鉴权、`cfp list/status/logs`、`config_src=local`、生态与分发）
各自单独开 spec —— 它们会改变公共 API 形状，与本 spec "只修 bug、不改 API 形状"的原则冲突。

## 2. 目标与非目标

### 目标

1. 每一个在 README / npm README / `index.d.ts` 里被承诺的行为，代码里都能兑现。
2. 三个端（CLI / Rust SDK / Node）对同一概念（协议、错误码、日志级别）的行为一致。
3. 上述行为有测试兜底，且不引入新的第三方依赖。
4. 不破坏 `cloudpipe-sdk` 0.1.x 的既有公共 API 语义（唯一新增是 `cloudpipe_sdk::logging`）。

### 非目标

- 不做多隧道 / 多路由 / Access 鉴权 / 本地仪表盘。
- 不引入 `tracing-subscriber`、`tokio-util` 等新依赖（`logging.rs:9-13` 明确说明为单二进制体积刻意不用）。
- 不改变 npm 包的公共调用形状（`connect(options)` → `listener`，`on(name, cb)` 单参数回调）。
  修复 D3 的方式是**加一层 facade** 让运行时符合文档，而不是改文档迁就运行时。

## 3. 变更组 1 — cloudflared 分发（D1）

### 3.1 现状

`crates/cloudpipe-sdk/src/binary.rs:81-101` 的 `ASSET_SHA256` 只钉了 `cloudflared-linux-amd64`
与 `cloudflared-linux-arm64` 两项。而 `download_asset_name`（`binary.rs:182-200`）支持
darwin / windows / linux-arm 共 7 个资产。

**更严重的是：钉住的那两项摘要本身就是错的。**

| 资产 | 仓库里钉的 | 上游 `2025.11.1` 实际的 |
|---|---|---|
| `cloudflared-linux-amd64` | `cc348a425d7944d3fad9d8ac124b06725d471634153b58e0d57c72a79cc1ea4e` | `991dffd8889ee9f0147b6b48933da9e4407e68ea8c6d984f55fa2d3db4bb431d` |
| `cloudflared-linux-arm64` | `3d67ef9295f7d5528a1acd74e8db79638a283b46f8922d5427545ff3f8ca3cc7` | `9979dc152097a29b6de4d1ef13e2f1821c67a6f096f88cc18f0fd25106305d3a` |

上游值由两条独立路径确认：本地下载后 `sha256sum`，以及 GitHub Release API 的
`digest` 字段（`GET /repos/cloudflare/cloudflared/releases/tags/2025.11.1`）——
两者逐字节一致。

后果分两种，**两种都让自动安装彻底不可用**：

- Linux（x86_64 / arm64）：下载成功但校验失败 → `InstallError::ChecksumMismatch`。
- macOS / Windows / linux-arm：没有摘要 → `InstallError::UnverifiedAsset`，直接拒绝安装。

也就是说，**今天任何没有预先安装 `cloudflared` 的用户，第一次运行 `cfp` 都会失败**。
根 README:315-318 与 `crates/cloudpipe-node/README.md:17-18` 承诺的
"No separate `cloudflared` install required" 在任何平台上都不成立。

`ci.yml` 的 `verify-checksums` job 本应捕获这一点 —— 它的抽取逻辑是正确的
（能提取出那两条），比对也必然失败。因此**该 job 在 `main` 上当前是红的**，
只是它红的方式（"WRONG cloudflared-linux-amd64"）被淹没在 CI 日志里，
没有被当作发布阻断处理。

顺带说明：摘要值从 `4fa905a` 引入起就是错的 —— 那次提交同时引入了
`CLOUDFLARED_VERSION` 与这张表，两者从未与上游对齐过。

### 3.2 变更

1. **补全并修正摘要表**：`ASSET_SHA256` 增加
   `cloudflared-darwin-amd64.tgz`、`cloudflared-darwin-arm64.tgz`、
   `cloudflared-windows-amd64.exe`、`cloudflared-windows-386.exe`、`cloudflared-linux-arm`，
   并**修正两条已存在的 Linux 摘要**（当前值与上游不符，见 §3.1）。

   摘要对象是**下载到的原始字节**：`binary.rs:428` 在解包之前对响应体做 SHA-256，
   所以 `.tgz` 的摘要就是 `.tgz` 文件自身的摘要，与解包逻辑无关。

   `2025.11.1` 的正确值（本地下载 `sha256sum` 与 GitHub Release API 的 `digest`
   字段双向确认）：

   | 资产 | SHA-256 |
   |---|---|
   | `cloudflared-darwin-amd64.tgz` | `155a288fef19dba08f0c7145c16a207baf137462d8a1289a78bf8564f9e51244` |
   | `cloudflared-darwin-arm64.tgz` | `45cfbb59a720f60b873906aa6469f8c4058f26be6d351c3e2920bc9cb4714273` |
   | `cloudflared-linux-amd64` | `991dffd8889ee9f0147b6b48933da9e4407e68ea8c6d984f55fa2d3db4bb431d` |
   | `cloudflared-linux-arm64` | `9979dc152097a29b6de4d1ef13e2f1821c67a6f096f88cc18f0fd25106305d3a` |
   | `cloudflared-linux-arm` | `b0ba2cda4a91ed212b0b1328f5209f97786e89d5e7cbddcfffa9570e56f69000` |
   | `cloudflared-windows-386.exe` | `1e76c0d4e6ff2576db46ad1a7397415729fbaf886b22aa49327f66185a2e8fc8` |
   | `cloudflared-windows-amd64.exe` | `413f9b24dc6e61a455564651524f167b8ce29ac4ccd40703dea7af93cd37ed39` |

2. **在 `binary.rs` 增加一个生产可见的资产清单** `ALL_ASSETS`：

   ```rust
   /// Every release asset this SDK knows how to install, in a fixed order.
   pub const ALL_ASSETS: &[&str] = &[
       "cloudflared-darwin-amd64.tgz",
       "cloudflared-darwin-arm64.tgz",
       "cloudflared-linux-amd64",
       "cloudflared-linux-arm64",
       "cloudflared-linux-arm",
       "cloudflared-windows-386.exe",
       "cloudflared-windows-amd64.exe",
   ];
   ```

   目前 `binary.rs` 里已有一个同名常量，但它只存在于 `#[cfg(test)]` 块内
   （`binary.rs:713`），生产代码与 CI 都看不到它 —— CI 的 `verify-checksums` job
   正是因此算错了覆盖缺口（见 §3.2 第 3 点）。新常量放在生产模块中，
   `download_asset_name` 的每个分支都必须命中 `ALL_ASSETS`（由单测断言）。

3. **新增生成脚本** `scripts/gen-cloudflared-checksums.sh`：
   - 从 `crates/cloudpipe-sdk/src/binary.rs` 解析 `CLOUDFLARED_VERSION` 与 `ALL_ASSETS`；
   - 对每个资产下载并计算 SHA-256；
   - 输出 `<asset>\t<digest>` 的 tsv 到 stdout。

   脚本必须**直连 `github.com` 的官方 release URL 取摘要**。`CFP_GITHUB_PROXY` 镜像只可作为
   下载 fallback，不可用于取摘要 —— 镜像可能改写响应，用镜像的摘要会导致 in-tree 表与官方
   release 不一致，`verify-checksums` job 随即失败。

3. **CI `verify-checksums` job 改为"生成 → 比对"**：

   当前 job 有两个缺陷：

   - 只在"已钉条目的摘要算错"时失败；"某个平台压根没钉"只打印一条 `::notice::`。
   - 那条 notice **实际上是死代码**：`ci.yml:126` 用
     `src.split("const ALL_ASSETS", 1)[1]` 取资产名，而 `ALL_ASSETS` 只存在于
     `#[cfg(test)]` 块中；`pinned` 那一侧（`ci.yml:129`）则取了 `ASSET_SHA256`
     之后的**全部** `"cloudflared…"` 字面量 —— 集合比 `names` 大，
     `names - pinned` 恒为空，于是"哪个平台没钉摘要"从未被报告过。

   改为：

   - 运行生成脚本得到期望的 tsv；
   - 与 in-tree 表逐行 diff；
   - 不一致 → 失败，并打印可直接粘贴的替换片段；
   - 一致 → 通过。

   这样"表里少一个平台"与"表里摘要过期"都是硬失败，覆盖缺口的报告不再依赖那段
   已经失效的正则。

4. **文档同步**：
   - `binary.rs:91-100` 的"故意留空"注释改为真实状态说明；
   - 根 README「Limitations」中"摘要表只覆盖 Linux x86_64 和 arm64"一段删除，
     改为说明"所有发布的平台都已钉摘要；未钉的平台需要自行安装或设 `CFP_CLOUDFLARED_SHA256`"。

### 3.3 验收

- `cargo test -p cloudpipe-sdk` 中新增断言：7 个 `download_asset_name` 可达的资产
  在 `expected_sha256` 下都返回 `Some`。
- `scripts/gen-cloudflared-checksums.sh` 输出与 in-tree 表逐行相等。
- 在 macOS 上跑 `cfp` 且 PATH 上无 `cloudflared` 时，能自动安装成功。

### 3.4 风险

- 需要联网下载约 200 MB 资产。GFW 环境走 `CFP_GITHUB_PROXY` 镜像。
- 摘要必须来自官方 URL；若官方 asset 在中途被替换（极不可能，release 资产不可变），
  `verify-checksums` 会失败 —— 这正是期望行为。

## 4. 变更组 2 — Node 事件模型（D2、D3、D12、D13）

### 4.1 D2：启动期事件全部丢失

**根因**：`listener.rs:137-160` 中事件本身**没有丢** —— `on_event` 闭包在 `builder.start()`
之前注册，事件进的是 unbounded mpsc（`listener.rs:133`）。丢失发生在**派发环节**：
`event_pump`（`listener.rs:284-300`）在派发时才查回调注册表，而注册表只能由 `Listener::on`
填充；`Listener` 要等 `connect()` resolve 才存在。因此
`banner` / `creatingTunnel` / `ingressConfigured` / `dnsCreated` / `cloudflaredStarted` /
首次 `edgeConnected` 全部投进空注册表后被丢弃。

**变更**：把回调注册表从裸 `HashMap` 升级为结构体，额外持有一个待投递队列。

```rust
struct CallbackTable {
    /// 每个事件名已注册的回调。
    by_event: HashMap<String, Vec<EventCallback>>,
    /// 已产生但当时无任何回调的事件，按产生顺序保存。
    pending: VecDeque<(String, String)>,
}
```

- `event_pump`：取某个事件名的回调快照时，若快照为空，则把该事件压入 `pending`
  （上限 `PENDING_EVENT_CAPACITY = 512`，超限丢最旧），而不是丢弃。
- `Listener::on`：注册新回调后，从 `pending` 中取出**同名**条目，按产生顺序依次投递给该回调。
- `Listener::off` / `remove_all_listeners`：同时清理 `pending` 中对应事件名的条目，
  避免"已取消订阅的事件又被补投"。

**时序语义**：事件在 t 时刻产生、回调在 t' 时刻注册（t' > t）时，回调在 t' 时刻收到该事件。
内容正确、顺序正确，但接收时刻晚于产生时刻。这与 ngrok 的同类取舍一致，必须写进
npm README 与 `index.d.ts` 的注释。

### 4.2 D3：回调元数与运行时不一致

**根因**：手写的 `crates/cloudpipe-node/index.d.ts:201` 声明

```ts
on(event: ListenerEventName, callback: (payload: string) => void): void;
```

但 napi-rs 的 `ThreadsafeFunction<String, ()>` 是 `CalleeHandled = true`，运行时实际调用
`cb(null, payload)` —— payload 是**第二个**参数。napi 自动生成的 `npm/index.d.ts:21`
才是正确的（`callback: ((err: Error | null, arg: string) => void)`）。
而 `.github/workflows/npm.yml` 明确发布的是那份手写的 `.d.ts`。

后果：README:299-302 与 `examples/with-events.js:45` 里的 `JSON.parse(payload)`
实际执行 `JSON.parse(null)` → `null`。**npm 用户拿到的每个事件载荷都是 null。**

**变更**：新增一层手写 JS facade `crates/cloudpipe-node/js/index.js`（CommonJS）：

```js
const native = require('../npm/index.js')

// facade 自持回调表，对外提供 ngrok 语义的 on/off
const registry = new Map()   // eventName -> Set<callback>

function connect(options) {
  const listener = native.connect(options)
  return wrapListener(listener)
}
```

facade 的职责：

1. **适配回调元数**：向 native 注册 `(err, payload) => { if (err) return; dispatch(name, payload) }`，
   对外仍是单参数 `cb(payload)`。README 与现有示例无需改动即可正确工作。
2. **`off(event, cb)` 精确移除**：facade 自持 `Map<event, Set<cb>>`，
   每个事件名只向 native 注册一次；`off(event, cb)` 只删该回调，
   集合空了才调 native 的 `off(event)`。
3. **`remove_all_listeners()`** 透传。
4. **`cleanup(options)`**：新增导出，调用 SDK 的 `cleanup_with_report` /
   `preview_cleanup`（`cloudpipe-sdk/src/cleanup.rs`）。Node 侧此前完全没有这个入口，
   Node 应用崩溃后留下的孤儿隧道/DNS 只有 CLI 能回收。
5. **`cleanup` / `previewCleanup` 的选项形状**：`{ token, account, zone, domain, dryRun }`，
   与 `ConnectOptions` 的 token/domain/account/zone 字段保持一致。

`package.json` 相应调整：

- `"main": "js/index.js"`；
- `"files"` 去掉 `npm/index.d.ts`（napi 生成的、较弱的类型，会与手写版漂移），
  只保留手写的 `index.d.ts` 作为唯一类型来源；
- `"files"` 增加 `js/index.js`；
- `"types"` 仍指向手写的 `index.d.ts`。

### 4.3 D12：`wait()` 单槽 + `off` 语义

- `wait()`：当前 `session_loop` 用单个 `Option<oneshot::Sender<()>>`（`listener.rs:210, 228`），
  第二次 `wait()` 顶掉第一个，第一个的 sender 被 drop，其 `rx.await` 以
  `channel_dropped` 失败 → JS 侧得到 `[OTHER] session task dropped the reply channel`。
  **变更**：改为 `tokio::sync::Notify` + `AtomicBool` 的"完成标志"模型 —— 多个等待者都能被唤醒，
  且唤醒是幂等的。
- `off(event)`：native 侧保留"清空该事件全部回调"的语义（供 facade 内部使用），
  对外由 facade 提供 `off(event, cb)`。

### 4.4 D13：类型文件与平台回退

- 手写 `index.d.ts` 补齐 `INVALID_DOMAIN`（见 §5.2）。
- **平台回退链是误导性的**。`npm/index.js`（napi 生成）的加载顺序是：先
  `require('./cloudpipe.<platform>-<arch>.node')`（**包内自带**，4 个 target 都有），
  再退到 `require('@xinggao/cloudpipe-<platform>-<arch>')` —— 后者**从未在 npm 上发布过**，
  `package.json` 里也没有对应的 `optionalDependencies`。

  因此**不能**通过"补 `optionalDependencies` 声明"来修：那会把 npm 指向不存在的包。
  正确做法是承认"本包自带二进制、不使用 napi 的分平台包布局"：

  - facade 在 `require('../npm/index.js')` 失败时捕获异常，抛出可执行的错误：
    `cloudpipe does not ship a prebuilt binary for <platform>-<arch>.
     Supported: linux-x64-gnu, darwin-arm64, darwin-x64, win32-x64-msvc.
     Build from source: npm run build:local`
  - `package.json` **不新增** `optionalDependencies`。
  - musl / linux-arm64 / darwin-universal 的构建留待 v0.2.0
    （`npm.yml` 已有此约定），本 spec 不动构建矩阵 —— 但错误信息必须说清楚。

  注意 `npm/index.js` 是 napi **生成**的文件，每次 `napi build` 都会被覆盖，
  因此这个改动必须落在 facade 里，不能手改生成文件。

### 4.5 验收

- `npm test` 中新增端到端用例：连 mock API → 收到 `dnsCreated` → 载荷可 `JSON.parse` 且字段正确
  → `off(event, cb)` 后不再收到 → `close()`。
- `index.d.ts` 的 `on()` 签名与 facade 实际行为一致（用 `tsc --noEmit` 对 `index.d.ts` 做类型检查）。

## 5. 变更组 3 — 错误契约（D11）

### 5.1 单一来源

现状：`CloudpipeErrorCode::as_str()`（`node-core/src/error.rs:47`）是权威定义，
但 `node-core/src/parse.rs:31-41` 与 `parse.rs:59-64` 用**字符串字面量**匹配，
`node/src/lib.rs:270-301` 又把码表抄了一遍。

`parse.rs:28-30` 的注释承诺"a future code rename surfaces as a compile error in this crate"，
但 `let _ = parent_code;`（`parse.rs:54, 67`）只是让编译器闭嘴，实际不会报错。

**变更**：

1. 在 `node-core/src/error.rs` 增加
   ```rust
   impl CloudpipeErrorCode {
       /// 全部错误码，顺序即 `errorCodes()` 的返回顺序。
       pub const ALL: &'static [Self] = &[ /* ... */ ];
       /// 由 `as_str()` 的逆映射解析，未知码返回 `None`。
       pub fn from_str(code: &str) -> Option<Self>;
   }
   ```
   `from_str` 用 `Self::ALL` 逐个比对 `as_str()`，因此**改名必然同时改到 `as_str` 与 `from_str`
   两处，`match` 的穷尽性由编译器保证**。
2. `parse.rs` 改为调用 `CloudpipeErrorCode::from_str` / `CloudflareApiKind::from_str`，
   删除两处字符串字面量 match 与 `let _ =` 占位。
3. `node/src/lib.rs` 的 `error_codes()` 改为 `CloudpipeErrorCode::ALL.iter().map(|c| c.as_str())`。
4. `CloudflareApiKind` 同样增加 `ALL` 与 `from_str`。

### 5.2 5xx 不再降级

`node-core/src/error.rs:126` 把 `CloudflareErrorKind::ServerError` 映射成
`CloudflareApiKind::Other`，导致 JS 侧无法区分"5xx"与"未知码"。

**变更**：`CloudflareApiKind` 增加 `ServerError` 变体（`as_str()` = `"SERVER_ERROR"`），
`kind_from_sdk` 正确映射。`CloudflareErrorKind::Other(_)` 仍映射为 `Other`。

### 5.3 结构化错误对象

现状：Node 侧所有错误都是 `napi::Error`，其 `message` 形如 `"[CODE] 人类可读文本"`，
JS 侧只能靠字符串前缀。

**变更**：`crates/cloudpipe-node/src/error.rs` 产出的错误附带结构化字段：

- `error.code`：`"CLOUDFLARE_API"` 这样的裸码（无 `[` `]`）；
- `error.kind`：子码（如 `"RATE_LIMITED"`），没有时为 `undefined`；
- `error.message`：保留 `[CODE] 文本` 的旧格式，**不破坏**现有消费者。

napi-rs 的 `Error` 类型不支持自定义字段，因此实现方式是：把 code/kind 编码进
`napi::Error` 的 `reason` 之外的信息 —— 具体为在 facade 层把 `error.message` 的前缀
解析成 `code`/`kind` 并挂到 error 对象上（facade 已经掌握 `parseErrorCode` 的逻辑）。
Rust 侧只需保证前缀格式稳定不变。

`index.d.ts` 增加：

```ts
export interface CloudpipeError extends Error {
  /** 裸错误码，如 "MISSING_CREDENTIAL"。 */
  code: string
  /** 子码，如 "RATE_LIMITED"；无子码时为 undefined。 */
  kind?: string
}
```

### 5.4 验收

- `node-core` 新增测试：每个 `CloudpipeErrorCode` 的 `as_str()` 经 `from_str()` 往返相等；
  改名字会让 `from_str` 的穷尽性检查失败（由编译器保证，测试断言往返即可）。
- `node/src/lib.rs` 的 `error_codes()` 与 `CloudpipeErrorCode::ALL` 一致（单测断言）。

## 6. 变更组 4 — CLI 一致性（D4–D10）

### 6.1 D4：tracing subscriber 丢字段

`crates/cloudpipe/src/logging.rs:111-120` 的 `MessageVisitor` 只实现了 `record_debug`。
`tracing::field::Visit` 的 `record_u64` / `record_i64` / `record_u32` / `record_i32` /
`record_bool` / `record_f64` / `record_str` / `record_error` 默认全是 no-op。
于是 `session/supervisor.rs:98` 的 `warn!(attempts = ..., "{err:#}")` 只打印消息，
**不打印 attempts**。`crates/cloudpipe-node/src/lib.rs:98-103` 是逐字复制，同样的问题。

**变更**：

1. 把 subscriber 移到 SDK，新增公共模块 `cloudpipe_sdk::logging`：
   ```rust
   pub mod logging {
       pub enum LogSetting { Level(Level), Off }
       impl LogSetting { pub fn parse(...); pub fn resolve(verbose: bool); }
       pub fn init(setting: LogSetting);
   }
   ```
   这同时补上 SDK 的一个真实缺口：**SDK 依赖 `tracing` 但从不提供订阅方式**，
   嵌入方想看 SDK 内部 trace 只能自己写一个 subscriber。

2. `MessageVisitor` 补齐全部 `record_*` 方法。格式化规则：
   `record_str` 直接写裸值（不加引号），`record_debug` 保留 `{:?}`，
   数值不加引号，`record_error` 写 `err = <显示形式>`。

3. `crates/cloudpipe/src/logging.rs` 改为 re-export SDK 的实现，删除本地副本；
   `crates/cloudpipe-node/src/lib.rs` 同样改为调用 `cloudpipe_sdk::logging::init`。

**不引入 `tracing-subscriber`**：`logging.rs:9-13` 说明为单二进制体积刻意不用，
本 spec 尊重该约束。

### 6.2 D5：`LogLevel` 死承诺

`classify_line`（`binary.rs:115-146`）只产出 `Connection` / `Error` / `Ignore` 三类，
SDK 只发 `LogLevel::Error`（`session/bootstrap.rs:224-229`），
但 `Event::CloudflaredLog { level }`（`event.rs:97-102`）与
`index.d.ts:142` 都声明了 `info` / `warn` / `error` 三级。

**变更**：`classify_line` 增加对 cloudflared 自身日志级别的识别。
cloudflared 的 stderr 行形如

```
2025-11-01T12:00:00Z INF Starting tunnel
2025-11-01T12:00:00Z WRN Retrying connection
2025-11-01T12:00:00Z ERR connection terminated
```

解析规则：行首（允许前导空白）匹配 `<ISO8601>Z ` 之后的大写级别 token
（`INF` / `WRN` / `ERR` / `DBG` / `TRC`）→ 映射为 `LogLevel::Info` / `Warn` / `Error`。
`DBG`/`TRC` 归为 `Info`（`LogLevel` 只有三级）。识别不出级别 token 的行，
沿用现有的"含 error/err → Error，否则 Ignore"规则。

`LineKind` 增加 `Log(LogLevel)` 变体；`bootstrap.rs` 的 stderr pump 按级别发
`Event::CloudflaredLog { level }`。`Ignore` 仍表示噪声，不产生事件。

**行为影响**：cloudflared 的 warning 现在会浮到 UI。这是新增信息，不是噪声回归。

### 6.3 D6：协议大小写

`cli.rs:123` 用字面量匹配 `"http" | "https" | ...`，`cfp HTTPS 8443` 报
`unexpected argument "HTTPS"`。而 SDK 的 `Protocol::parse`（`protocol.rs:29`）
与 Node 的 `parse_protocol`（`node-core/src/options.rs`）都大小写不敏感。

**变更**：`cli.rs` 的第一个位置参数改为先尝试 `Protocol::parse`，失败再当作端口解析；
错误信息用 `Protocol::parse` 的 `InvalidProtocol` 文案（`protocol.rs:72`），
列出受支持的协议。

### 6.4 D7：密钥上 argv

`cli.rs:73-77` 的 `Command::Key { token: Option<String> }` 让 token 出现在
shell history 与 `/proc/<pid>/cmdline` 中 —— 与 SDK 对 tunnel token 特意用 0600 文件的
谨慎姿态（`binary.rs` 模块文档）自相矛盾。

**变更**：

1. `cfp key -` 从 stdin 读取 token（读一行，去空白）。
2. 新增 `cfp key --token-file <path>` 从文件读取。
3. 仍以位置参数传入 token 时，向 stderr 打印一行提示：
   `warning: the token is visible in \`ps\` output — prefer \`cfp key -\` or --token-file`。
   位置参数保留（向后兼容），但 README 的快速开始改用 `cfp key -`。
4. 读入的 token 一律 `trim()`（`main.rs:183` 已有），空值仍然报错。

### 6.5 D8：先干活后失败

`main.rs:209-212`：先 `config.apply_zone(&zones[0])?`，再 `config.token = ...` 与保存。
若 `zones[0]` 缺 account id（`config.rs:131-140`），函数在此返回错误，
**token 没有落盘** —— 用户重跑 `cfp key` 会撞上完全一样的错误。

**变更**：调整顺序 —— 先写入 token 并保存，再解析 zone。
zone 解析失败时：

- token 已经保存（不丢凭证）；
- 向用户报错，提示"token 已保存，但 zone 解析失败：…；可用 `cfp domain <name>` 重新选择"；
- 进程仍以非零码退出（这是一个真实的失败），但配置状态是"已保存 token、zone 待定"。

多 zone 时保持现有行为（存第一个 + 提示 `cfp domain`），但同样在 token 保存之后进行。

### 6.6 D9：`key_status` 路径与 token 来源

`ui.rs:487-498`：无条件打印 `~/.cfp/config.json`（`CFP_CONFIG_DIR` 生效时是错的），
且 `effective_token()` 被 mask 掉 —— 一个只存在于环境变量的 token 显示得
与已持久化的一模一样。

**变更**：

1. 用 `ConfigStore::file_path()`（`config.rs:175`，目前是 `#[allow(dead_code)]`）打印真实路径。
2. `ui::key_status` 区分三种状态并分别标注来源：
   - 已保存到文件；
   - 仅来自 `CLOUDFLARE_API_TOKEN` 环境变量（未持久化）；
   - 未配置。
   三者的提示文案不同（"未配置"要给出 `cfp key -` 的指引）。

### 6.7 D10：魔法数字与字符串 round-trip

- `ui.rs:293-299` 的 `EdgeConnected` 只在 `total == 1 || total == 4` 时打印。
  `total` 是"至今建立的连接数"，cloudflared 通常建 4 条 QUIC 连接，于是 2、3 不打印。
  **变更**：每条连接都打印一行（`conn_index` 是 0-based 索引），不再依赖总数。
  UI 输出格式同步调整（连接行数从 1~2 行变成 4 行，属预期变化）。
- `main.rs:140-142` 把协议转成字符串传给 `ui::tunnel_live`，后者（`ui.rs:380`）
  用 `Protocol::parse(...).unwrap_or(Protocol::Http)` 反解 —— 解析失败会静默显示成 http。
  **变更**：`ui::tunnel_live` 与 `ui::render_live_block` 直接接受 `Protocol`，
  删掉字符串中转。

### 6.8 验收

- CLI 新增单测：协议大小写、`cfp key -` 的 stdin 路径、`key_status` 的三种来源、
  `render_event` 对每个 `Event` 变体的输出。

## 7. 变更组 5 — 测试补齐（D14）

### 7.1 CLI

`crates/cloudpipe/src/main.rs` 目前零测试。

**变更**：

1. 把 `handle_key` / `handle_domain` / `handle_domains` / `handle_cleanup` 的核心逻辑
   抽成接收 `&CloudflareApi`（而不是从 `Ctx` 内部取）的纯函数，
   `Ctx::api()` 继续负责生产路径的懒加载。
2. **新建 `crates/cloudpipe-test-support` crate**，承载进程内 mock Cloudflare API
   与 fake `cloudflared` 脚本生成器。它只作为 **dev-dependency** 被
   `cloudpipe-sdk` 与 `cfp` 使用，不进入任何发布产物，也不改变 SDK 的公共 API。

   - 从 `crates/cloudpipe-sdk/tests/integration.rs` 中抽出 mock 服务器
     （`Req` 解析、`MockState`、`route`、`spawn_mock_api`）与 fake `cloudflared`
     辅助函数（`write_fake_cloudflared`、`write_crashing_cloudflared`、`write_script`）；
   - `integration.rs` 改为 `use cloudpipe_test_support::…`，保持现有全部用例的行为不变；
   - `cfp` 的测试同样依赖它。

   选这个方案而不是"在 CLI 里复制一份 mock"，是因为复制会让两份 mock 各自漂移 ——
   而 mock 的行为正确性直接决定 CLI 测试的结论可信度。

3. `Ctx` 增加一个仅测试用的 base URL 覆盖，使 CLI 测试能打到上述 mock API
   （SDK 侧的 `CloudflareApi::with_base`，`client.rs:266`，本就为此存在）。
4. 覆盖以下用例：
   - `handle_key` 在多 zone 时选第一个并提示；
   - `handle_key` 在 zone 缺 account id 时**token 已保存**（D8 的回归测试）；
   - `handle_domain` 对不可访问 zone 的报错文案；
   - `handle_cleanup` 的 dry-run 与真删两条路径。

### 7.2 UI

`ui.rs` 的 `render_event`（整个 Event → 文本映射）零覆盖。

**变更**：表驱动测试，每个 `Event` 变体一条断言，断言关键子串
（如 `DnsCreated` 变体必须包含完整域名）。`ui.rs` 已有 `set_style` 机制
（`ui.rs:176-217`）用于测试时固定样式，测试沿用它。

### 7.3 Node

`crates/cloudpipe-node/test/smoke.js` 只覆盖本地拒绝路径；
`test/parse-error-code.js`（16 个用例）**从未被 CI 执行**（`package.json:48` 的
`"test": "node test/smoke.js"`）。

**变更**：

1. `ConnectOptions` 增加 `apiBase?: string`（**仅用于测试**，指向 mock API 的 base URL）。
   SDK 的 `CloudflareApi::with_base`（`client.rs:266`）本就为此存在。
2. `test/smoke.js` 增加端到端用例：连 mock → 收到启动期事件（验证 D2 的修复）
   → 载荷可解析 → `off(event, cb)` 后不再收到 → `close()` → 资源被回收。
3. `"test": "node test/smoke.js && node test/parse-error-code.js"`，
   让 `parse-error-code.js` 真正进入 CI。
4. `examples/consumer/index.js` 的断言同步更新（它跑在**已发布的 npm tarball** 上，
   是发布前的最后一道闸）。

## 8. 不在本 spec 范围内

| 项 | 理由 |
|---|---|
| 多路由 / 多隧道（B1） | 新特性，改 ingress 的 API 形状，单独 spec |
| Cloudflare Access 鉴权（B2） | 新 API 依赖 + 额外 token 权限，单独 spec |
| `cfp list` / `status` / `logs`（B3） | 新子命令，单独 spec |
| `config_src=local`（B5） | 新工作流模式，单独 spec |
| crates.io / Python SDK / Docker / GH Action（B7） | 分发工程，与本 spec 无耦合，单独 spec |
| npm musl / linux-arm64 target | `npm.yml` 已约定推到 v0.2.0 |

## 9. 风险与前置条件

1. **需要联网**：补摘要要下载约 200 MB cloudflared 资产；GFW 环境需 `CFP_GITHUB_PROXY`。
2. **Node 行为变更**：`off(event, cb)` 与结构化 `error.code` / `error.kind` 是新增能力，
   不是破坏性变更（旧的 `off(event)` 与 `[CODE] message` 格式都保留）。
3. **新增公共 API 面**：`cloudpipe_sdk::logging` 是 SDK 的新模块，
   CI 的 `cargo semver-checks` 需要更新 baseline（新增 API 是向后兼容的，
   semver-checks 默认不报错，但 baseline 记录需要同步）。
4. **`cloudpipe-sdk` 的 `Cargo.toml` 不需要新依赖** —— 全部改动用现有依赖
   （`tracing`、`serde`、`tokio`）即可完成。

## 10. 完成定义

- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace --all-targets`、`cargo test --workspace --doc` 全绿。
- `cargo deny check` 通过（无新增依赖，依赖图不变）。
- `npm test`（含新的 e2e 与 `parse-error-code.js`）通过。
- `npm run build:debug` 后 `node examples/quickstart.js` 与 `node examples/with-events.js` 通过。
- README / npm README / `index.d.ts` 中每一条被承诺的行为，都有对应的测试或代码位置支撑。
- CI 的 `verify-checksums` job 在表与官方 release 一致时通过。
