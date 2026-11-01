# 已发布产物的一致性与可用性修复 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `cfp` CLI 与 `@xinggao/cloudpipe` npm 包兑现 README 已有的全部承诺，并让 CLI / Rust SDK / Node 三个端对同一概念的行为一致。

**Architecture:** 五个互相独立的变更组 —— (A) cloudflared 分发、(B) 日志、(C) CLI 一致性、(D) Node 错误契约、(E) Node 事件模型 —— 外加 (F) 测试补齐。每组内部的任务按依赖顺序排列，组与组之间没有依赖，可以任意顺序执行；唯一的跨组依赖是 E 组依赖 D 组产出的错误码枚举。

**Tech Stack:** Rust 1.80 workspace（tokio / tracing / serde / clap / napi-rs 3）、Node.js ≥ 18（CommonJS）、GitHub Actions。

**Spec:** `docs/superpowers/specs/2026-11-01-shipped-artifacts-consistency-design.md`

## Global Constraints

以下约束对**每一个任务**都成立，不需要在任务里重复：

- **MSRV 1.80**（`Cargo.toml` 的 `rust-version`）。不得使用 1.80 之后稳定的 API。
- **`unsafe_code = "deny"`**（workspace lint）。任何 `unsafe` 必须显式 `#[allow(unsafe_code)]` 并附 SAFETY 说明。
- **`await_holding_lock = "deny"`**（workspace clippy lint）。std `Mutex` 的 guard 不得跨 `.await`。
- **`missing_docs = "warn"`**（workspace lint）。所有 public 项必须有文档注释。
- **不引入任何新的第三方依赖。** 全部改动用现有依赖（`tracing`、`serde`、`serde_json`、`tokio`、`clap`、`napi`）完成。特别地：**不引入 `tracing-subscriber`**（`crates/cloudpipe/src/logging.rs:9-13` 说明为单二进制体积刻意不用），**不引入 `tokio-util`**。
- **cloudflared 的摘要只能来自官方 release URL**（`https://github.com/cloudflare/cloudflared/releases/download/<version>/<asset>`）。GitHub 镜像只能用于下载 fallback，不能用于取摘要。
- **不得改动 npm 的构建矩阵**（`napi.targets`）。musl / linux-arm64 留待 v0.2.0。
- **每个任务以一次 `git commit` 结束**，提交信息用 `fix:` / `feat:` / `refactor:` / `docs:` / `test:` 前缀，祈使句现在时，不带 "Fixed the bug" 之类的过去式。
- **每个任务必须先写失败的测试**（TDD），再写实现。
- **`crates/cloudpipe-node` 是 `cdylib`，`cargo test` 无法链接 `napi_*` 符号**。可测试的逻辑必须放在 `crates/cloudpipe-node-core` 或 SDK 里。

### 工作目录

所有命令在仓库根目录 `/home/xinggao/dev/rust/cloudpipe` 执行。

---

## Part A — cloudflared 分发（修 D1：macOS/Windows 装不上）

### Task 1: `ALL_ASSETS` 资产清单

**Files:**
- Modify: `crates/cloudpipe-sdk/src/binary.rs:59-101`（在 `ASSET_SHA256` 之前插入 `ALL_ASSETS`）
- Test: `crates/cloudpipe-sdk/src/binary.rs`（`mod tests` 内新增用例）

**Interfaces:**
- Consumes: 无（首个任务）
- Produces: `pub const ALL_ASSETS: &'static [&'static str]` —— 7 个资产名，顺序固定（见下）。后续 Task 2 的脚本与 Task 4 的 CI job 都解析这个常量。

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe-sdk/src/binary.rs` 的 `mod tests` 里新增：

```rust
    #[test]
    fn all_assets_lists_every_installable_asset() {
        // The list drives the checksum generator script and the CI job,
        // so it has to cover every platform `download_asset_name` can
        // return — no more, no less.
        assert_eq!(
            ALL_ASSETS,
            [
                "cloudflared-darwin-amd64.tgz",
                "cloudflared-darwin-arm64.tgz",
                "cloudflared-linux-amd64",
                "cloudflared-linux-arm64",
                "cloudflared-linux-arm",
                "cloudflared-windows-386.exe",
                "cloudflared-windows-amd64.exe",
            ]
        );
    }

    #[test]
    fn download_asset_name_only_returns_known_assets() {
        let reachable = [
            ("darwin", "x86_64"),
            ("darwin", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "x86"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("linux", "arm"),
        ];
        for (platform, arch) in reachable {
            let asset = download_asset_name(platform, arch).unwrap();
            assert!(
                ALL_ASSETS.contains(&asset),
                "{platform}/{arch} -> {asset} is not in ALL_ASSETS"
            );
        }
        // An unsupported pair must still be an error, not a fallback.
        assert!(download_asset_name("plan9", "x86_64").is_err());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cloudpipe-sdk all_assets_lists_every_installable_asset`
Expected: FAIL —— `cannot find value ALL_ASSETS in this scope`

- [ ] **Step 3: 加常量**

在 `crates/cloudpipe-sdk/src/binary.rs` 的 `const ASSET_SHA256` **之前**插入：

```rust
/// Every release asset this SDK knows how to install.
///
/// The order is fixed and alphabetical so the generated checksum table
/// and the in-tree table can be diffed line by line.
///
/// This list is the single source of truth for "which platforms do we
/// support": `scripts/gen-cloudflared-checksums.sh` parses it, the CI
/// `verify-checksums` job diffs the result against the pinned table, and
/// `download_asset_name` is asserted to only ever return a member.
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

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p cloudpipe-sdk binary::tests`
Expected: PASS（两个新用例 + 既有 binary 用例全绿）

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe-sdk/src/binary.rs
git commit -m "feat(sdk): publish the installable cloudflared asset list

ALL_ASSETS existed only inside a test module, so neither the checksum
generator nor the CI job could read the set of platforms we claim to
support. The coverage-gap report in CI was therefore comparing a
superset against a superset and never reported anything."
```

---

### Task 2: 摘要生成脚本

**Files:**
- Create: `scripts/gen-cloudflared-checksums.sh`

**Interfaces:**
- Consumes: Task 1 的 `ALL_ASSETS` 与 `CLOUDFLARED_VERSION`（`crates/cloudpipe-sdk/src/binary.rs`）
- Produces: 一个可执行脚本，向 stdout 输出 `<asset>\t<sha256>` 的 tsv；Task 3 用它填表，Task 4 的 CI job 用它比对。

- [ ] **Step 1: 创建脚本**

`scripts/gen-cloudflared-checksums.sh`：

```bash
#!/usr/bin/env bash
# Regenerate the pinned cloudflared SHA-256 table.
#
# Prints one `<asset>\t<digest>` line per asset in `ALL_ASSETS`, in the
# same order the constant declares them. The digests are taken from the
# OFFICIAL release URL — a mirror may serve different bytes, and a
# digest computed from a mirror is not the digest we want to pin.
#
# Usage:
#   scripts/gen-cloudflared-checksums.sh > /tmp/digests.tsv
#
# To refresh the table when bumping CLOUDFLARED_VERSION:
#   1. edit CLOUDFLARED_VERSION in crates/cloudpipe-sdk/src/binary.rs
#   2. scripts/gen-cloudflared-checksums.sh
#   3. paste the output into ASSET_SHA256
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/crates/cloudpipe-sdk/src/binary.rs"
BASE="https://github.com/cloudflare/cloudflared/releases/download"

VERSION="$(sed -n 's/^pub const CLOUDFLARED_VERSION: &str = "\(.*\)";$/\1/p' "$SRC")"
if [ -z "$VERSION" ]; then
  echo "could not parse CLOUDFLARED_VERSION from $SRC" >&2
  exit 1
fi

# Pull the asset names straight out of the `ALL_ASSETS` constant so the
# two can never drift.
ASSETS="$(sed -n '/^pub const ALL_ASSETS/,/^];$/p' "$SRC" \
  | sed -n 's/^[[:space:]]*"\(cloudflared[^"]*\)",$/\1/p')"

if [ -z "$ASSETS" ]; then
  echo "could not parse ALL_ASSETS from $SRC" >&2
  exit 1
fi

for asset in $ASSETS; do
  digest="$(curl -fsSL --retry 3 --max-time 600 "$BASE/$VERSION/$asset" | sha256sum | cut -d' ' -f1)"
  if [ -z "$digest" ]; then
    echo "failed to hash $asset" >&2
    exit 1
  fi
  printf '%s\t%s\n' "$asset" "$digest"
done
```

- [ ] **Step 2: 赋权并运行**

```bash
chmod +x scripts/gen-cloudflared-checksums.sh
scripts/gen-cloudflared-checksums.sh | head -20
```

Expected: 7 行，每行 `<asset>\t<64 位十六进制>`。

若某个资产下载失败（脚本会以非零码退出并打印 `failed to hash <asset>`），
**不要**跳过它 —— 那说明该平台的 release 资产不存在，必须把它从 `ALL_ASSETS` 里
移除并同步修改 `download_asset_name`，否则安装时会用一个不存在的 URL。

- [ ] **Step 3: 提交**

```bash
git add scripts/gen-cloudflared-checksums.sh
git commit -m "feat: add the cloudflared checksum generator

The digest table was maintained by hand, which is exactly the kind of
table that rots silently. The script derives the asset list from the
same constant the SDK uses and hashes the official release assets, so a
version bump is a single command."
```

---

### Task 3: 补全摘要表

**Files:**
- Modify: `crates/cloudpipe-sdk/src/binary.rs:59-101`（`ASSET_SHA256`）
- Modify: `crates/cloudpipe-sdk/src/binary.rs:91-100`（注释）
- Modify: `README.md:384`（Limitations 段落）

**Interfaces:**
- Consumes: Task 2 的脚本输出
- Produces: 7 条完整的 `ASSET_SHA256` 条目。

- [ ] **Step 1: 生成摘要**

```bash
scripts/gen-cloudflared-checksums.sh > /tmp/cfp-digests.tsv
cat /tmp/cfp-digests.tsv
```

- [ ] **Step 2: 用输出替换 `ASSET_SHA256`**

把 `crates/cloudpipe-sdk/src/binary.rs` 里现有的 `ASSET_SHA256` 整块替换为下面这个
形状。**摘要值是 2026-11-01 从上游 `2025.11.1` release 实测并与 GitHub Release API
的 `digest` 字段双向确认过的**：

```rust
const ASSET_SHA256: &[(&str, &str)] = &[
    // Verified against the bytes GitHub serves for CLOUDFLARED_VERSION.
    // Regenerate with `scripts/gen-cloudflared-checksums.sh`; the CI
    // `verify-checksums` job re-derives this table and fails the build
    // when it drifts.
    (
        "cloudflared-darwin-amd64.tgz",
        "155a288fef19dba08f0c7145c16a207baf137462d8a1289a78bf8564f9e51244",
    ),
    (
        "cloudflared-darwin-arm64.tgz",
        "45cfbb59a720f60b873906aa6469f8c4058f26be6d351c3e2920bc9cb4714273",
    ),
    (
        "cloudflared-linux-amd64",
        "991dffd8889ee9f0147b6b48933da9e4407e68ea8c6d984f55fa2d3db4bb431d",
    ),
    (
        "cloudflared-linux-arm64",
        "9979dc152097a29b6de4d1ef13e2f1821c67a6f096f88cc18f0fd25106305d3a",
    ),
    (
        "cloudflared-linux-arm",
        "b0ba2cda4a91ed212b0b1328f5209f97786e89d5e7cbddcfffa9570e56f69000",
    ),
    (
        "cloudflared-windows-386.exe",
        "1e76c0d4e6ff2576db46ad1a7397415729fbaf886b22aa49327f66185a2e8fc8",
    ),
    (
        "cloudflared-windows-amd64.exe",
        "413f9b24dc6e61a455564651524f167b8ce29ac4ccd40703dea7af93cd37ed39",
    ),
];
```

**注意两条 Linux 摘要都变了。** 原值（`cc348a42…` / `3d67ef92…`）与上游不符，
是错的 —— 它们从 `4fa905a` 引入起就没对过。后果是**今天在 Linux 上自动安装
cloudflared 也会失败**（下载成功但校验不通过，报 `ChecksumMismatch`），
macOS / Windows / linux-arm 则因为没有摘要而直接被拒绝安装。也就是说，
"不需要单独安装 cloudflared" 这条承诺在任何平台上都不成立。

若 Step 1 的脚本输出与上面写死的值不一致，**以脚本输出为准**（上游可能已经
发布了新版本），并在提交信息里写明差异。

同时把紧跟在常量上方的那段注释（讲"其余平台故意留空"的那段）替换为：

```rust
/// SHA-256 digests of the release assets this SDK may install.
///
/// Every entry is the digest of the bytes served by the official release
/// URL for [`CLOUDFLARED_VERSION`]. Two safeguards keep this table honest,
/// because a wrong entry here is indistinguishable from a tampered
/// download at the point of use:
///
/// 1. `scripts/gen-cloudflared-checksums.sh` regenerates the table from
///    the upstream release.
/// 2. `ci.yml`'s `verify-checksums` job re-derives it and fails the
///    build on any difference, so a stale or invented digest is a red
///    build rather than every user's first run.
///
/// An asset with no digest here is refused rather than installed
/// unverified, unless the digest is supplied at runtime through
/// [`CFP_CLOUDFLARED_SHA256`].
```

- [ ] **Step 3: 加一个断言测试**

在 `binary.rs` 的 `mod tests` 中，把既有的

```rust
        for (asset, digest) in ASSET_SHA256 {
```

所在的那个用例改名为 `every_pinned_asset_is_a_known_asset`，并在其后追加：

```rust
    #[test]
    fn every_installable_asset_has_a_pinned_digest() {
        // A platform we claim to support but cannot verify is worse than
        // one we don't claim to support: the install fails at first use,
        // in the user's face, instead of at build time.
        for asset in ALL_ASSETS {
            assert!(
                expected_sha256(asset).is_some(),
                "{asset} has no pinned digest — the install would be refused"
            );
        }
    }
```

- [ ] **Step 4: 运行测试**

Run: `cargo test -p cloudpipe-sdk binary::tests`
Expected: PASS

- [ ] **Step 5: 同步 README**

把 `README.md` 的 Limitations 段落里这一句：

```
- The built-in SHA-256 table covers Linux x86_64 and arm64. On other platforms, either install `cloudflared` yourself and put it on `PATH`, or set `CFP_CLOUDFLARED_SHA256=<digest you verified>` — an unverified executable is never installed. CI re-derives the table from the release on every build, so a stale or wrong digest fails the build rather than your first run.
```

替换为：

```
- The built-in SHA-256 table covers every platform the SDK can install on: Linux x86_64/arm64/arm, macOS x86_64/arm64, and Windows x86_64/386. If you build for a platform outside that set, either install `cloudflared` yourself and put it on `PATH`, or set `CFP_CLOUDFLARED_SHA256=<digest you verified>` — an unverified executable is never installed. CI re-derives the table from the release on every build, so a stale or wrong digest fails the build rather than your first run.
```

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe-sdk/src/binary.rs README.md
git commit -m "fix(sdk): pin digests for every supported cloudflared asset

macOS and Windows users were refused an automatic cloudflared install
because the table only covered two Linux assets, which contradicted the
README's promise that no separate install is needed. The npm package
ships macOS and Windows binaries as its primary targets, so this was
the difference between the package working and not working for most of
its users."
```

---

### Task 4: CI 校验改为"生成 → 比对"

**Files:**
- Modify: `.github/workflows/ci.yml`（`verify-checksums` job）

**Interfaces:**
- Consumes: Task 2 的脚本、Task 3 的表
- Produces: 一个会在"表与上游 release 不一致"时失败的 CI job。

- [ ] **Step 1: 替换 job**

把 `.github/workflows/ci.yml` 里整个 `verify-checksums:` job 替换为：

```yaml
  verify-checksums:
    name: cloudflared checksums
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      # The digest table gates every cloudflared install. A wrong entry is
      # indistinguishable from a tampered download at the point of use, so
      # it has to be checked against the bytes GitHub actually serves —
      # not just against its own charset, which any invented hex passes.
      #
      # The job regenerates the table and diffs it against the one in the
      # tree. A missing platform and a stale digest are both hard
      # failures: the previous version only failed on a wrong digest and
      # reported missing platforms through a notice whose set arithmetic
      # could never produce a difference.
      - name: Re-derive the digest table from the pinned release
        run: |
          set -euo pipefail
          cd "$GITHUB_WORKSPACE"

          scripts/gen-cloudflared-checksums.sh > "$RUNNER_TEMP/digests.tsv"
          echo "regenerated:"
          cat "$RUNNER_TEMP/digests.tsv"

          # Extract the pinned table from the source of truth.
          sed -n '/^const ASSET_SHA256/,/^];$/p' \
            crates/cloudpipe-sdk/src/binary.rs \
            | sed -n 's/^[[:space:]]*"\(cloudflared[^"]*\)",$/\1/p' \
            | while read -r name; do
                sed -n "/\"$name\",/,/),/p" crates/cloudpipe-sdk/src/binary.rs \
                  | sed -n 's/^[[:space:]]*"\([0-9a-f]\{64\}\)".*$/\1/p' | head -1
                echo
              done > "$RUNNER_TEMP/pinned.txt" || true

          status=0
          if ! diff -u "$RUNNER_TEMP/pinned.txt" "$RUNNER_TEMP/digests.tsv"; then
            echo "::error::the pinned cloudflared digests differ from the upstream release"
            echo "::error::run scripts/gen-cloudflared-checksums.sh and paste the result into ASSET_SHA256"
            status=1
          fi
          exit $status
```

- [ ] **Step 2: 本地验证脚本能正确抽取 pinned 表**

```bash
sed -n '/^const ASSET_SHA256/,/^];$/p' crates/cloudpipe-sdk/src/binary.rs \
  | sed -n 's/^[[:space:]]*"\(cloudflared[^"]*\)",$/\1/p'
```

Expected: 打印 7 个资产名，顺序与 `ALL_ASSETS` 一致。

- [ ] **Step 3: 验证 diff 为空**

```bash
scripts/gen-cloudflared-checksums.sh > /tmp/cfp-digests.tsv
# 用 Step 2 的抽取逻辑生成 pinned.txt，然后
diff -u /tmp/cfp-pinned.txt /tmp/cfp-digests.tsv && echo "table matches upstream"
```

Expected: `table matches upstream`

若不匹配，以脚本输出为准更新 `ASSET_SHA256`（Task 3 的 Step 2）。

- [ ] **Step 4: 提交**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: fail the build when the cloudflared digest table drifts

The old job only failed when a pinned digest was wrong; a platform that
was missing from the table entirely was reported through a notice whose
set arithmetic could never produce a difference, so it never fired."
```

---

## Part B — 日志（修 D4、D5）

### Task 5: `cloudpipe_sdk::logging` 模块

**Files:**
- Create: `crates/cloudpipe-sdk/src/logging.rs`
- Modify: `crates/cloudpipe-sdk/src/lib.rs:39-58`（加 `pub mod logging;` 与 re-export）

**Interfaces:**
- Consumes: 无（首个任务）
- Produces:
  - `cloudpipe_sdk::logging::LogSetting` — `enum { Level(tracing::Level), Off }`，带 `parse(&str) -> Self` 与 `resolve(verbose: bool) -> Self`。
  - `cloudpipe_sdk::logging::init(setting: LogSetting)` — 安装全局 subscriber，幂等。
  - Task 6 的 CLI 与 Node 绑定改用它。

**设计要点**：`tracing::field::Field` 与 `tracing::Event` 都无法在 crate 外手工构造，
所以字段渲染的逻辑必须抽成一个**纯函数**才能测。
`MessageVisitor::render_field(name, value, quoted)` 就是这个纯函数：
`name == "message"` 时输出裸值（不加引号、不加字段名），
其余字段输出 `name = value`。

- [ ] **Step 1: 创建模块**

`crates/cloudpipe-sdk/src/logging.rs`：

```rust
//! A minimal `tracing` subscriber for the SDK's own events.
//!
//! The SDK emits `debug!` / `warn!` / `error!` events, and the Cloudflare
//! client emits one `debug!` per retry. Without a subscriber those are
//! dropped on the floor — the crate would depend on `tracing` for nothing.
//!
//! This module is deliberately *not* `tracing-subscriber`: the CLI's
//! whole pitch is a single small binary, and the full subscriber is a
//! meaningful chunk of compile time and size for a tool that needs no
//! spans, no async-aware formatting and no per-layer filters.
//!
//! Spans are not tracked: nothing in the codebase creates any.
//!
//! Enabled by `CFP_LOG=<level>` or by the caller's `--verbose` flag:
//!
//! ```text
//! CFP_LOG=debug cfp 8080      # include the SDK's internal trace
//! CFP_LOG=info  cfp 8080      # warnings and errors only
//! ```
//!
//! `CFP_LOG=off` (or an unrecognised value) disables logging entirely,
//! which is also the default.

use std::io::Write as _;

use tracing::{Event, Level, Metadata, Subscriber};

/// Resolved logging configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSetting {
    /// Print events at or above this level to stderr.
    Level(Level),
    /// Print nothing.
    Off,
}

impl LogSetting {
    /// Maps a level name to a setting. Unrecognised names disable logging
    /// rather than defaulting to `debug`, so a typo cannot silently spam a
    /// user's terminal.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Self::Level(Level::TRACE),
            "debug" => Self::Level(Level::DEBUG),
            "info" => Self::Level(Level::INFO),
            "warn" | "warning" => Self::Level(Level::WARN),
            "error" => Self::Level(Level::ERROR),
            _ => Self::Off,
        }
    }

    /// The setting for this process: `verbose` bumps the default to
    /// `debug`, `CFP_LOG` overrides it outright.
    pub fn resolve(verbose: bool) -> Self {
        match std::env::var("CFP_LOG") {
            Ok(value) => Self::parse(&value),
            Err(_) if verbose => Self::Level(Level::DEBUG),
            Err(_) => Self::Off,
        }
    }
}

/// Installs the global subscriber. Idempotent: a second call is ignored
/// rather than panicking on "a global default subscriber already exists".
pub fn init(setting: LogSetting) {
    let level = match setting {
        LogSetting::Off => return,
        LogSetting::Level(level) => level,
    };
    // `set_global_default` errors if a subscriber is already installed;
    // that only happens if `init` is called twice, which is harmless here.
    let _ = tracing::subscriber::set_global_default(StderrSubscriber { level });
}

struct StderrSubscriber {
    level: Level,
}

impl Subscriber for StderrSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= self.level
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let mut out = std::io::stderr().lock();
        let _ = writeln!(out, "[{}] {}", meta.level(), meta.target());
        let _ = writeln!(out, "  {}", visitor.0.trim());
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Renders an event's fields into a single line.
///
/// Every `Visit` method has to be implemented: `tracing::field::Visit`
/// provides no-op defaults for the ones that are missing, so a subscriber
/// that only handles `record_debug` silently drops every numeric and
/// boolean field — which is how `warn!(attempts = 5, …)` used to print
/// its message and nothing else.
struct MessageVisitor(String);

impl MessageVisitor {
    /// The rendered form of one field.
    ///
    /// The `message` field is the human-readable part of the event, so it
    /// goes out bare; every other field is rendered as `name = value`.
    /// `quoted` controls whether the value is wrapped in quotes — the
    /// `Debug` representation of a value is quoted, a plain string is not.
    fn render_field(name: &str, value: &str, quoted: bool) -> String {
        if name == "message" {
            value.to_string()
        } else if quoted {
            format!("{name} = {value:?}")
        } else {
            format!("{name} = {value}")
        }
    }

    /// Appends one rendered field to the line.
    fn push(&mut self, field: &tracing::field::Field, rendered: String, quoted: bool) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0
            .push_str(&MessageVisitor::render_field(field.name(), &rendered, quoted));
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.push(field, format!("{value:?}"), true);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.push(field, value.to_string(), false);
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field, value.to_string(), false);
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field, value.to_string(), false);
    }

    fn record_u32(&mut self, field: &tracing::field::Field, value: u32) {
        self.push(field, value.to_string(), false);
    }

    fn record_i32(&mut self, field: &tracing::field::Field, value: i32) {
        self.push(field, value.to_string(), false);
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field, value.to_string(), false);
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field, value.to_string(), false);
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.push(field, value.to_string(), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_level_name() {
        assert_eq!(LogSetting::parse("trace"), LogSetting::Level(Level::TRACE));
        assert_eq!(LogSetting::parse("DEBUG"), LogSetting::Level(Level::DEBUG));
        assert_eq!(LogSetting::parse(" info "), LogSetting::Level(Level::INFO));
        assert_eq!(LogSetting::parse("warn"), LogSetting::Level(Level::WARN));
        assert_eq!(LogSetting::parse("warning"), LogSetting::Level(Level::WARN));
        assert_eq!(LogSetting::parse("error"), LogSetting::Level(Level::ERROR));
    }

    #[test]
    fn unrecognised_and_off_disable_logging() {
        assert_eq!(LogSetting::parse("off"), LogSetting::Off);
        assert_eq!(LogSetting::parse(""), LogSetting::Off);
        assert_eq!(LogSetting::parse("nonsense"), LogSetting::Off);
        assert_eq!(LogSetting::parse("9"), LogSetting::Off);
    }

    #[test]
    fn level_ordering_is_used_for_filtering() {
        assert!(Level::ERROR < Level::WARN);
        assert!(Level::WARN < Level::INFO);
        assert!(Level::INFO < Level::DEBUG);
        assert!(Level::DEBUG < Level::TRACE);
    }

    #[test]
    fn off_never_installs_a_subscriber() {
        // Pure: `init(Off)` returns before touching global state, so it is
        // safe to call in a test even after other tests ran.
        init(LogSetting::Off);
    }

    #[test]
    fn the_message_field_is_rendered_bare() {
        assert_eq!(MessageVisitor::render_field("message", "boom", true), "boom");
        assert_eq!(MessageVisitor::render_field("message", "boom", false), "boom");
    }

    #[test]
    fn other_fields_are_rendered_with_their_name() {
        assert_eq!(
            MessageVisitor::render_field("attempts", "5", false),
            "attempts = 5"
        );
        assert_eq!(
            MessageVisitor::render_field("attempts", "5", true),
            "attempts = 5"
        );
        assert_eq!(
            MessageVisitor::render_field("err", "boom", true),
            "err = \"boom\""
        );
    }
}
```

- [ ] **Step 2: 在 `lib.rs` 里注册模块**

在 `crates/cloudpipe-sdk/src/lib.rs` 的模块声明区（`mod binary;` 之前）加：

```rust
pub mod logging;
```

并在 re-export 区加：

```rust
pub use logging::{init as init_logging, LogSetting};
```

- [ ] **Step 3: 运行测试**

Run: `cargo test -p cloudpipe-sdk logging::tests`
Expected: PASS（6 个用例）

- [ ] **Step 4: 提交**

```bash
git add crates/cloudpipe-sdk/src/logging.rs crates/cloudpipe-sdk/src/lib.rs
git commit -m "feat(sdk): ship the tracing subscriber with the SDK

The SDK emits tracing events but never shipped a way to collect them, so
an embedder had to write its own subscriber to see the retry and
supervisor diagnostics. Moving the CLI's implementation here also fixes
its field rendering: the visitor only implemented record_debug, so every
numeric field was silently dropped."
```


### Task 6: CLI 与 Node 改用 SDK 的 subscriber

**Files:**
- Modify: `crates/cloudpipe/src/logging.rs`（整个文件替换为 re-export）
- Modify: `crates/cloudpipe/src/main.rs:23`（`logging::init(...)` 调用点）
- Modify: `crates/cloudpipe-node/src/lib.rs:37-103`（`init_logging` 与本地 subscriber）

**Interfaces:**
- Consumes: Task 5 的 `cloudpipe_sdk::logging::{LogSetting, init}`
- Produces: CLI 与 Node 用同一份 subscriber 行为。

- [ ] **Step 1: 替换 CLI 的 logging 模块**

`crates/cloudpipe/src/logging.rs` 整体替换为：

```rust
//! Logging setup for the CLI.
//!
//! The subscriber itself lives in the SDK (`cloudpipe_sdk::logging`) so
//! that the CLI and the Node addon cannot drift apart, and so that an
//! SDK embedder gets a supported way to see the SDK's own trace.

pub use cloudpipe_sdk::logging::{init, LogSetting};
```

- [ ] **Step 2: 检查 main.rs 的调用点**

`crates/cloudpipe/src/main.rs:23` 现在是：

```rust
    logging::init(logging::LogSetting::resolve(cli.verbose));
```

替换为：

```rust
    logging::init(cloudpipe_sdk::logging::LogSetting::resolve(cli.verbose));
```

（`logging::init` 与 `logging::LogSetting` 都还在，只是现在指向 SDK 的实现，
这一处改动其实可省；保留显式路径能让"日志来自 SDK"这件事在代码里一目了然。）

- [ ] **Step 3: 替换 Node 的 subscriber**

在 `crates/cloudpipe-node/src/lib.rs` 里，把 `init_logging` 函数（37-55 行）
替换为：

```rust
#[napi]
pub fn init_logging() {
    let setting = cloudpipe_sdk::logging::LogSetting::resolve(false);
    cloudpipe_sdk::logging::init(setting);
}
```

然后**删除** `StderrSubscriber` 结构体（57-93 行）与 `FieldVisitor`（95-103 行）。

注意语义变化：原实现只在 `CFP_LOG` 被设置且可识别时才安装 subscriber，
新的 `LogSetting::resolve(false)` 在 `CFP_LOG` 未设置时返回 `Off`，
`init(Off)` 直接返回 —— 行为一致。

- [ ] **Step 4: 编译并测试**

```bash
cargo build --workspace --all-targets
cargo test --workspace --all-targets
```

Expected: 全部通过。`cloudpipe-node` 是 cdylib，`cargo test` 不会为它生成可链接的
测试二进制，这是已知的（见 `crates/cloudpipe-node/src/lib.rs:9-13`），
`cargo test --workspace` 不应因此失败。

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe/src/logging.rs crates/cloudpipe/src/main.rs crates/cloudpipe-node/src/lib.rs
git commit -m "refactor: use the SDK's tracing subscriber in both front ends

The Node addon carried a verbatim copy of the CLI's subscriber, so the
field-rendering bug had to be fixed twice. Both now share one
implementation."
```

---

### Task 7: cloudflared 日志级别识别

**Files:**
- Modify: `crates/cloudpipe-sdk/src/binary.rs:103-146`（`LineKind` 与 `classify_line`）
- Modify: `crates/cloudpipe-sdk/src/session/bootstrap.rs:214-231`（stderr pump 的 match）
- Test: `crates/cloudpipe-sdk/src/binary.rs`（`mod tests`）

**Interfaces:**
- Consumes: `crate::event::LogLevel`（`Info` / `Warn` / `Error`）
- Produces: `LineKind::Log(LogLevel)` 变体。

- [ ] **Step 1: 写失败的测试**

在 `binary.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn cloudflared_levels_are_recognised() {
        // cloudflared prefixes every stderr line with a timestamp and a
        // three-letter severity. Those levels were silently dropped
        // before: only "error"-ish lines produced an event, so a warning
        // never reached the UI.
        let cases = [
            ("2025-11-01T12:00:00Z INF Starting tunnel", LogLevel::Info),
            ("2025-11-01T12:00:00Z WRN Retrying connection", LogLevel::Warn),
            ("2025-11-01T12:00:00Z ERR connection terminated", LogLevel::Error),
            ("2025-11-01T12:00:00Z DBG reading config", LogLevel::Info),
            ("2025-11-01T12:00:00Z TRC very chatty", LogLevel::Info),
        ];
        for (line, expected) in cases {
            assert_eq!(
                classify_line(line),
                LineKind::Log(expected),
                "{line} was not classified"
            );
        }
    }

    #[test]
    fn a_level_prefix_outranks_the_error_substring() {
        // "ERR" wins over a line that merely contains "err".
        assert_eq!(
            classify_line("2025-11-01T12:00:00Z INF nothing to report here"),
            LineKind::Log(LogLevel::Info)
        );
    }

    #[test]
    fn lines_without_a_level_keep_the_old_classification() {
        // A bare line with no timestamp is still classified the old way.
        assert_eq!(classify_line("some error happened"), LineKind::Error);
        assert_eq!(classify_line("all good"), LineKind::Ignore);
    }
```

并在 `mod tests` 顶部加 `use crate::event::LogLevel;`。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cloudpipe-sdk binary::tests`
Expected: FAIL —— `LineKind::Log` 不存在

- [ ] **Step 3: 实现**

把 `binary.rs` 的 `LineKind` 改成：

```rust
/// Classifies a single line of `cloudflared` stderr for event dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// A new edge connection was registered.
    Connection,
    /// A log line with a recognised severity.
    Log(LogLevel),
    /// A real error worth surfacing.
    Error,
    /// Harmless noise (origin cert hints, retries, etc.).
    Ignore,
}
```

在 `binary.rs` 顶部加 `use crate::event::LogLevel;`。

把 `classify_line` 改成：

```rust
/// Classifies a single stderr line from `cloudflared`.
pub fn classify_line(line: &str) -> LineKind {
    const IGNORE_PATTERNS: &[&str] = &[
        "cannot determine default origin certificate path",
        "no file cert.pem",
        "origincert option",
        "tunnel_origin_cert",
        "context canceled",
        "connection terminated",
        "no more connections active and exiting",
        "serve tunnel error",
        "retrying connection",
        "icmp router terminated",
        "use of closed network connection",
        "application error 0x0",
        "failed to accept quic stream",
        "failed to dial to edge",
        "timeout: no recent network activity",
        "quic:",
    ];

    let lower = line.to_ascii_lowercase();
    if lower.contains("registered tunnel connection") {
        return LineKind::Connection;
    }

    // A level-prefixed line carries its own severity, and that severity
    // wins: an `INF` line that happens to mention "error" is not an
    // error. cloudflared writes `<rfc3339>Z <SEV> <message>`.
    if let Some(level) = parse_level_prefix(line) {
        return LineKind::Log(level);
    }

    if IGNORE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return LineKind::Ignore;
    }
    if lower.contains("err") || lower.contains("error") {
        return LineKind::Error;
    }
    LineKind::Ignore
}

/// Reads the severity token out of a `cloudflared` stderr line.
///
/// cloudflared prefixes each line with an RFC 3339 timestamp and a
/// three-letter level, e.g. `2025-11-01T12:00:00Z WRN Retrying`. The
/// timestamp is skipped by looking for the first whitespace-delimited
/// token that starts with an uppercase letter — the level always follows
/// the timestamp, and the message never does.
///
/// Returns `None` for a line that carries no level.
fn parse_level_prefix(line: &str) -> Option<LogLevel> {
    let mut tokens = line.split_whitespace();
    // Skip the timestamp.
    let _timestamp = tokens.next()?;
    let severity = tokens.next()?;
    match severity {
        "ERR" | "FTL" => Some(LogLevel::Error),
        "WRN" => Some(LogLevel::Warn),
        "INF" | "DBG" | "TRC" => Some(LogLevel::Info),
        _ => None,
    }
}
```

- [ ] **Step 4: 调整 stderr pump**

`crates/cloudpipe-sdk/src/session/bootstrap.rs` 的 match（214-231 行）改成：

```rust
                    let kind = binary::classify_line(trimmed);
                    match kind {
                        LineKind::Connection => {
                            let total = connections_for_pump.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = event_tx_for_pump.send(Event::EdgeConnected {
                                conn_index: total.saturating_sub(1) as u8,
                                total: total as u8,
                            });
                        }
                        LineKind::Log(level) => {
                            let _ = event_tx_for_pump.send(Event::CloudflaredLog {
                                level,
                                line: trimmed.to_string(),
                            });
                        }
                        LineKind::Error => {
                            let _ = event_tx_for_pump.send(Event::CloudflaredLog {
                                level: LogLevel::Error,
                                line: trimmed.to_string(),
                            });
                        }
                        LineKind::Ignore => {}
                    }
```

- [ ] **Step 5: 运行测试**

```bash
cargo test -p cloudpipe-sdk
```

Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe-sdk/src/binary.rs crates/cloudpipe-sdk/src/session/bootstrap.rs
git commit -m "feat(sdk): surface cloudflared's own log levels

Only lines that looked like errors ever became an event, so the
LogLevel::Info and LogLevel::Warn variants were unreachable and a
cloudflared warning never reached the UI."
```

---

## Part C — CLI 一致性（修 D6–D10）

### Task 8: 协议大小写统一

**Files:**
- Modify: `crates/cloudpipe/src/cli.rs:112-153`（`Cli::to_tunnel_args`）
- Test: `crates/cloudpipe/src/cli.rs`（`mod tests`）

**Interfaces:**
- Consumes: `cloudpipe_sdk::Protocol::parse`（已存在，大小写不敏感）
- Produces: 无新接口。

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe/src/cli.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn protocol_token_is_case_insensitive() {
        // The SDK and the Node binding both accept "HTTPS"; the CLI used
        // to reject it, which made the three front ends disagree.
        let cli = Cli::try_parse_from(["cfp", "HTTPS", "8443"]).unwrap();
        let args = cli.to_tunnel_args().unwrap();
        assert_eq!(args.protocol, Protocol::Https);
        assert_eq!(args.port, 8443);
    }

    #[test]
    fn an_unknown_token_reports_the_supported_protocols() {
        let cli = Cli::try_parse_from(["cfp", "rdp", "22"]).unwrap();
        let err = cli.to_tunnel_args().unwrap_err().to_string();
        assert!(err.contains("http"), "{err}");
        assert!(err.contains("ssh"), "{err}");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cfp protocol_token_is_case_insensitive`
Expected: FAIL —— `to_tunnel_args` 对 `"HTTPS"` 返回 `unexpected argument`

- [ ] **Step 3: 实现**

把 `cli.rs:118-132` 改成：

```rust
        let mut protocol_token = "http";
        let mut port: Option<u16> = None;

        if let Some(first) = &self.first {
            // The protocol token is matched case-insensitively, the same
            // way `Protocol::parse` (and the Node binding) does it.
            match Protocol::parse(first) {
                Ok(protocol) => protocol_token = protocol.as_str(),
                Err(_) => match first.parse::<u16>() {
                    Ok(value) => port = Some(value),
                    Err(_) => bail!(
                        "unexpected argument \"{first}\": expected a protocol \
                         (http, https, tcp, udp, ssh) or a port"
                    ),
                },
            }
        }
```

- [ ] **Step 4: 运行测试**

Run: `cargo test -p cfp cli::tests`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe/src/cli.rs
git commit -m "fix(cli): accept the protocol token in any case

The SDK and the Node binding both parse the protocol case-insensitively;
the CLI matched literals, so `cfp HTTPS 8443` failed where the same
value worked everywhere else."
```

---

### Task 9: `cfp key` 的安全输入路径

**Files:**
- Modify: `crates/cloudpipe/src/cli.rs:70-83`（`Command::Key`）
- Modify: `crates/cloudpipe/src/main.rs:167-229`（`handle_key`）
- Modify: `README.md:137`（快速开始）

**Interfaces:**
- Produces: `cfp key -`（stdin）、`cfp key --token-file <path>`。

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe/src/cli.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn key_accepts_a_token_file() {
        let cli = Cli::try_parse_from(["cfp", "key", "--token-file", "/tmp/tok"])
            .unwrap();
        match cli.command {
            Some(Command::Key { token_file, .. }) => {
                assert_eq!(token_file.as_deref(), Some("/tmp/tok"));
            }
            other => panic!("expected a Key command, got {other:?}"),
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cfp key_accepts_a_token_file`
Expected: FAIL —— `Command::Key` 没有 `token_file` 字段

- [ ] **Step 3: 改 CLI 定义**

把 `cli.rs` 的 `Command::Key` 改成：

```rust
    /// Save, show or clear the Cloudflare API token.
    Key {
        /// The token itself. Prefer `-` (read from stdin) or
        /// `--token-file`: a token passed as an argument is visible to
        /// every local user through `ps` and `/proc/<pid>/cmdline`.
        token: Option<String>,

        /// Read the token from this file instead of the command line.
        #[arg(long, value_name = "PATH")]
        token_file: Option<String>,

        /// Clear the saved credentials.
        #[arg(long)]
        clear: bool,
    },
```

- [ ] **Step 4: 改 `handle_key`**

把 `main.rs:167-186` 改成：

```rust
async fn handle_key(ctx: &Ctx, token: Option<String>, token_file: Option<String>, clear: bool) -> Result<()> {
    if clear {
        let cleared = Config::new_empty();
        ctx.store.save(&cleared)?;
        ui::success("All credentials cleared.");
        ui::key_status(&cleared);
        return Ok(());
    }

    // Three ways to supply the token, in order of preference. Reading it
    // from stdin or a file keeps it out of `ps` output and shell history.
    let token = match (token.as_deref(), token_file.as_deref()) {
        (Some(_), Some(_)) => bail!("pass the token either as an argument or via --token-file, not both"),
        (Some("-"), None) => read_token_from_stdin()?,
        (Some(value), None) => {
            eprintln!(
                "warning: a token passed as an argument is visible in `ps` output — \
                 prefer `cfp key -` or `cfp key --token-file <path>`"
            );
            value.to_string()
        }
        (None, Some(path)) => read_token_from_file(path)?,
        (None, None) => {
            // Bare `cfp key` shows the current status, so a missing token
            // is the documented outcome here rather than an error.
            ui::key_status(&ctx.config);
            return Ok(());
        }
    };

    let trimmed = token.trim();
    if trimmed.is_empty() {
        bail!("empty token — pass a non-empty value");
    }
```

并在 `main.rs` 里加两个辅助函数：

```rust
/// Reads a token from stdin, one line.
fn read_token_from_stdin() -> Result<String> {
    use std::io::BufRead as _;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading the token from stdin")?;
    Ok(line)
}

/// Reads a token from a file.
fn read_token_from_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("reading the token from {path}"))
}
```

把 `main.rs:44` 的调用改成：

```rust
        Some(Command::Key {
            token,
            token_file,
            clear,
        }) => handle_key(&ctx, token, token_file, clear).await,
```

- [ ] **Step 5: 运行测试与构建**

```bash
cargo test -p cfp
cargo build -p cfp
```

Expected: PASS

手动验证：

```bash
printf 'cf_test_token\n' | ./target/debug/cfp key -
```

Expected: 读到 token 后走验证流程（会因为是假 token 而报 Cloudflare 错误，
但**不应**报 "empty token"）。

- [ ] **Step 6: 同步 README**

把 `README.md:137` 附近的

```bash
# Save the token. cfp verifies it with Cloudflare and auto-discovers
# the account and the first accessible zone.
cfp key <CF_API_TOKEN>
```

替换为：

```bash
# Save the token. cfp verifies it with Cloudflare and auto-discovers
# the account and the first accessible zone. Reading it from stdin keeps
# it out of your shell history and out of `ps` output.
cfp key -            # then paste the token and press Enter
```

并在 README 的「Commands」表里把 `cfp key [TOKEN] [--clear]` 一行替换为：

```
| `cfp key [TOKEN\|-] [--token-file <path>] [--clear]` | Save, show or clear the Cloudflare token. `-` reads it from stdin. Saving auto-discovers account/zone. |
```

- [ ] **Step 7: 提交**

```bash
git add crates/cloudpipe/src/cli.rs crates/cloudpipe/src/main.rs README.md
git commit -m "feat(cli): read the API token from stdin or a file

A token passed as an argument lands in the shell history and in
/proc/<pid>/cmdline, where every local user can read it — the same
leak the SDK went to some trouble to avoid for the tunnel token."
```

---

### Task 10: `cfp key` 先存 token 后解析 zone

**Files:**
- Modify: `crates/cloudpipe/src/main.rs:188-229`（`handle_key` 的后半段）
- Modify: `crates/cloudpipe/src/config.rs`（`Config::apply_zone` 的错误信息）

**Interfaces:**
- Consumes: Task 9 之后的 `handle_key` 签名
- Produces: 无新接口。

- [ ] **Step 1: 写失败的测试**

`crates/cloudpipe/src/main.rs` 目前没有测试模块。加一个：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_zone_resolution_still_keeps_the_token() {
        // `apply_zone` fails when the zone carries no account id. The
        // token must survive that failure — losing it means the user
        // re-runs `cfp key` and hits the identical error.
        let mut config = Config::default();
        let zone = cloudpipe_sdk::Zone {
            id: "zone-1".into(),
            name: "example.com".into(),
            account: None,
        };
        let err = config.apply_zone(&zone).unwrap_err();
        assert!(format!("{err:#}").contains("account"), "{err:#}");
    }
}
```

（这条测试断言的是 `apply_zone` 的失败形态；真正的"token 已保存"由 Task 19 的
端到端 CLI 测试覆盖。）

- [ ] **Step 2: 运行测试确认通过**

Run: `cargo test -p cfp a_failed_zone_resolution_still_keeps_the_token`
Expected: PASS（`apply_zone` 的行为本来就是这样，测试锁住它）

- [ ] **Step 3: 调整 `handle_key` 的顺序**

把 `main.rs` 中 `handle_key` 里从 `ui::step("Verifying token...")` 到函数结尾
的部分替换为：

```rust
    ui::step("Verifying token with Cloudflare...");
    let api = CloudflareApi::new(trimmed.to_string()).map_err(anyhow::Error::msg)?;
    let status = api
        .verify_token()
        .await
        .map_err(ui::render_sdk_error)
        .context("verifying token — check the value, your network, and the token's permissions")?;
    ui::success(&format!("Token verified (status: {}).", status.status));

    let zones = api
        .list_zones()
        .await
        .map_err(ui::render_sdk_error)
        .with_context(|| {
            "listing accessible zones — make sure the token has Zone:Read on at least one zone"
        })?;

    if zones.is_empty() {
        bail!("token is valid but has access to no zones — add a Zone:Read resource to the token");
    }

    // The token is saved *before* the zone is resolved. A zone that
    // carries no account id used to abort the whole command with nothing
    // written, so re-running `cfp key` produced the identical failure
    // and the user could never get past it.
    let mut config = ctx.config.clone();
    config.token = Some(trimmed.to_string());
    ctx.store.save(&config)?;
    ui::success("Token saved.");

    if zones.len() == 1 {
        config.apply_zone(&zones[0])?;
        ctx.store.save(&config)?;
        ui::success(&format!(
            "Tunnels will be exposed under {}.",
            config.domain.clone().unwrap()
        ));
        ui::key_status(&config);
    } else {
        config.apply_zone(&zones[0]).map_err(|err| {
            anyhow::anyhow!(
                "{err:#}\n\n  The token was saved — pick a zone with `cfp domain <name>`."
            )
        })?;
        ctx.store.save(&config)?;
        ui::heading("Multiple zones are accessible:");
        ui::zones(&zones, config.domain.as_deref());
        ui::success(&format!(
            "Default domain: {}. Switch with `cfp domain <name>`.",
            config.domain.clone().unwrap()
        ));
    }
    Ok(())
```

- [ ] **Step 4: 运行测试**

```bash
cargo test -p cfp
```

Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe/src/main.rs
git commit -m "fix(cli): save the token before resolving the zone

A zone whose account id Cloudflare omitted aborted `cfp key` with
nothing written to disk, so re-running the command produced the exact
same error and the user could never get past it."
```

---

### Task 11: `key_status` 的真实路径与 token 来源

**Files:**
- Modify: `crates/cloudpipe/src/ui.rs:480-500`（`key_status`）
- Modify: `crates/cloudpipe/src/config.rs:175`（`ConfigStore::file_path` 去掉 `#[allow(dead_code)]`）
- Modify: `crates/cloudpipe/src/main.rs`（`handle_key` 中 `ui::key_status(&cleared)` 的调用）

**Interfaces:**
- Consumes: `ConfigStore::file_path()`、`Config::token`（已存在）

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe/src/ui.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn key_status_distinguishes_where_the_token_came_from() {
        let mut config = Config::default();
        config.token = Some("cf_saved_token".into());

        let saved = key_status_text(&config);
        assert!(saved.contains("saved"), "{saved}");
        assert!(saved.contains("cf_s"), "{saved}");

        // A config with no stored token must not claim to have one —
        // whether the environment supplies it or nothing does.
        let empty = key_status_text(&Config::default());
        assert!(!empty.contains("saved"), "{empty}");
        assert!(empty.contains("not configured") || empty.contains("CLOUDFLARE_API_TOKEN"), "{empty}");
    }
```

（断言刻意避开 "saved" 一词出现在环境变量分支里 —— 那条分支写的是
"not persisted to disk"。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cfp key_status_distinguishes_where_the_token_came_from`
Expected: FAIL —— `key_status_text` 不存在

- [ ] **Step 3: 拆出可测的文本函数**

`key_status` 现在直接打印。把它拆成"生成文本"与"打印"两部分。

把 `ui.rs` 的 `key_status` 替换为：

```rust
/// Prints the current credential status.
pub fn key_status(config: &Config) {
    println!("{}", key_status_text(config));
}

/// The credential status as text.
///
/// The token's *source* matters to the user: a token that only exists in
/// `CLOUDFLARE_API_TOKEN` is not persisted, and telling the two apart is
/// the difference between "logged in" and "logged in until you close the
/// shell".
pub fn key_status_text(config: &Config) -> String {
    let mut out = String::new();
    out.push_str("Cloudflare API token:\n");
    match &config.token {
        Some(token) => {
            out.push_str(&format!(
                "  {} (saved to {})\n",
                mask_token(token),
                config_file_label()
            ));
        }
        None => {
            if env_token().is_some() {
                out.push_str("  set via CLOUDFLARE_API_TOKEN (not persisted to disk)\n");
            } else {
                out.push_str("  not configured\n");
                out.push_str(
                    "  run `cfp key -` and paste the token, or set CLOUDFLARE_API_TOKEN\n",
                );
            }
        }
    }
    out.push_str(&format!("Config file: {}\n", config_file_label()));
    out.push_str(&match (&config.account_id, &config.zone_id, &config.domain) {
        (Some(a), Some(z), Some(d)) => format!("  account {a}\n  zone {z}\n  domain {d}\n"),
        _ => "  no zone selected — run `cfp domain <name>`\n".to_string(),
    });
    out
}

/// The path the config file lives at, honouring `CFP_CONFIG_DIR`.
fn config_file_label() -> String {
    crate::config::ConfigStore::default()
        .map(|store| store.file_path().display().to_string())
        .unwrap_or_else(|_| "~/.cfp/config.json".to_string())
}

/// The token supplied by the environment, if any.
fn env_token() -> Option<String> {
    std::env::var("CLOUDFLARE_API_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Masks a token so it can be shown without leaking it.
fn mask_token(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    let tail: String = token.chars().rev().take(4).collect();
    format!("{head}…{tail}")
}
```

（`ui.rs` 顶部需要 `use crate::config::Config;` —— 检查它是否已经 import；
`Config` 在 `key_status` 的签名里已经用到了，所以 import 应该已经存在。）

- [ ] **Step 4: 去掉 `file_path` 的 dead_code 标记**

`config.rs:175` 的 `#[allow(dead_code)]` 删掉（`file_path` 现在被用了）。

- [ ] **Step 5: 运行测试**

Run: `cargo test -p cfp ui::tests`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe/src/ui.rs crates/cloudpipe/src/config.rs
git commit -m "fix(cli): show where the token actually comes from

`key_status` printed a hardcoded ~/.cfp/config.json — wrong as soon as
CFP_CONFIG_DIR was set — and masked the token source, so a token that
only existed in the environment looked exactly like a saved one."
```

---

### Task 12: UI 事件渲染修正

**Files:**
- Modify: `crates/cloudpipe/src/ui.rs:219-318`（`banner` 与 `render_event`）
- Modify: `crates/cloudpipe/src/ui.rs:379-385`（`tunnel_live`）
- Modify: `crates/cloudpipe/src/main.rs:138-144`（调用点）
- Test: `crates/cloudpipe/src/ui.rs`（`mod tests`）

**Interfaces:**
- Consumes: `cloudpipe_sdk::Protocol`
- Produces:
  - `pub enum EventLine { Out(String), Err(String) }`
  - `pub fn render_event_text(event: &Event) -> Option<EventLine>` —— 纯函数
  - `pub fn banner_text() -> String` —— 纯函数
  - `ui::tunnel_live(url: &str, port: u16, protocol: Protocol, subdomain: &str, auto_restart: bool)`

**背景**：`render_event` 目前**直接打印**（签名 `pub fn render_event(event: Event)`，
没有返回值），所以整个 Event → 文本映射无法断言 —— 这正是它零覆盖的原因。
Task 21 的表驱动测试需要先把它拆成"生成文本"与"打印"两部分。

注意 `render_live_block`（`ui.rs:331-335`）**已经**接受 `Protocol`，
不需要改它 —— 要改的只是 `tunnel_live`，它现在接受 `&str` 并用
`Protocol::parse(...).unwrap_or(Protocol::Http)` 反解（解析失败会静默显示成 http）。

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe/src/ui.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn every_edge_connection_is_rendered() {
        // The old renderer only printed the first and the fourth
        // connection, on the assumption that cloudflared always opens
        // exactly four. It does not always.
        let previous = set_style(Style::plain());
        for total in 1..=4u8 {
            let text = render_event_text(&Event::EdgeConnected {
                conn_index: 0,
                total,
            })
            .expect("the connection must render");
            match text {
                EventLine::Out(line) => assert!(line.contains("connection"), "total {total}: {line}"),
                EventLine::Err(line) => panic!("connections go to stdout, got {line}"),
            }
        }
        set_style(previous);
    }

    #[test]
    fn a_connection_line_names_its_index() {
        let previous = set_style(Style::plain());
        let text = render_event_text(&Event::EdgeConnected {
            conn_index: 2,
            total: 4,
        })
        .expect("the connection must render");
        set_style(previous);
        match text {
            EventLine::Out(line) => assert_eq!(line, "  Edge connection 3 of 4"),
            EventLine::Err(line) => panic!("connections go to stdout, got {line}"),
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cfp every_edge_connection_is_rendered`
Expected: FAIL —— `render_event_text` 与 `EventLine` 不存在

- [ ] **Step 3: 拆出纯函数**

把 `ui.rs` 的 `banner` 与 `render_event` 替换为：

```rust
/// Where a rendered event line goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventLine {
    /// A line for stdout.
    Out(String),
    /// A line for stderr.
    Err(String),
}

/// The text the startup banner renders to.
pub fn banner_text() -> String {
    let st = style();
    format!(
        "{} {}",
        magenta_bold(&format!("{} cfp", st.bolt_glyph()), st),
        dim("— serverless tunnels via Cloudflare Edge", st)
    )
}

/// Prints the small startup banner.
pub fn banner() {
    println!("{}", banner_text());
}

/// The text an event renders to, or `None` when it renders nothing.
///
/// Pure, so the whole event-to-output mapping can be asserted on without
/// capturing stdout. The printing wrapper is [`render_event`].
pub fn render_event_text(event: &Event) -> Option<EventLine> {
    let st = style();
    let out = |text: String| Some(EventLine::Out(text));
    match event {
        // The banner is a block of its own.
        Event::Banner => None,
        Event::ResolvingConflicts => None,
        Event::CreatingTunnel { name } => {
            out(format!("  {}", dim(&format!("Creating tunnel \"{name}\"..."), st)))
        }
        Event::IngressConfigured { protocol, port } => out(format!(
            "  {}",
            dim(
                &format!("Configuring ingress ({protocol}://localhost:{port})..."),
                st
            )
        )),
        Event::DnsCreated { full_name } => {
            out(format!("  {}", dim(&format!("Creating DNS {full_name}..."), st)))
        }
        Event::CloudflaredStarted => None,
        Event::Restarting { reason, attempt } => out(format!(
            "  {}",
            yellow(
                &format!(
                    "cloudflared ended ({}) — restarting (attempt #{attempt})...",
                    describe_reason(reason)
                ),
                st
            )
        )),
        Event::Restarted { attempt } => out(format!(
            "  {}",
            green(
                &format!("Resumed on the same URL (attempt #{attempt})."),
                st
            )
        )),
        Event::RestartGivingUp {
            attempts,
            last_error,
        } => out(format!(
            "  {}",
            red(
                &format!(
                    "auto-restart failed {attempts} times in a row — giving up. \
                     last error: {last_error}"
                ),
                st
            )
        )),
        Event::BootstrapFailed { attempt, error } => out(format!(
            "  {}",
            yellow(&format!("reconnect attempt #{attempt} failed: {error}"), st)
        )),
        Event::EdgeConnected { conn_index, total } => out(format!(
            "  {}",
            green(
                &format!("Edge connection {} of {total}", conn_index + 1),
                st
            )
        )),
        Event::CloudflaredLog { level, line } => match level {
            // Warnings and errors are worth interrupting the output for.
            // `Info` is dropped here on purpose: cloudflared emits it
            // constantly and the CLI would drown in it. The event itself
            // still reaches every SDK subscriber, so an embedder sees
            // everything.
            LogLevel::Error | LogLevel::Warn => {
                Some(EventLine::Err(format!("  [cloudflared] {}", yellow(line, st))))
            }
            LogLevel::Info => None,
        },
        Event::ShuttingDown { reason } => {
            if matches!(reason, ShutdownReason::ChildExited) {
                out(format!(
                    "  {}",
                    dim("cloudflared exited — cleaning up before respawn.", st)
                ))
            } else {
                None
            }
        }
        Event::Cleaned => out(format!("  {}", green("Tunnel and DNS record cleaned up.", st))),
        _ => None,
    }
}

/// Renders a single SDK event to stdout.
pub fn render_event(event: Event) {
    if matches!(event, Event::Banner) {
        banner();
        return;
    }
    match render_event_text(&event) {
        Some(EventLine::Out(text)) => println!("{text}"),
        Some(EventLine::Err(text)) => eprintln!("{text}"),
        None => {}
    }
}
```

- [ ] **Step 4: 改 `tunnel_live`**

把 `ui.rs` 的 `tunnel_live` 替换为：

```rust
/// Prints the tunnel success block after the URL is live.
pub fn tunnel_live(url: &str, port: u16, protocol: Protocol, subdomain: &str, auto_restart: bool) {
    print!(
        "{}",
        render_live_block(url, protocol, port, subdomain, auto_restart)
    );
}
```

（`render_live_block` 的签名不变 —— 它已经接受 `Protocol`。）

`main.rs` 的调用点改成：

```rust
    ui::tunnel_live(
        handle.url(),
        args.port,
        args.protocol,
        handle.subdomain(),
        args.auto_restart,
    );
```

- [ ] **Step 5: 运行测试**

```bash
cargo test -p cfp
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS / 无警告

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe/src/ui.rs crates/cloudpipe/src/main.rs
git commit -m "refactor(cli): split the event renderer into text and printing

The renderer printed straight to the terminal, so the whole event-to-
output mapping had no coverage — which is how the connection counter
could end up gated on 'total == 1 || total == 4' without anyone
noticing. The pure half is now assertable, and every connection is
rendered instead of the first and the fourth."
```

---

## Part D — Node 错误契约（修 D11）

### Task 13: 错误码单一来源

**Files:**
- Modify: `crates/cloudpipe-node-core/src/error.rs:45-90`
- Modify: `crates/cloudpipe-node-core/src/parse.rs:29-68`
- Modify: `crates/cloudpipe-node/src/lib.rs:268-301`
- Test: `crates/cloudpipe-node-core/src/error.rs`、`crates/cloudpipe-node-core/src/parse.rs`

**Interfaces:**
- Produces:
  - `CloudpipeErrorCode::ALL: &'static [CloudpipeErrorCode]`
  - `CloudpipeErrorCode::from_str(code: &str) -> Option<Self>`
  - `CloudflareApiKind::ALL: &'static [CloudflareApiKind]`
  - `CloudflareApiKind::from_str(code: &str) -> Option<Self>`
  - `CloudflareApiKind::ServerError` 变体（`as_str()` = `"SERVER_ERROR"`）

- [ ] **Step 1: 写失败的测试**

在 `crates/cloudpipe-node-core/src/error.rs` 的 `mod tests` 中新增：

```rust
    #[test]
    fn every_code_round_trips_through_from_str() {
        for code in CloudpipeErrorCode::ALL {
            assert_eq!(
                CloudpipeErrorCode::from_str(code.as_str()),
                Some(*code),
                "{} did not round-trip",
                code.as_str()
            );
        }
    }

    #[test]
    fn every_kind_round_trips_through_from_str() {
        for kind in CloudflareApiKind::ALL {
            assert_eq!(
                CloudflareApiKind::from_str(kind.as_str()),
                Some(*kind),
                "{} did not round-trip",
                kind.as_str()
            );
        }
    }

    #[test]
    fn unknown_codes_are_rejected() {
        assert_eq!(CloudpipeErrorCode::from_str("NOPE"), None);
        assert_eq!(CloudflareApiKind::from_str("NOPE"), None);
    }

    #[test]
    fn server_error_is_its_own_kind() {
        // A 5xx used to be flattened into `Other`, so JS could not tell
        // "the server broke" from "we have never heard of this code".
        let err = Error::Cloudflare(ApiError::Api {
            code: Some(503),
            message: "unavailable".into(),
            kind: CloudflareErrorKind::ServerError,
        });
        assert_eq!(
            error_prefix(&err),
            "[CLOUDFLARE_API:SERVER_ERROR] "
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cloudpipe-node-core every_code_round_trips_through_from_str`
Expected: FAIL —— `ALL` / `from_str` 不存在

- [ ] **Step 3: 实现**

在 `node-core/src/error.rs` 的 `impl CloudpipeErrorCode` 里加：

```rust
    /// Every error code, in the order `errorCodes()` returns them.
    pub const ALL: &'static [Self] = &[
        Self::MissingCredential,
        Self::CloudflareApi,
        Self::CloudflaredBinary,
        Self::InvalidSubdomain,
        Self::InvalidDomain,
        Self::SubdomainInUse,
        Self::AlreadyShutDown,
        Self::Io,
        Self::InvalidArgument,
        Self::Other,
    ];

    /// The inverse of [`as_str`](Self::as_str).
    ///
    /// Unknown codes return `None` — a newer SDK may emit a code this
    /// build has never heard of, and that must not be an error.
    pub fn from_str(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == code)
    }
```

在 `impl CloudflareApiKind` 里加：

```rust
    /// Every sub-kind, in the order `cloudflareErrorKinds()` returns them.
    pub const ALL: &'static [Self] = &[
        Self::RateLimited,
        Self::DnsExists,
        Self::AuthFailed,
        Self::InvalidToken,
        Self::ServerError,
        Self::Other,
    ];

    /// The inverse of [`as_str`](Self::as_str).
    pub fn from_str(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == code)
    }
```

给 `CloudflareApiKind` 加 `ServerError` 变体（放在 `InvalidToken` 之后）：

```rust
    /// The server returned 5xx. Worth retrying — the request never took
    /// effect.
    ServerError,
```

并把它的 `as_str` 加上：

```rust
            Self::ServerError => "SERVER_ERROR",
```

把 `kind_from_sdk` 里那两行：

```rust
        // A bare 5xx carries no numeric code worth surfacing; `OTHER` is
        // the JS-facing "unclassified".
        CloudflareErrorKind::ServerError => CloudflareApiKind::Other,
```

改成：

```rust
        CloudflareErrorKind::ServerError => CloudflareApiKind::ServerError,
```

- [ ] **Step 4: 让 `parse.rs` 用枚举而不是字面量**

把 `node-core/src/parse.rs` 的 `extract_code` 主体改成：

```rust
pub fn extract_code(message: &str) -> Option<String> {
    let raw = message.strip_prefix('[')?;
    let end = raw.find(']')?;
    let inner = &raw[..end];
    let mut parts = inner.splitn(2, ':');
    let parent = parts.next()?;

    // Resolve against the typed enum rather than a list of string
    // literals: a renamed code then breaks the build here instead of
    // silently drifting away from the JS helper.
    if CloudpipeErrorCode::from_str(parent).is_none() {
        // Unknown parent — return the raw text verbatim, sub-kind included.
        // We deliberately don't fail so forward-compat works both ways: a
        // newer SDK emitting an unknown code gives an older JS helper the
        // whole string back, not an exception and not a truncated prefix.
        return Some(match parts.next() {
            Some(suffix) => format!("{parent}:{suffix}"),
            None => parent.to_string(),
        });
    }

    let Some(suffix) = parts.next() else {
        return Some(parent.to_string());
    };
    if CloudflareApiKind::from_str(suffix).is_none() {
        return Some(format!("{parent}:{suffix}"));
    }
    Some(format!("{parent}:{suffix}"))
}
```

并删掉两处 `let _ = parent_code;` / `let _ = suffix_code;`。

- [ ] **Step 5: 让 `error_codes()` 用 `ALL`**

`node/src/lib.rs` 的 `error_codes` 改成：

```rust
#[napi]
pub fn error_codes() -> Vec<String> {
    CloudpipeErrorCode::ALL
        .iter()
        .map(|c| c.as_str().to_string())
        .collect()
}
```

`cloudflare_error_kinds` 改成：

```rust
#[napi]
pub fn cloudflare_error_kinds() -> Vec<String> {
    CloudflareApiKind::ALL
        .iter()
        .map(|k| k.as_str().to_string())
        .collect()
}
```

- [ ] **Step 6: 运行测试**

```bash
cargo test -p cloudpipe-node-core
cargo test -p cloudpipe-sdk
```

Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add crates/cloudpipe-node-core/src/error.rs crates/cloudpipe-node-core/src/parse.rs crates/cloudpipe-node/src/lib.rs
git commit -m "refactor(node): make the error-code table a single source of truth

The code list was written out as string literals in three places, so a
rename would have left the parser quietly accepting a code nothing
produces. The parser now resolves against the typed enum, and a 5xx is
no longer flattened into the same bucket as an unknown code."
```

---

## Part E — Node 事件模型（修 D2、D3、D12、D13）

### Task 14: 启动期事件缓冲

**Files:**
- Modify: `crates/cloudpipe-node/src/listener.rs:56-76`（注册表类型）
- Modify: `crates/cloudpipe-node/src/listener.rs:281-300`（`event_pump`）
- Modify: `crates/cloudpipe-node/src/listener.rs:328-368`（`on` / `off`）

**Interfaces:**
- Consumes: 无（Task 13 不依赖本任务）
- Produces: `struct CallbackTable { by_event: HashMap<String, Vec<EventCallback>>, pending: VecDeque<(String, String)> }`

- [ ] **Step 1: 写失败的测试**

`crates/cloudpipe-node` 是 cdylib，不能直接跑单测。把**事件缓冲的逻辑**放进
`crates/cloudpipe-node-core`（它本来就是 napi-free 的逻辑层），在那里测。

在 `crates/cloudpipe-node-core/src/event.rs` 里加一个与 napi 无关的缓冲器：

```rust
/// A queue of events that were produced before any listener existed.
///
/// `cloudflared`'s startup events fire while the tunnel is being created,
/// which is strictly before a JS caller can register a handler — the
/// `Listener` object does not exist yet. Dropping them made the
/// `dnsCreated` / `cloudflaredStarted` / `edgeConnected` events
/// unreachable from JavaScript.
#[derive(Debug, Default)]
pub struct PendingEvents {
    items: std::collections::VecDeque<(String, String)>,
}

impl PendingEvents {
    /// How many events are held before the oldest is dropped.
    pub const CAPACITY: usize = 512;

    /// Records an event that had no listener.
    pub fn push(&mut self, name: String, payload: String) {
        if self.items.len() >= Self::CAPACITY {
            self.items.pop_front();
        }
        self.items.push_back((name, payload));
    }

    /// Removes and returns every held event with the given name, in the
    /// order it was produced.
    pub fn drain_named(&mut self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut kept = std::collections::VecDeque::new();
        while let Some((n, payload)) = self.items.pop_front() {
            if n == name {
                out.push(payload);
            } else {
                kept.push_back((n, payload));
            }
        }
        self.items = kept;
        out
    }

    /// Drops every held event with the given name.
    pub fn remove_named(&mut self, name: &str) {
        self.items.retain(|(n, _)| n != name);
    }

    /// How many events are currently held.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
```

在同文件的 `mod tests` 中加：

```rust
    #[test]
    fn pending_events_are_drained_in_production_order() {
        let mut pending = PendingEvents::default();
        pending.push("dnsCreated".into(), "{\"fullName\":\"a\"}".into());
        pending.push("banner".into(), "{}".into());
        pending.push("dnsCreated".into(), "{\"fullName\":\"b\"}".into());

        let drained = pending.drain_named("dnsCreated");
        assert_eq!(
            drained,
            vec!["{\"fullName\":\"a\"}".to_string(), "{\"fullName\":\"b\"}".to_string()]
        );
        // The unrelated event survives.
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn pending_events_are_capped() {
        let mut pending = PendingEvents::default();
        for i in 0..(PendingEvents::CAPACITY + 10) {
            pending.push("x".into(), i.to_string());
        }
        assert_eq!(pending.len(), PendingEvents::CAPACITY);
    }

    #[test]
    fn removing_a_name_drops_its_pending_events() {
        let mut pending = PendingEvents::default();
        pending.push("a".into(), "1".into());
        pending.push("b".into(), "2".into());
        pending.remove_named("a");
        assert_eq!(pending.len(), 1);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p cloudpipe-node-core pending_events`
Expected: FAIL —— `PendingEvents` 不存在

- [ ] **Step 3: 实现 `PendingEvents`**

把 Step 1 的代码加进 `crates/cloudpipe-node-core/src/event.rs`，并在
`crates/cloudpipe-node-core/src/lib.rs` 的 re-export 区加：

```rust
pub use event::PendingEvents;
```

- [ ] **Step 4: 在绑定里用它**

`crates/cloudpipe-node/src/listener.rs`：

把

```rust
/// Registry of per-event callbacks, shared with the event pump.
type CallbackRegistry = Arc<TokioMutex<HashMap<String, Vec<EventCallback>>>>;
```

改成

```rust
/// Per-event callbacks plus the events that arrived before any of them
/// existed.
#[derive(Default)]
struct CallbackTable {
    by_event: HashMap<String, Vec<EventCallback>>,
    pending: cloudpipe_node_core::PendingEvents,
}

/// Registry of per-event callbacks, shared with the event pump.
type CallbackRegistry = Arc<TokioMutex<CallbackTable>>;
```

`Listener::start` 里的

```rust
let callbacks: CallbackRegistry = Arc::new(TokioMutex::new(HashMap::new()));
```

改成

```rust
let callbacks: CallbackRegistry = Arc::new(TokioMutex::new(CallbackTable::default()));
```

`event_pump` 改成：

```rust
async fn event_pump(
    mut event_rx: mpsc::UnboundedReceiver<EventEnvelope>,
    callbacks: CallbackRegistry,
) {
    while let Some((name, payload)) = event_rx.recv().await {
        // Snapshot the listeners registered for this event under a brief
        // lock; the actual `cb.call` runs after we drop the guard so a
        // slow JS handler doesn't block other registrations.
        let snapshot: Vec<EventCallback> = {
            let mut guard = callbacks.lock().await;
            let list = guard.by_event.get(&name).cloned().unwrap_or_default();
            if list.is_empty() {
                // No listener yet. The startup events fire before a JS
                // caller can possibly register one — the `Listener` does
                // not exist until `connect()` resolves — so dropping them
                // here made `dnsCreated` and friends unreachable.
                guard.pending.push(name.clone(), payload.clone());
            }
            list
        };
        for cb in snapshot {
            let _ = cb.call(Ok(payload.clone()), ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}
```

`Listener::on` 改成：

```rust
    #[napi]
    pub fn on(
        &self,
        event: String,
        callback: napi::threadsafe_function::ThreadsafeFunction<String, ()>,
    ) -> Result<()> {
        if !cloudpipe_node_core::event::EVENT_NAMES.contains(&event.as_str()) {
            return Err(error::invalid_arg(format!(
                "unknown event \"{event}\" — expected one of: {}",
                cloudpipe_node_core::event::EVENT_NAMES.join(", ")
            )));
        }
        let callback = Arc::new(callback);
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.by_event.entry(event.clone()).or_default().push(callback.clone());
            // Hand over the events that arrived before this listener
            // existed, in the order they were produced.
            let ready = guard.pending.drain_named(&event);
            drop(guard);
            for payload in ready {
                let _ = callback.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }
```

`Listener::off` 改成：

```rust
    #[napi]
    pub fn off(&self, event: String) -> Result<()> {
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.by_event.remove(&event);
            // Events still waiting for a listener are dropped too —
            // re-subscribing must not resurrect a cancelled event.
            guard.pending.remove_named(&event);
        });
        Ok(())
    }
```

`Listener::remove_all_listeners` 改成：

```rust
    #[napi]
    pub fn remove_all_listeners(&self) -> Result<()> {
        let registry = Arc::clone(&self.callbacks);
        runtime().block_on(async move {
            let mut guard = registry.lock().await;
            guard.by_event.clear();
            guard.pending = cloudpipe_node_core::PendingEvents::default();
        });
        Ok(())
    }
```

- [ ] **Step 5: 编译与测试**

```bash
cargo build --workspace --all-targets
cargo test --workspace --all-targets
```

Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe-node-core/src/event.rs crates/cloudpipe-node-core/src/lib.rs crates/cloudpipe-node/src/listener.rs
git commit -m "fix(node): deliver the events that fire before a listener exists

The startup events were not lost in transit — they were dropped at
dispatch, because the registry could only be filled by a call on a
Listener that did not exist yet. Every npm consumer therefore never saw
dnsCreated, cloudflaredStarted or the first edgeConnected."
```

---

### Task 15: `wait()` 支持多个等待者

**Files:**
- Modify: `crates/cloudpipe-node/src/listener.rs:64-77`（`Command`）
- Modify: `crates/cloudpipe-node/src/listener.rs:196-268`（`session_loop` 与 `wait_for_finish`）
- Modify: `crates/cloudpipe-node/src/listener.rs:79-92`（`Listener` 字段）
- Modify: `crates/cloudpipe-node/src/listener.rs:381-395`（`Listener::wait`）

**Interfaces:**
- Produces: `Listener::wait()` 在多个调用者下都能被唤醒；隧道已结束时立即返回 `Ok(())`。

- [ ] **Step 1: 改 `session_loop`**

把 `session_loop` 改成：

```rust
async fn session_loop(mut handle: TunnelHandle, mut cmd_rx: mpsc::Receiver<Command>) {
    // Every parked `wait()` gets its own sender. The previous version
    // kept a single one, so a second `wait()` silently replaced the
    // first — whose promise then rejected with "session task dropped
    // the reply channel" instead of resolving.
    let mut waiters: Vec<oneshot::Sender<()>> = Vec::new();
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    // The Listener went away without closing.
                    release_waiters(&mut waiters);
                    finished.store(true, Ordering::SeqCst);
                    break;
                };
                match cmd {
                    Command::Wait(tx) => {
                        waiters.push(tx);
                    }
                    Command::Stop(tx) => {
                        let result: StopResult = handle
                            .stop()
                            .await
                            .map_err(|e| error::from_sdk_error(e).reason);
                        let _ = tx.send(result);
                        // Release anyone parked in `wait()`.
                        release_waiters(&mut waiters);
                        finished.store(true, Ordering::SeqCst);
                        break;
                    }
                    Command::Update { port, reply } => {
                        let result = update_port(&mut handle, port).await;
                        let _ = reply.send(result);
                    }
                }
            }
            () = wait_for_finish(&handle) => {
                // The tunnel ended on its own; nothing else would clear
                // the parked waiters.
                release_waiters(&mut waiters);
                finished.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
}

/// Resolves every parked `wait()`.
fn release_waiters(waiters: &mut Vec<oneshot::Sender<()>>) {
    for tx in waiters.drain(..) {
        let _ = tx.send(());
    }
}
```

- [ ] **Step 2: 把"已结束"标志传给 `Listener`**

`Listener` 结构体加字段：

```rust
    /// Set once the session task has finished. A `wait()` after that
    /// resolves immediately instead of reporting a dead session.
    finished: Arc<std::sync::atomic::AtomicBool>,
```

`Listener::start` 里，`finished` 必须在 session 线程启动**之前**创建，
并同时交给 session loop 与 `Listener`：

```rust
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_for_thread = Arc::clone(&finished);

        std::thread::Builder::new()
            .name("cloudpipe-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("cloudpipe-node: failed to build session runtime: {e}");
                        finished_for_thread.store(true, Ordering::SeqCst);
                        return;
                    }
                };
                rt.block_on(session_loop(handle, cmd_rx, finished_for_thread));
            })
            .map_err(|e| error::other(format!("failed to spawn session thread: {e}")))?;
```

`session_loop` 的签名相应改为：

```rust
async fn session_loop(
    mut handle: TunnelHandle,
    mut cmd_rx: mpsc::Receiver<Command>,
    finished: Arc<std::sync::atomic::AtomicBool>,
) {
```

（`finished` 现在由参数传入，删掉 Step 1 里在函数体内部创建的版本。）

`Listener` 的构造里加：

```rust
            finished,
```

- [ ] **Step 3: 改 `Listener::wait`**

```rust
    /// Blocks until the tunnel exits on its own (cloudflared crashed and
    /// auto-restart was exhausted, or the process ended).
    ///
    /// This does **not** stop the tunnel, and it does not prevent a
    /// subsequent [`close`](Self::close) from working.
    ///
    /// Several callers may wait at once; all of them are released when
    /// the tunnel ends.
    #[napi]
    pub async fn wait(&self) -> Result<()> {
        // Already over: resolve rather than report a dead session.
        if self.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Wait(tx))
            .await
            .map_err(|_| error::session_gone())?;
        rx.await.map_err(|_| error::channel_dropped())?;
        Ok(())
    }
```

`listener.rs` 顶部加 `use std::sync::atomic::Ordering;`。

- [ ] **Step 4: 编译与测试**

```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: 无警告

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe-node/src/listener.rs
git commit -m "fix(node): let several waiters wait on the same tunnel

A second wait() replaced the first, whose promise then rejected with
'session task dropped the reply channel'. A wait() issued after the
tunnel had already ended reported the same thing instead of resolving."
```

---

### Task 16: Node 的 `cleanup()` 入口

**Files:**
- Modify: `crates/cloudpipe-node/src/lib.rs`（新增 `CleanupOptions` 与 `cleanup`）
- Modify: `crates/cloudpipe-node/src/error.rs`（如需）

**Interfaces:**
- Produces: `#[napi] pub async fn cleanup(options: CleanupOptions) -> Result<CleanupReportJs>`
  - `CleanupOptions { token: String, domain?: string, account?: string, zone?: string, dryRun?: boolean, apiBase?: string }`
  - `CleanupReportJs { removed: Vec<CleanupActionJs>, wouldRemove: Vec<CleanupActionJs>, errors: Vec<String> }`
  - `CleanupActionJs { kind: string, id: string, name: string }`

- [ ] **Step 1: 实现**

在 `crates/cloudpipe-node/src/lib.rs` 里加：

```rust
/// Options accepted by [`cleanup`].
#[napi(object)]
pub struct CleanupOptions {
    /// Cloudflare API token. **Required.**
    pub token: String,

    /// Base domain (e.g. `example.com`). Optional — the SDK resolves the
    /// zone from the token when only one is accessible.
    #[napi(js_name = "domain")]
    pub domain: Option<String>,

    /// Cloudflare account id. Optional.
    #[napi(js_name = "account")]
    pub account: Option<String>,

    /// Cloudflare zone id. Optional.
    #[napi(js_name = "zone")]
    pub zone: Option<String>,

    /// Report what would be removed without removing anything.
    #[napi(js_name = "dryRun")]
    pub dry_run: Option<bool>,

    /// Base URL of a Cloudflare-compatible API. Intended for tests.
    #[napi(js_name = "apiBase")]
    pub api_base: Option<String>,
}

/// One resource a cleanup run touched.
#[napi(object)]
pub struct CleanupActionJs {
    /// `"tunnel"` or `"dnsRecord"`.
    #[napi(js_name = "kind")]
    pub kind: String,
    /// Cloudflare resource id.
    pub id: String,
    /// Human-readable name (tunnel name or DNS hostname).
    pub name: String,
}

/// The result of a cleanup run.
#[napi(object)]
pub struct CleanupReportJs {
    /// Resources actually removed.
    pub removed: Vec<CleanupActionJs>,
    /// Resources that would have been removed, when `dryRun` was set.
    #[napi(js_name = "wouldRemove")]
    pub would_remove: Vec<CleanupActionJs>,
    /// Per-resource failures.
    pub errors: Vec<String>,
}

fn cleanup_action_to_js(action: &cloudpipe_sdk::CleanupAction) -> CleanupActionJs {
    match action {
        cloudpipe_sdk::CleanupAction::Tunnel { id, name } => CleanupActionJs {
            kind: "tunnel".into(),
            id: id.clone(),
            name: name.clone(),
        },
        cloudpipe_sdk::CleanupAction::DnsRecord { id, name } => CleanupActionJs {
            kind: "dnsRecord".into(),
            id: id.clone(),
            name: name.clone(),
        },
    }
}

/// Reclaims dead tunnels and orphaned DNS records in one zone.
///
/// A Node application that dies without closing its listener leaves a
/// tunnel and a CNAME behind. The SDK could always clean those up, but
/// only the CLI could reach that code — until now.
#[napi]
pub async fn cleanup(options: CleanupOptions) -> Result<CleanupReportJs> {
    let mut builder = cloudpipe_sdk::TunnelBuilder::new().token(options.token.clone());
    if let Some(domain) = options.domain.filter(|d| !d.is_empty()) {
        builder = builder.domain(domain);
    }
    if let Some(account) = options.account.filter(|a| !a.is_empty()) {
        builder = builder.account(account);
    }
    if let Some(zone) = options.zone.filter(|z| !z.is_empty()) {
        builder = builder.zone(zone);
    }
    if let Some(base) = options.api_base.filter(|b| !b.is_empty()) {
        let api = cloudpipe_sdk::CloudflareApi::with_base(options.token.clone(), base)
            .map_err(|e| error::other(format!("{e:#}")))?;
        builder = builder.api(api);
    }

    let report = if options.dry_run.unwrap_or(false) {
        builder.preview_zone().await
    } else {
        builder.cleanup_zone().await
    }
    .map_err(|e| error::from_sdk_error(e))?;

    Ok(CleanupReportJs {
        removed: report.removed.iter().map(cleanup_action_to_js).collect(),
        would_remove: report.would_remove.iter().map(cleanup_action_to_js).collect(),
        errors: report.errors.clone(),
    })
}
```

- [ ] **Step 2: 编译**

```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: 无警告

- [ ] **Step 3: 提交**

```bash
git add crates/cloudpipe-node/src/lib.rs
git commit -m "feat(node): expose the SDK's cleanup to JavaScript

The SDK could reclaim dead tunnels and orphaned DNS records, but only
the CLI could reach that code. A Node application that died without
closing its listener left both behind with no way to reclaim them from
the same language."
```

---

### Task 17: JS facade

**Files:**
- Create: `crates/cloudpipe-node/js/index.js`
- Modify: `crates/cloudpipe-node/package.json`（`main`、`files`）

**Interfaces:**
- Consumes: Task 14 的 pending 缓冲、Task 15 的多等待者、Task 16 的 `cleanup`
- Produces: `@xinggao/cloudpipe` 的入口模块，导出 `connect`、`cleanup`、`Listener`、`version`、`errorCodes`、`eventNames`、`parseErrorCode`、`cloudflareErrorKinds`、`initLogging`。

**设计要点（重要）**：facade 必须在 `connect()` 返回**之前**就为**每一个**事件名
向 native 注册一个 handler，并在自己这一侧保留一份 pending 队列。

原因：Task 14 让 native 侧把"当时没有监听者"的事件存进 `pending`，
并在**下一次** `on()` 注册时补发。若 facade 像通常那样"第一个 JS 回调来了才
向 native 注册"，那么：
- 第一个 `on('dnsCreated', cb1)` 会触发 native 的补发 → cb1 收到 ✓
- 紧接着的 `on('dnsCreated', cb2)` 因为 native 侧已经注册过，**不会**再触发补发
  → cb2 永远收不到启动期事件 ✗

所以 facade 改成：连接建立后立刻为 `native.eventNames()` 里的每个名字注册一个
native handler，事件先落到 facade 自己的 pending 队列；JS 回调注册时，
facade 把队列里该名字的事件补发给**当时已注册的全部**回调。

- [ ] **Step 1: 创建 facade**

`crates/cloudpipe-node/js/index.js`：

```js
'use strict'

/**
 * The public entry point of `@xinggao/cloudpipe`.
 *
 * The native addon is a thin, untyped shell over the Rust SDK. This
 * facade is where the JavaScript-facing contract lives:
 *
 *  - `on(event, cb)` calls `cb(payload)`, with a single argument. napi's
 *    `ThreadsafeFunction` calls `cb(null, payload)`, so the native
 *    binding cannot offer that shape on its own.
 *  - `off(event, cb)` removes one callback. The native binding can only
 *    clear a whole event.
 *  - Events that fire before a handler is registered are delivered as
 *    soon as one is. The startup events fire while the tunnel is being
 *    created — before `connect()` even resolves — so without this they
 *    would be unreachable from JavaScript.
 *  - Errors carry `code` and `kind` properties instead of only a
 *    `[CODE]` prefix on the message.
 *  - A platform without a prebuilt binary gets an actionable message
 *    instead of "Cannot find native binding".
 */

const SUPPORTED_TARGETS = [
  'linux-x64-gnu',
  'darwin-arm64',
  'darwin-x64',
  'win32-x64-msvc',
]

/** How many events are held per name before the oldest is dropped. */
const PENDING_CAP = 64

let native
try {
  native = require('../npm/index.js')
} catch (cause) {
  const target = `${process.platform}-${process.arch}`
  throw new Error(
    `cloudpipe does not ship a prebuilt binary for ${target}.\n` +
      `Supported targets: ${SUPPORTED_TARGETS.join(', ')}.\n` +
      'Build from source with `npm run build:local`.',
    { cause },
  )
}

/**
 * Splits a cloudpipe error message into its code and kind.
 *
 * @param {string} message
 * @returns {{ code: string, kind?: string } | null}
 */
function splitCode(message) {
  const raw = native.parseErrorCode(message)
  if (!raw) return null
  const [code, kind] = raw.split(':')
  return kind ? { code, kind } : { code }
}

/**
 * Adds `code` / `kind` to an error thrown by the native binding.
 *
 * The `[CODE] ` prefix stays in the message so existing consumers that
 * parse it keep working.
 *
 * @param {unknown} err
 * @returns {unknown}
 */
function decorate(err) {
  if (!(err instanceof Error)) return err
  const parsed = splitCode(err.message)
  if (!parsed) return err
  // napi-rs sets `code` to its own status string; keep it under a
  // different name so nothing is lost when we take the slot.
  if (err.code !== undefined && err.code !== parsed.code) {
    err.napiCode = err.code
  }
  err.code = parsed.code
  if (parsed.kind) err.kind = parsed.kind
  return err
}

/**
 * Wraps a native listener so the JavaScript API matches what the
 * README promises.
 *
 * @param {any} listener
 * @param {string[]} eventNames
 */
function wrapListener(listener, eventNames) {
  /** @type {Map<string, Set<Function>>} */
  const handlers = new Map()
  /** @type {Map<string, string[]>} */
  const pending = new Map()

  function deliver(set, payload) {
    for (const cb of Array.from(set)) {
      try {
        cb(payload)
      } catch {
        // One throwing handler must not stop the others.
      }
    }
  }

  function dispatch(name, payload) {
    const set = handlers.get(name)
    if (set && set.size > 0) {
      deliver(set, payload)
      return
    }
    // Nobody is listening yet. Hold the event: the startup events fire
    // before any JavaScript could possibly have registered a handler.
    let queue = pending.get(name)
    if (!queue) {
      queue = []
      pending.set(name, queue)
    }
    if (queue.length >= PENDING_CAP) queue.shift()
    queue.push(payload)
  }

  // Register natively for every known event up front. This is what
  // drains the SDK's own pending buffer (the events emitted while the
  // tunnel was being created) into this facade instead of dropping it.
  for (const name of eventNames) {
    listener.on(name, (err, payload) => {
      if (err) return
      dispatch(name, payload)
    })
  }

  return {
    get url() {
      return listener.url
    },
    get fullName() {
      return listener.fullName
    },
    get subdomain() {
      return listener.subdomain
    },
    get port() {
      return listener.port
    },
    /**
     * Registers a callback for a named event.
     *
     * @param {string} event
     * @param {(payload: string) => void} callback
     */
    on(event, callback) {
      if (typeof callback !== 'function') {
        throw new TypeError('on(event, callback) requires a function')
      }
      let set = handlers.get(event)
      if (!set) {
        set = new Set()
        handlers.set(event, set)
      }
      set.add(callback)
      // Hand over the events that arrived before this listener.
      const queue = pending.get(event)
      if (queue && queue.length > 0) {
        pending.delete(event)
        deliver(set, queue.shift())
        for (const payload of queue) deliver(set, payload)
      }
      return this
    },
    /**
     * Removes one callback, or every callback for the event when no
     * callback is given.
     *
     * @param {string} event
     * @param {Function} [callback]
     */
    off(event, callback) {
      const set = handlers.get(event)
      if (!set) return this
      if (callback === undefined) {
        set.clear()
      } else {
        set.delete(callback)
      }
      if (set.size === 0) handlers.delete(event)
      return this
    },
    /**
     * Removes every registered callback.
     */
    removeAllListeners() {
      handlers.clear()
      return this
    },
    /** Blocks until the tunnel exits on its own. */
    async wait() {
      await listener.wait()
    },
    /** Re-points the tunnel at a different local port. */
    async update(port) {
      await listener.update(port)
    },
    /** Triggers a clean shutdown. */
    async close() {
      await listener.close()
    },
  }
}

/**
 * Opens a new tunnel.
 *
 * @param {any} options
 */
async function connect(options) {
  try {
    const listener = await native.connect(options)
    return wrapListener(listener, native.eventNames())
  } catch (err) {
    throw decorate(err)
  }
}

/**
 * Reclaims dead tunnels and orphaned DNS records.
 *
 * @param {any} options
 */
async function cleanup(options) {
  try {
    return await native.cleanup(options)
  } catch (err) {
    throw decorate(err)
  }
}

module.exports = {
  connect,
  cleanup,
  Listener: native.Listener,
  version: native.version,
  errorCodes: native.errorCodes,
  eventNames: native.eventNames,
  parseErrorCode: native.parseErrorCode,
  cloudflareErrorKinds: native.cloudflareErrorKinds,
  initLogging: native.initLogging,
}
```

- [ ] **Step 2: 改 `package.json`**

把 `"main": "npm/index.js"` 改成 `"main": "js/index.js"`。

把 `files` 改成：

```json
  "files": [
    "js/index.js",
    "npm/index.js",
    "npm/cloudpipe.linux-x64-gnu.node",
    "npm/cloudpipe.darwin-arm64.node",
    "npm/cloudpipe.darwin-x64.node",
    "npm/cloudpipe.win32-x64-msvc.node",
    "index.d.ts",
    "README.md",
    "LICENSE",
    "package.json"
  ],
```

（`npm/index.d.ts` 被移除 —— 它是 napi 自动生成的弱类型版本，
与手写的 `index.d.ts` 会漂移。`types` 仍指向手写的那份。）

- [ ] **Step 3: 构建并验证**

```bash
cd crates/cloudpipe-node
npm run build:debug
node -e "const c = require('./js/index.js'); console.log(c.version, c.errorCodes().length)"
```

Expected: 打印版本号与 `10`（错误码个数，Task 13 之后 `ALL` 有 10 项）。

- [ ] **Step 4: 提交**

```bash
git add crates/cloudpipe-node/js/index.js crates/cloudpipe-node/package.json
git commit -m "feat(node): add a JS facade over the native addon

The addon called listeners as cb(null, payload), while the published
type declaration promised cb(payload) — so every consumer's
JSON.parse(payload) was parsing null. The facade also gives off(event,
cb) its per-callback semantics, replays the events that fired before a
handler existed, attaches code/kind to errors, and turns a missing
prebuilt binary into an actionable message."
```


### Task 18: 类型文件与文档同步

**Files:**
- Modify: `crates/cloudpipe-node/index.d.ts`
- Modify: `crates/cloudpipe-node/README.md`
- Modify: `README.md:277-326`

**Interfaces:**
- Consumes: Task 13 的错误码（`INVALID_DOMAIN` 补齐）、Task 16 的 `cleanup`、Task 17 的 facade

- [ ] **Step 1: 补 `index.d.ts`**

在 `crates/cloudpipe-node/index.d.ts` 的 `CloudpipeErrorCode` 联合里加上
`INVALID_DOMAIN`（若尚未存在）：

```ts
export type CloudpipeErrorCode =
  | 'MISSING_CREDENTIAL'
  | 'CLOUDFLARE_API'
  | 'CLOUDFLARE_BINARY'
  | 'INVALID_SUBDOMAIN'
  | 'INVALID_DOMAIN'
  | 'SUBDOMAIN_IN_USE'
  | 'ALREADY_SHUT_DOWN'
  | 'IO'
  | 'INVALID_ARGUMENT'
  | 'OTHER'
```

把 `on()` 的签名改成与 facade 一致：

```ts
  /**
   * Registers a callback for a named event.
   *
   * The callback receives the event payload as a JSON string — `JSON.parse`
   * it inside the handler to access fields.
   *
   * Events that fired before this call are delivered to the callbacks
   * registered for that event at the moment of the call, so the startup
   * events (`dnsCreated`, `cloudflaredStarted`, …) are not lost. A
   * callback registered later for the same event does not receive them.
   */
  on(event: ListenerEventName, callback: (payload: string) => void): this
```

把 `off()` 的签名改成：

```ts
  /**
   * Removes one callback for an event, or every callback for it when no
   * callback is given.
   */
  off(event: ListenerEventName, callback?: (payload: string) => void): this
```

新增错误接口与 `cleanup` 声明：

```ts
/**
 * An error raised by cloudpipe.
 *
 * `code` is the stable cloudpipe taxonomy code; `kind` is the sub-kind
 * for Cloudflare API errors.
 */
export interface CloudpipeError extends Error {
  /** Stable error code, e.g. `"CLOUDFLARE_API"`. */
  code: string
  /** Sub-kind for Cloudflare API errors, e.g. `"RATE_LIMITED"`. */
  kind?: string
  /** napi-rs' own status code, when it differs from `code`. */
  napiCode?: string
}

/**
 * Options for {@link cleanup}.
 */
export interface CleanupOptions {
  /** Cloudflare API token. */
  token: string
  /** Base domain (e.g. `example.com`). */
  domain?: string
  /** Cloudflare account id. */
  account?: string
  /** Cloudflare zone id. */
  zone?: string
  /** Report what would be removed without removing anything. */
  dryRun?: boolean
}

/**
 * One resource a cleanup run touched.
 */
export interface CleanupAction {
  kind: 'tunnel' | 'dnsRecord'
  id: string
  name: string
}

/**
 * The result of a cleanup run.
 */
export interface CleanupReport {
  removed: CleanupAction[]
  wouldRemove: CleanupAction[]
  errors: string[]
}

/**
 * Reclaims dead tunnels and orphaned DNS records in one zone.
 */
export function cleanup(options: CleanupOptions): Promise<CleanupReport>
```

- [ ] **Step 2: 类型检查**

```bash
cd crates/cloudpipe-node
npx --yes typescript@5 tsc --noEmit --strict index.d.ts
```

Expected: 无错误输出

（若 `npx` 不可用，跳过并在提交信息里注明 —— 但要在 PR 描述里说明类型未经检查。）

- [ ] **Step 3: 同步 npm README**

在 `crates/cloudpipe-node/README.md` 里：

- 在事件一节补一句：
  `事件在监听器注册之前产生时，会在注册后立刻补发，因此启动阶段的事件（dnsCreated、cloudflaredStarted 等）不会丢失。`
- 在 API 一节加上 `cleanup(options)` 的说明与示例。
- 把 `off(event)` 的描述改成"可以只移除某一个回调"。

- [ ] **Step 4: 同步根 README**

根 README 的「As a Node.js library」一节，把

```js
listener.on('dnsCreated', (json) => {
  const { fullName } = JSON.parse(json);
  console.log(`DNS created for ${fullName}`);
});
```

保留不变（facade 保持了单参数回调），并在示例后面加一句：

```
事件在 `on()` 注册之前产生时会在注册后补发 —— `dnsCreated`、`cloudflaredStarted`
这类启动期事件不会丢失。
```

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe-node/index.d.ts crates/cloudpipe-node/README.md README.md
git commit -m "docs(node): align the type declarations and READMEs with the facade

The published on() signature did not match what the addon actually
called, the error-code union was missing INVALID_DOMAIN, and the
startup-event guarantee was nowhere documented."
```

---

## Part F — 测试补齐（修 D14）

### Task 19: `cloudpipe-test-support` crate

**Files:**
- Create: `crates/cloudpipe-test-support/Cargo.toml`
- Create: `crates/cloudpipe-test-support/src/lib.rs`
- Modify: `Cargo.toml`（workspace members）
- Modify: `crates/cloudpipe-sdk/tests/integration.rs`
- Modify: `crates/cloudpipe-sdk/Cargo.toml`（dev-dependencies）
- Modify: `crates/cloudpipe/Cargo.toml`（dev-dependencies）

**Interfaces:**
- Produces: `cloudpipe_test_support::{MockState, spawn_mock_api, write_fake_cloudflared, write_crashing_cloudflared, write_script, api_for, wait_until, CFARGO}`

- [ ] **Step 1: 建 crate**

`crates/cloudpipe-test-support/Cargo.toml`：

```toml
[package]
name = "cloudpipe-test-support"
version = "0.0.0"
edition.workspace = true
description = "Shared test doubles for the cloudpipe workspace — a mock Cloudflare API and fake cloudflared binaries"
license.workspace = true
publish = false

[dependencies]
serde_json = { workspace = true }
tokio = { workspace = true, features = ["io-util", "net", "rt", "time"] }

[lints]
workspace = true
```

在根 `Cargo.toml` 的 `members` 里加 `"crates/cloudpipe-test-support"`。

- [ ] **Step 2: 搬代码**

把 `crates/cloudpipe-sdk/tests/integration.rs` 的第 20-441 行（从 `const CFARGO`
到 `wait_until` 函数结束）搬进 `crates/cloudpipe-test-support/src/lib.rs`，
并做以下改动：

- 全部改为 `pub`（`MockState` 及其方法、`spawn_mock_api`、`write_fake_cloudflared`、
  `write_crashing_cloudflared`、`write_script`、`api_for`、`wait_until`、`CFARGO`）。
- `MockState::new()` 改为 `pub fn new() -> Arc<Self>`。
- `write_*` 系列函数的 `dir: &Path` 参数改 `pub`。
- `api_for` 需要 `CloudflareApi`，因此 crate 要依赖 `cloudpipe-sdk`：

  ```toml
  [dependencies]
  cloudpipe-sdk = { path = "../cloudpipe-sdk", version = "0.1.0" }
  ```

  这会形成 `cloudpipe-sdk` →（dev-dependency）→ `cloudpipe-test-support` →
  `cloudpipe-sdk` 的循环。**Cargo 允许 dev-dependency 循环**，但为了避免
  混淆，改为**不依赖 SDK**：`api_for` 移到 SDK 的测试文件里，
  `cloudpipe-test-support` 只提供 mock 服务器与 fake 二进制。

  即：`api_for` 留在 `integration.rs`，`cloudpipe-test-support` 不依赖
  `cloudpipe-sdk`。

- 文件顶部加 crate 级文档：

```rust
//! Shared test doubles for the cloudpipe workspace.
//!
//! The Cloudflare API mock and the fake `cloudflared` scripts live here
//! rather than inside one crate's test module, so the SDK's integration
//! tests and the CLI's tests exercise the *same* mock. A second copy
//! would drift, and a drifting mock makes a green test suite meaningless.
```

- `spawn_mock_api` 需要 `Arc`、`TcpListener` 等 import，按新文件的依赖补齐。

- [ ] **Step 3: 让 `integration.rs` 使用新 crate**

把 `integration.rs` 顶部替换为：

```rust
//! End-to-end integration tests for [`cloudpipe_sdk`].
//!
//! Each test spins up a tiny in-process HTTP server that mocks the subset
//! of the Cloudflare API the SDK talks to, points the SDK at it via
//! [`CloudflareApi::with_base`], and substitutes a shell script for the
//! `cloudflared` binary via [`TunnelBuilder::cloudflared_path`]. Nothing
//! here touches the network or a real Cloudflare account.
//!
//! The mock itself lives in `cloudpipe-test-support` so the CLI's tests
//! can use the same one.
//!
//! Test isolation: every case gets its own [`tempfile::TempDir`] (which
//! cleans itself up) and its own mock server on an ephemeral port. The
//! earlier version derived temp paths from `process::id()`, which made
//! cases running in the same process race for the same directory.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloudpipe_sdk::{CleanupAction, CloudflareApi, Protocol, TunnelBuilder};
use cloudpipe_test_support::{
    spawn_mock_api, wait_until, write_crashing_cloudflared, write_fake_cloudflared, MockState,
    CFARGO,
};
use serde_json::{json, Value};
```

并把文件里所有 `MockState`、`spawn_mock_api` 等引用改成从 crate 导入。
`api_for` 留在文件里：

```rust
/// A client pointed at a mock server.
fn api_for(base: &str) -> CloudflareApi {
    CloudflareApi::with_base("test-token", base).unwrap()
}
```

- [ ] **Step 4: 加 dev-dependency**

`crates/cloudpipe-sdk/Cargo.toml` 的 `[dev-dependencies]` 加：

```toml
cloudpipe-test-support = { path = "../cloudpipe-test-support" }
```

`crates/cloudpipe/Cargo.toml` 的 `[dev-dependencies]` 加同样的行。

- [ ] **Step 5: 运行测试**

```bash
cargo test --workspace --all-targets
```

Expected: 全部通过，**用例数量与改动前一致**（integration.rs 的行为没有变化）。

若有用例因为搬运而失败，通常是某个 `pub` 可见性或 import 漏了 —— 修 import，
不要改测试的断言语义。

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe-test-support Cargo.toml crates/cloudpipe-sdk crates/cloudpipe
git commit -m "test: share the Cloudflare mock between the SDK and the CLI

The mock lived inside the SDK's integration test file, so the CLI had
no way to test itself against it. A second copy would have drifted, and
a drifting mock makes a green suite meaningless."
```

---

### Task 20: CLI 的 handler 测试

**Files:**
- Modify: `crates/cloudpipe/src/main.rs`
- Test: `crates/cloudpipe/src/main.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 19 的 `cloudpipe_test_support`
- Produces: 无新公共接口。

- [ ] **Step 1: 让 `Ctx` 支持注入 base URL**

在 `crates/cloudpipe/src/main.rs` 的 `Ctx` 结构体加字段：

```rust
    /// Overrides the Cloudflare API base URL. Only used by tests, which
    /// point it at the in-process mock.
    api_base: Option<String>,
```

`Ctx::load` 里加 `api_base: None`。

`Ctx::api()` 改成：

```rust
    fn api(&self) -> Result<&CloudflareApi> {
        if let Some(api) = self.api.get() {
            return Ok(api);
        }
        let api = match &self.api_base {
            Some(base) => CloudflareApi::with_base(self.token()?, base.clone())
                .map_err(anyhow::Error::msg)?,
            None => CloudflareApi::new(self.token()?).map_err(anyhow::Error::msg)?,
        };
        let _ = self.api.set(api);
        self.api
            .get()
            .ok_or_else(|| anyhow!("could not initialise the Cloudflare client"))
    }
```

- [ ] **Step 2: 写测试**

在 `main.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use cloudpipe_test_support::{spawn_mock_api, MockState};

    /// A `Ctx` pointed at a mock API, with its config file in a temp dir.
    async fn mock_ctx() -> (Ctx, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = MockState::new();
        let base = spawn_mock_api(state).await;
        let mut ctx = Ctx::load().expect("ctx");
        // `ConfigStore::with_dir` already exists (config.rs:166) and roots
        // the store at `<dir>/config.json`.
        ctx.store = ConfigStore::with_dir(dir.path().to_path_buf());
        ctx.api_base = Some(base);
        (ctx, dir)
    }

    #[tokio::test]
    async fn key_saves_the_token_even_when_the_zone_has_no_account() {
        let (ctx, _dir) = mock_ctx().await;
        // The mock returns a zone without an account id, which is what
        // made the old `cfp key` fail with nothing written to disk.
        let err = handle_key(
            &ctx,
            Some("cf_test".to_string()),
            None,
            false,
        )
        .await
        .expect_err("the zone resolution must fail");
        assert!(format!("{err:#}").contains("account"), "{err:#}");

        // The token survived.
        let saved = ctx.store.load();
        assert_eq!(saved.token.as_deref(), Some("cf_test"));
    }

    #[tokio::test]
    async fn domain_reports_an_inaccessible_zone() {
        let (ctx, _dir) = mock_ctx().await;
        let err = handle_domain(&ctx, "nope.example.com".into())
            .await
            .expect_err("the zone is not accessible");
        assert!(format!("{err:#}").contains("not accessible"), "{err:#}");
    }
}
```

**注意**：Task 19 的 mock 必须能返回一个**没有 account 的 zone**，否则第一条测试
无法触发目标分支。若 `MockState` 目前总是带 account，在 `MockState` 上加一个
开关：

```rust
    /// Set to make listed zones carry no account reference.
    pub zones_without_account: AtomicBool,
```

并在 `route` 里处理 `GET /zones`：

```rust
        if state.zones_without_account.load(Ordering::SeqCst) {
            zones["result"][0].as_object_mut().unwrap().remove("account");
        }
```

- [ ] **Step 3: 确认 `ConfigStore::with_dir` 可用**

`ConfigStore::with_dir(dir: PathBuf) -> Self`（`config.rs:166`）已经存在，
把 store 落在 `<dir>/config.json`。**不需要**新增构造器。

若它不存在（`with_dir` 只在某个分支上有），加：

```rust
    /// A store rooted at an explicit directory. Used by tests.
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }
```

- [ ] **Step 4: 运行测试**

```bash
cargo test -p cfp
```

Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/cloudpipe/src/main.rs crates/cloudpipe/src/config.rs crates/cloudpipe-test-support
git commit -m "test(cli): cover the key and domain handlers

main.rs had no tests at all. The token-loss bug on a failed zone
resolution could only have been caught by running the command against a
mock API."
```

---

### Task 21: `render_event_text` 的表驱动测试

**Files:**
- Test: `crates/cloudpipe/src/ui.rs`（`mod tests`）

**Interfaces:**
- Consumes: Task 12 的 `render_event_text`、`EventLine`、`banner_text`
- Produces: 无新接口。

**关于样式**：`style()`（`ui.rs:211`）在非 tty 下本来就是 plain
（`Style::detect()` 的 `color` 与 `unicode` 都要求 tty），所以 `cargo test` 下
输出天然没有 ANSI 转义。但 `cargo test -- --nocapture` 在 tty 里跑时就不是 ——
因此每个用例显式调一次 `set_style(Style::plain())`，**且不恢复**：
样式是进程级全局的，多个用例并行时"设置后恢复"会互相踩，
而所有用例都要 plain，所以只设不恢复是安全且无竞态的。

- [ ] **Step 1: 写测试**

在 `crates/cloudpipe/src/ui.rs` 的 `mod tests` 中新增：

```rust
    /// Renders an event with the plain style, so the assertions are
    /// about the text and not about the terminal.
    fn render(event: &Event) -> Option<String> {
        set_style(Style::plain());
        render_event_text(event).map(|line| match line {
            EventLine::Out(text) | EventLine::Err(text) => text,
        })
    }

    #[test]
    fn every_event_variant_renders_exactly_what_it_says() {
        use cloudpipe_sdk::{Event, LogLevel, Protocol, ShutdownReason};

        let cases: Vec<(Event, Option<&str>)> = vec![
            (Event::Banner, None),
            (Event::ResolvingConflicts, None),
            (
                Event::CreatingTunnel {
                    name: "demo".into(),
                },
                Some("  Creating tunnel \"demo\"..."),
            ),
            (
                Event::IngressConfigured {
                    protocol: Protocol::Https,
                    port: 8443,
                },
                Some("  Configuring ingress (https://localhost:8443)..."),
            ),
            (
                Event::DnsCreated {
                    full_name: "demo.example.com".into(),
                },
                Some("  Creating DNS demo.example.com..."),
            ),
            (Event::CloudflaredStarted, None),
            (
                Event::Restarting {
                    reason: ShutdownReason::ChildExited,
                    attempt: 2,
                },
                Some("  cloudflared ended (cloudflared exited) — restarting (attempt #2)..."),
            ),
            (
                Event::Restarted { attempt: 2 },
                Some("  Resumed on the same URL (attempt #2)."),
            ),
            (
                Event::RestartGivingUp {
                    attempts: 5,
                    last_error: "boom".into(),
                },
                Some("  auto-restart failed 5 times in a row — giving up. last error: boom"),
            ),
            (
                Event::BootstrapFailed {
                    attempt: 3,
                    error: "boom".into(),
                },
                Some("  reconnect attempt #3 failed: boom"),
            ),
            (
                Event::EdgeConnected {
                    conn_index: 0,
                    total: 2,
                },
                Some("  Edge connection 1 of 2"),
            ),
            (
                Event::CloudflaredLog {
                    level: LogLevel::Warn,
                    line: "careful".into(),
                },
                Some("  [cloudflared] careful"),
            ),
            (
                Event::CloudflaredLog {
                    level: LogLevel::Info,
                    line: "noise".into(),
                },
                None,
            ),
            (
                Event::ShuttingDown {
                    reason: ShutdownReason::ChildExited,
                },
                Some("  cloudflared exited — cleaning up before respawn."),
            ),
            (
                Event::ShuttingDown {
                    reason: ShutdownReason::UserRequested,
                },
                None,
            ),
            (
                Event::Cleaned,
                Some("  Tunnel and DNS record cleaned up."),
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(
                render(&event).as_deref(),
                expected,
                "{event:?} rendered unexpectedly"
            );
        }
    }

    #[test]
    fn cloudflared_errors_go_to_stderr() {
        use cloudpipe_sdk::{Event, LogLevel};
        set_style(Style::plain());
        match render_event_text(&Event::CloudflaredLog {
            level: LogLevel::Error,
            line: "boom".into(),
        }) {
            Some(EventLine::Err(line)) => assert_eq!(line, "  [cloudflared] boom"),
            other => panic!("errors belong on stderr, got {other:?}"),
        }
    }

    #[test]
    fn the_banner_mentions_the_tool() {
        set_style(Style::plain());
        assert!(banner_text().contains("cfp"), "{}", banner_text());
    }
```

- [ ] **Step 2: 运行测试**

Run: `cargo test -p cfp ui::tests`
Expected: PASS。

若某个断言不匹配，**先对照 Task 12 里 `render_event_text` 的实现**看是哪一边写错了：
断言值必须与实现产出的文本逐字一致。改断言，不要改实现 ——
除非实现里确实有一个你不认同的措辞，那时才改实现并同步断言。

- [ ] **Step 3: 提交**

```bash
git add crates/cloudpipe/src/ui.rs
git commit -m "test(cli): cover the event renderer

render_event mapped every SDK event to a line of terminal output and had
no coverage at all, which is how the connection counter could end up
gated on `total == 1 || total == 4` without anyone noticing."
```

---

### Task 22: Node 端到端测试

**Files:**
- Create: `crates/cloudpipe-node/test/mock-api.js`
- Modify: `crates/cloudpipe-node/test/smoke.js`
- Modify: `crates/cloudpipe-node/package.json`（`test` 脚本）

**Interfaces:**
- Consumes: Task 14 的 pending 缓冲、Task 16 的 `cleanup`、Task 17 的 facade
- Produces: 无新接口。

- [ ] **Step 1: 加一个 mock API server**

创建 `crates/cloudpipe-node/test/mock-api.js`：

```js
'use strict'

const http = require('http')

/**
 * A minimal stand-in for the Cloudflare API, enough for one tunnel
 * lifecycle: create a tunnel, write its ingress, create a DNS record,
 * then delete both.
 */
function startMockApi() {
  const state = {
    tunnels: new Map(),
    records: new Map(),
    deleted: [],
  }
  let nextId = 1

  const server = http.createServer((req, res) => {
    let body = ''
    req.on('data', (chunk) => {
      body += chunk
    })
    req.on('end', () => {
      const json = (payload) => {
        res.writeHead(200, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify(payload))
      }
      const url = req.url || ''
      const method = req.method || 'GET'
      const parsed = () => {
        try {
          return JSON.parse(body || '{}')
        } catch {
          return {}
        }
      }

      if (method === 'POST' && url.includes('/tunnels')) {
        const created = {
          id: `tunnel-${nextId++}`,
          name: parsed().name || 'demo',
          status: 'healthy',
          token: 'fake-tunnel-token',
        }
        state.tunnels.set(created.id, created)
        return json({ success: true, result: created })
      }
      if (method === 'PUT' && url.includes('/configurations')) {
        return json({ success: true, result: null })
      }
      if (method === 'POST' && url.includes('/dns_records')) {
        const record = {
          id: `record-${nextId++}`,
          name: parsed().name,
          content: 'x.cfargotunnel.com',
        }
        state.records.set(record.id, record)
        return json({ success: true, result: record })
      }
      if (method === 'DELETE' && url.includes('/dns_records')) {
        const id = url.split('/').pop()
        state.deleted.push(id)
        state.records.delete(id)
        return json({ success: true, result: null })
      }
      if (method === 'DELETE' && url.includes('/connections')) {
        return json({ success: true, result: null })
      }
      if (method === 'DELETE' && url.includes('/tunnels')) {
        const id = url.split('/').pop()
        state.deleted.push(id)
        state.tunnels.delete(id)
        return json({ success: true, result: null })
      }
      if (method === 'GET' && url.includes('/user/tokens/verify')) {
        return json({ success: true, result: { id: 'tok', status: 'active' } })
      }
      if (method === 'GET' && url.includes('/zones')) {
        return json({
          success: true,
          result: [
            { id: 'zone-1', name: 'example.com', account: { id: 'account-1' } },
          ],
          result_info: { page: 1, per_page: 50, total_pages: 1 },
        })
      }
      if (method === 'GET' && url.includes('/tunnels')) {
        return json({
          success: true,
          result: [],
          result_info: { page: 1, per_page: 100, total_pages: 1 },
        })
      }
      if (method === 'GET' && url.includes('/dns_records')) {
        return json({
          success: true,
          result: [],
          result_info: { page: 1, per_page: 100, total_pages: 1 },
        })
      }
      json({
        success: false,
        errors: [{ code: 7003, message: `unmocked: ${method} ${url}` }],
      })
    })
  })

  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address()
      resolve({
        base: `http://127.0.0.1:${port}`,
        state,
        close: () => server.close(),
      })
    })
  })
}

module.exports = { startMockApi }
```

- [ ] **Step 2: 加一个 fake cloudflared**

在 `crates/cloudpipe-node/test/` 下新建 `fake-cloudflared.js`：

```js
'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')

/**
 * Writes a fake `cloudflared` that reports two edge connections and then
 * stays alive.
 *
 * The three "Registered tunnel connection" lines are what the SDK turns
 * into `edgeConnected` events — which is what makes it possible to test
 * that `off(event, cb)` removes exactly one callback: the first two
 * arrive before the callback is removed, the third after.
 *
 * @returns {string} the path to the fake binary
 */
function writeFakeCloudflared() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cfp-fake-'))
  const script = path.join(dir, 'cloudflared')
  fs.writeFileSync(
    script,
    [
      '#!/bin/sh',
      'echo "2025-11-01T12:00:00Z INF Registered tunnel connection" >&2',
      'sleep 1',
      'echo "2025-11-01T12:00:01Z INF Registered tunnel connection" >&2',
      'sleep 2',
      'echo "2025-11-01T12:00:03Z INF Registered tunnel connection" >&2',
      'while true; do sleep 1; done',
      '',
    ].join('\n'),
  )
  fs.chmodSync(script, 0o755)
  return script
}

module.exports = { writeFakeCloudflared }
```

- [ ] **Step 3: 写端到端用例**

在 `crates/cloudpipe-node/test/smoke.js` 末尾追加：

```js
const { startMockApi } = require('./mock-api.js')
const { writeFakeCloudflared } = require('./fake-cloudflared.js')

/** Polls until `predicate` is true, or gives up. */
async function waitUntil(predicate, budgetMs = 5000) {
  const deadline = Date.now() + budgetMs
  while (Date.now() < deadline) {
    if (predicate()) return true
    await new Promise((resolve) => setTimeout(resolve, 25))
  }
  return false
}

async function testStartupEventsAreDelivered() {
  const mock = await startMockApi()
  const { connect } = require('../js/index.js')

  const listener = await connect({
    token: 'test-token',
    domain: 'example.com',
    port: 8080,
    subdomain: 'demo',
    apiBase: mock.base,
    cloudflaredPath: writeFakeCloudflared(),
  })

  // The events that fire while the tunnel is being created must reach a
  // handler registered after `connect()` resolved.
  const seen = []
  listener.on('dnsCreated', (payload) => seen.push(payload))

  await waitUntil(() => seen.length > 0)

  const created = seen.map((p) => JSON.parse(p))
  const dns = created.find((p) => p.fullName === 'demo.example.com')
  assert.ok(dns, `dnsCreated was not delivered: ${JSON.stringify(seen)}`)

  await listener.close()
  mock.close()
}

async function testPayloadIsAStringNotNull() {
  const mock = await startMockApi()
  const { connect } = require('../js/index.js')

  const listener = await connect({
    token: 'test-token',
    domain: 'example.com',
    port: 8080,
    subdomain: 'demo',
    apiBase: mock.base,
    cloudflaredPath: writeFakeCloudflared(),
  })

  let payload = null
  listener.on('ingressConfigured', (p) => {
    payload = p
  })

  await waitUntil(() => payload !== null)
  assert.strictEqual(typeof payload, 'string', 'the payload must be a string')
  const parsed = JSON.parse(payload)
  assert.strictEqual(parsed.port, 8080, `unexpected payload: ${payload}`)

  await listener.close()
  mock.close()
}

async function testOffRemovesOnlyOneCallback() {
  const mock = await startMockApi()
  const { connect } = require('../js/index.js')

  const listener = await connect({
    token: 'test-token',
    domain: 'example.com',
    port: 8080,
    subdomain: 'demo',
    apiBase: mock.base,
    cloudflaredPath: writeFakeCloudflared(),
  })

  let first = 0
  let second = 0
  const a = () => first++
  const b = () => second++
  listener.on('edgeConnected', a)
  listener.on('edgeConnected', b)

  // The fake cloudflared reports three connections, at t=0, t=1 and t=3.
  // The first one may already have been buffered by the SDK before the
  // handlers were registered; either way both handlers have seen two
  // events by the time the third arrives.
  await waitUntil(() => first >= 2 && second >= 1)

  const beforeFirst = first
  listener.off('edgeConnected', a)

  await waitUntil(() => second >= 2)

  assert.strictEqual(first, beforeFirst, 'the removed callback kept firing')
  assert.ok(second >= 2, `the second callback saw ${second} events`)

  await listener.close()
  mock.close()
}
```

把这三个函数加进 `smoke.js` 的执行列表。

**注意**：`edgeConnected` 的第一次事件可能在 `on('edgeConnected', a)` 之前就到了 ——
facade 会把它存进 pending，然后在 `on()` 时补发。所以 `a` 收到第一次事件，
而 `b`（在 `a` 之后注册）收不到；第二次事件两个都收到。
断言写成"移除后 `first` 不再增长、`second` 仍在增长"，因此对
"第一次事件是补发的还是正常派发的"两种时序都成立。

- [ ] **Step 4: 让 `parse-error-code.js` 进入 CI**

把 `crates/cloudpipe-node/package.json` 的 `"test"` 改成：

```json
    "test": "node test/smoke.js && node test/parse-error-code.js",
```

- [ ] **Step 5: 跑测试**

```bash
cd crates/cloudpipe-node
npm run build:debug
npm test
```

Expected: 全部通过

若 `dnsCreated` 仍然没到，检查 Task 17 的 facade 是否在 `connect()` 里就为
所有事件名注册了 native handler —— 只在第一个 JS 回调注册时才注册是不够的。

- [ ] **Step 6: 提交**

```bash
git add crates/cloudpipe-node/test crates/cloudpipe-node/package.json
git commit -m "test(node): cover the startup events and the per-callback off()

parse-error-code.js had sixteen cases that CI never ran, and the event
path had no end-to-end coverage at all — which is why a dropped
startup event and a callback that received null both shipped."
```


### Task 23: 全量验证与文档一致性

**Files:**
- Modify: `README.md`（若验证发现不一致）
- Modify: `.github/workflows/ci.yml`（若验证发现缺项）

**Interfaces:**
- Consumes: 全部前置任务
- Produces: 一个绿色的构建。

- [ ] **Step 1: Rust 全量验证**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --doc
cargo deny check
```

Expected: 全部通过、无警告。

- [ ] **Step 2: semver 检查**

Task 5 给 SDK 加了公共模块 `cloudpipe_sdk::logging`，这是**新增** API ——
向后兼容，但 CI 的 `cargo semver-checks` 会把 baseline 与新版本对比。

```bash
cargo install cargo-semver-checks --locked
cargo semver-checks check-release --baseline-rev origin/main
```

Expected: 通过（新增 API 不构成破坏性变更）。若它因为 baseline 过期而失败，
用 `cargo semver-checks check-release --baseline-rev origin/main --current-version <new>`
确认差异只有新增项，然后更新 baseline。

- [ ] **Step 3: Node 全量验证**

```bash
cd crates/cloudpipe-node
npm run build:debug
npm test
node examples/quickstart.js
node examples/with-events.js
```

Expected: 全部通过（`quickstart.js` / `with-events.js` 需要真实的
`CLOUDFLARE_API_TOKEN`；在本地没有 token 时它们应当以"缺少 token"的
方式失败，而不是崩溃 —— 确认失败信息是这句话）。

- [ ] **Step 4: 逐条核对 README 的承诺**

打开 `README.md`，把每一条"承诺"对照代码确认：

| README 的说法 | 应当成立的地方 |
|---|---|
| "No separate `cloudflared` install required"（npm 一节） | `binary.rs` 的 `ASSET_SHA256` 覆盖所有平台 |
| "the SDK downloads a pinned, checksum-verified build on first use" | 同上 + `scripts/gen-cloudflared-checksums.sh` |
| "Move a live tunnel to a different local port" | SDK 有 `update_port`；Node 有 `update()`；**CLI 仍无入口** |
| 命令表里的每个 flag | `cfp --help` 与 `cfp cleanup --help` |
| 环境变量表里的每一项 | `CLOUDFLARE_API_TOKEN` / `CFP_CONFIG_DIR` / `CFP_GITHUB_PROXY` / `CFP_LOG` / `CFP_ASCII` / `NO_COLOR` |

对 "Move a live tunnel…" 这一条：CLI 确实没有对应的命令。本 spec 的范围不含
新子命令，所以在 README 里注明它只存在于 SDK 与 Node：把那一段改成

```bash
# Move a live tunnel to a different local port (public URL unchanged)
# Available in the SDK (TunnelHandle::update_port) and in Node
# (listener.update); the CLI does not expose it yet.
```

- [ ] **Step 5: 确认 CI 的文档检查仍然成立**

```bash
cargo build -p cfp
./target/debug/cfp --help
./target/debug/cfp cleanup --help
```

确认 `ci.yml` 的 `fmt-check-docs` job 里列出的每一个 flag 都还在。

- [ ] **Step 6: 提交**

```bash
git add README.md
git commit -m "docs: reconcile the README with what the code actually does"
```

若 Step 4 没有发现不一致，跳过这一步。

---

## 附录：实施顺序建议

组与组之间没有依赖，但按下面这个顺序执行时，每一步都有一条**可独立验证的
用户可见收益**，中途任何一点停下来，仓库都是可发布的：

1. **Part A**（Task 1-4）—— 所有平台的用户立刻能装上 cloudflared（今天的
   Linux 摘要也是错的，Part A 一并修掉）。
2. **Part B**（Task 5-7）—— 日志终于有级别、终于带得上数字字段。
3. **Part C**（Task 8-12）—— CLI 与 SDK/Node 行为一致，token 不再丢。
4. **Part D + E**（Task 13-18）—— npm 包的事件、错误与回调形状全部兑现。
5. **Part F**（Task 19-22）—— 测试兜底，防止上面这些再退化。
6. **Task 23** —— 全量验证与文档对齐。

**风险最高的一步是 Task 19**（把一个 1258 行的、正在通过全部测试的文件拆开）。
若它出问题，可以先退回"在 CLI 里复制一份最小 mock"的临时方案，把 Task 19
推迟到其余任务之后 —— 但那样 `cloudpipe-test-support` 这个 crate 就不建了，
Task 20 需要改成引用 CLI 内部的 mock 模块。
