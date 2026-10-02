# CloudNProxy

把**百度 T5 云代理节点**转换成**本地标准 SOCKS5 代理**。提供两种形态：

| 形态 | 二进制 | 说明 |
|---|---|---|
| **图形界面版** | `cloudnproxy` | Windows 11 / Linux 桌面，五页界面 + 托盘 |
| **无 GUI 版** | `t5d` | 纯 Rust，**不依赖 WebView 与图形环境**，适合服务器 / 容器 / 最小化发行版 |

> 早期 PowerShell 脚本的原理分析与完整时序图见 [`READ.md`](./READ.md)；界面设计稿见 [`ui-prototype.html`](./ui-prototype.html)。

---

## 它解决什么问题

百度 T5 节点（如 `163.177.17.189:443`）本质是 HTTP 正向代理，但有两个私有约定：

1. `CONNECT` 请求里的 `Host` 头必须写成 `cloudnproxy.baidu.com`，不能是真实目标域名；
2. 必须携带 `X-T5-Auth: <凭证>`。

因此浏览器、`curl` 等标准客户端**无法直接使用**。本应用在本地做协议翻译：

```
本地应用 ──SOCKS5──► CloudNProxy ──改写后的 CONNECT──► T5 节点 ──► 目标站点
                     127.0.0.1:10801     Host / X-T5-Auth
                            │
                            └─（可选）一级 HTTP 代理 Chain
```

---

## 功能

| 模块 | 说明 |
|---|---|
| **代理引擎** | 本地 SOCKS5 入站，仅支持 CONNECT；逐连接建立上游隧道，双向转发 |
| **断线重连** | 上游建立失败时按 300ms/600ms 退避重试，最多 3 次 |
| **自动切换** | 上游连续失败 3 次后，自动改选节点库中评分最高的可用节点（速度优先、其次延迟） |
| **隧道池** | 对同一目标预建一条备用隧道；下次同目标连接直接复用，省掉一次握手往返。只缓存**未传数据**的干净隧道 |
| **节点库** | 从指定域名（默认 `cloudnproxy.baidu.com`）解析节点，多 DoH 源并发取并集 |
| **测速** | 单节点 / 全部测速，输出延迟、下行带宽、出口 IP 与归属地、运营商、ASN |
| **日志** | 分级（trace/debug/info/warn/error）；内存环形缓冲 + 可选文件落盘；GUI 版只在窗口显示 |
| **托盘** | 显示窗口、启动/暂停、测速选优、复制 SOCKS5 地址、退出 |
| **自启** | Windows 注册表 Run 值 · Linux XDG autostart |
| **持久化** | 配置与测速结果写入 `config.toml` |
| **节点热切换** | 切换当前节点不重启引擎、不断开已有连接 |

---

## 无 GUI 版本（`t5d`）

```bash
t5d -f /etc/cloudnproxy/config.toml -log debug
```

| 参数 | 说明 |
|---|---|
| `-f, --file <路径>` | 配置文件路径。省略时按平台标准目录查找；文件不存在会自动生成默认配置 |
| `-log, --log <级别>` | **标准输出**日志级别：`trace` / `debug` / `info` / `warn` / `error`，覆盖配置里的 `log_level` |
| `-h, --help` | 帮助 |
| `-v, --version` | 版本 |

**日志去向**（两者独立）：

- **标准输出**：始终输出，级别由 `-log` 或配置的 `log_level` 决定；
- **日志文件**：由配置项 `log_file` 控制，**留空则不写文件**。

其余全部可调项都在 `config.toml` 中。收到 `SIGTERM` / `SIGINT` 会优雅退出。

### systemd 示例

```ini
# /etc/systemd/system/cloudnproxy.service
[Unit]
Description=CloudNProxy T5 SOCKS5 gateway
After=network-online.target

[Service]
ExecStart=/usr/local/bin/t5d -f /etc/cloudnproxy/config.toml -log info
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

---

## 配置文件

两版共用同一位置，方便互相切换：

- Windows：`%APPDATA%\dev.cloudnproxy.app\config.toml`
- Linux：`$XDG_CONFIG_HOME/dev.cloudnproxy.app/config.toml`（默认 `~/.config/dev.cloudnproxy.app/config.toml`）

主要字段：

```toml
# ---- 入站 ----
listen_host = "127.0.0.1"
listen_port = 10801
allow_lan = false              # 开启后监听 0.0.0.0

# ---- 上游 T5 ----
resolve_domain = "cloudnproxy.baidu.com"
upstream = "163.177.17.189:443"
current_node = "163.177.17.189:443"
fake_host = "cloudnproxy.baidu.com"
t5_auth = "1050504963"
max_conns = 512                # 0 表示不限

# ---- Chain 一级代理（默认直连）----
chain_enabled = false
chain_addr = ""

# ---- 转发行为 ----
connect_timeout_ms = 10000
tcp_nodelay = true
tunnel_pool = true
auto_reconnect = true
auto_switch = false

# ---- 日志 ----
log_level = "info"
log_file = ""                  # 留空 = 不写文件

# ---- 应用行为 ----
autostart = false
autostart_connect = true
start_minimized = true
close_to_tray = true
theme = "system"

# ---- 节点库（由「解析域名」+「测速」自动维护）----
[[nodes]]
ip = "14.215.182.75"
port = 443
speed_mbps = 127.8
latency_ms = 28
# ...
```

---

## 获取可执行文件

**本地无需 Rust 环境**——编译全部在 GitHub Actions 完成。推送后工作流按以下顺序执行：

1. `check` —— `t5-core` / `t5-daemon` 单元测试 + `cargo check --workspace`
2. `smoke` —— 真正启动 `t5d`，校验端口秒级就绪、标准输出、日志文件、SIGTERM 优雅退出，并对代理链路做软检查
3. `build` —— Windows（NSIS）与 Linux（AppImage / deb）
4. `musl` —— 静态链接版 `t5d-musl`，并在 **alpine 容器**中实测可运行
5. `release` —— 打 tag（如 `v0.1.1`）时自动发布 Release

产物 artifact：

| Artifact | 内容 |
|---|---|
| `cloudnproxy-windows-x64` | NSIS 安装包、`cloudnproxy.exe`、`t5d.exe` |
| `cloudnproxy-linux-x64` | `.AppImage`、`.deb`、`cloudnproxy`、`t5d` |
| `cloudnproxy-linux-musl` | `t5d-musl` |
| `daemon-smoke-logs` | 冒烟测试的 stdout 与日志文件 |

---

## 发行版文件说明

| 文件 | 平台 | 体积 | 说明 |
|---|---|---|---|
| `CloudNProxy_x.y.z_x64-setup.exe` | Windows | ~1.9 MB | NSIS 安装包。含开始菜单项、卸载程序 |
| `cloudnproxy.exe` | Windows | ~4.9 MB | 免安装绿色版，双击即可运行 |
| `t5d.exe` | Windows | ~2.0 MB | 无 GUI 版本，命令行工具 |
| `CloudNProxy_x.y.z_amd64.deb` | Linux | ~2.8 MB | Debian / Ubuntu 安装包。**webkit 等依赖由 apt 提供**，所以体积小 |
| `CloudNProxy_x.y.z_amd64.AppImage` | Linux | ~78 MB | 便携版。把 WebKit / GTK 整套运行时打包进文件，拷到任意发行版直接运行，无需安装任何依赖 |
| `cloudnproxy` | Linux | ~6.2 MB | 图形界面版裸二进制（要求系统已装 `libwebkit2gtk-4.1-0`） |
| `t5d` | Linux | ~2.4 MB | 无 GUI 版裸二进制，**动态链接 glibc** |
| `t5d-musl` | Linux | ~2.5 MB | 无 GUI 版**静态链接**，零动态依赖 |

### 怎么选

| 你的场景 | 选它 | 原因 |
|---|---|---|
| Windows 用户 | `...-setup.exe` | 有安装流程与卸载入口 |
| Linux 桌面，要图形界面 | `.deb` | 体积最小，依赖交给系统包管理器 |
| Linux 桌面，不想装东西 | `.AppImage` | `chmod +x` 后双击即用，代价是 78 MB |
| **Linux 服务器 / 容器 / Alpine** | **`t5d-musl`** | 一个文件拷过去就能跑，不需要 glibc、不需要任何依赖 |
| 发行版里已有 webkit | `t5d` / `cloudnproxy` | 直接运行，省去打包开销 |

### `t5d` 与 `t5d-musl` 的区别

两者是**同一个程序的两种编译方式**，功能、参数、配置文件完全一致，区别只在链接方式：

| | `t5d`（动态链接 gnu/glibc） | `t5d-musl`（静态链接 musl） |
|---|---|---|
| 体积 | 2.38 MB | 2.50 MB |
| `ldd` 结果 | 列出 `libc.so.6` 等一串依赖 | `statically linked`（`static-pie`） |
| 能否跑在 Alpine / BusyBox | ✗（musl 与 glibc 不兼容） | ✓ |
| 能否跑在老发行版（CentOS 7 等） | ✗ 常报 `GLIBC_2.xx not found` | ✓ |
| DNS 行为 | 走 glibc NSS，查找链完整 | musl 自带 resolver，只读 `/etc/resolv.conf`（本项目节点解析以 DoH 为主，影响可忽略） |

### 为什么 AppImage 比 deb 大 28 倍

两者承诺不同：**deb** 只声明"我依赖 webkit2gtk"，库由系统提供；**AppImage** 承诺"拷到任何机器直接跑"，所以把整套图形栈塞进文件 —— `libjavascriptcoregtk`（JS 引擎）、`libwebkit2gtk`、GTK3 全家桶、ICU 数据、GStreamer 核心库等。这部分约占 78 MB 中的 90%+，本项目自己的代码只有约 6 MB。

作为对照：Windows 端只有 1.9–4.9 MB，因为 Windows 11 系统自带 WebView2，不需要打包浏览器内核。

---

## 自行编译

| 平台 | 依赖 |
|---|---|
| Windows 11 | Visual Studio 2022 Build Tools（C++ 桌面开发 + Windows SDK）；WebView2 已内置 |
| Linux（GUI） | `libwebkit2gtk-4.1-dev`、`libayatana-appindicator3-dev`、`librsvg2-dev`、`patchelf`、`libgtk-3-dev`、`build-essential` |
| Linux（仅 `t5d`） | 只需要 `build-essential`，**不需要任何图形库** |

```bash
# 单元测试（不需要图形依赖）
cargo test -p t5-core --lib
cargo test -p t5-daemon

# 只编译无 GUI 版本
cargo build --release -p t5-daemon      # 产物 target/release/t5d

# 静态链接版（零动态依赖，可直接跑在 Alpine / BusyBox）
# Ubuntu/Debian 还需先：sudo apt install musl-tools cmake nasm perl
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p t5-daemon
# 产物 target/x86_64-unknown-linux-musl/release/t5d

# 图形界面版：先生成图标
node scripts/gen-icon.js icon-source.png
npm install -g "@tauri-apps/cli@^2"
tauri icon icon-source.png -o src-tauri/icons
tauri dev
tauri build --bundles nsis              # Windows
tauri build --bundles appimage,deb      # Linux
```

---

## 使用

### 图形界面版

1. 启动应用，托盘出现图标；
2. **节点库**页输入域名 → 点「解析域名」；
3. 点「全部测速」，等待结果（每节点 1–30 秒）；
4. 在最优节点行点「设为当前」（热切换，无需重启）；
5. **连接**页确认 FakeHost / T5Auth；
6. **概览**页打开总开关；
7. 客户端配置 `socks5://127.0.0.1:10801`，域名交给代理解析（`curl --socks5-hostname`）。

节点表格的「延迟」「速度」表头可点击切换升降序；右上角下拉可按运营商过滤。

### 无 GUI 版

```bash
t5d -f ./config.toml -log info
# 另一个终端
curl --socks5-hostname 127.0.0.1:10801 http://ip-api.com/json/
```

---

## 项目结构

```
cloudnproxy/
├── Cargo.toml                       # workspace
├── crates/t5-core/                  # 转发引擎（可独立编译、含单元测试）
│   └── src/{config,logbuf,stats,socks5,outbound,engine,resolver,bench,tunnel_pool}.rs
├── crates/t5-daemon/                # 无 GUI 版本 → t5d
├── src-tauri/                       # 图形界面版
│   ├── src/{lib,main,state,commands,tray}.rs
│   └── {tauri.conf.json, capabilities/default.json}
├── ui/                              # 前端（静态 HTML/CSS/JS，无打包器）
├── scripts/gen-icon.js              # 生成图标源 PNG（零依赖）
├── .github/workflows/build.yml
├── README.md                        # 本文件
├── READ.md                          # 早期 PowerShell 实现的原理分析
└── ui-prototype.html                # 界面设计稿（浏览器可直接打开预览）
```

---

## 已知限制

| 项 | 说明 |
|---|---|
| 仅 TCP | 未实现 SOCKS5 的 UDP ASSOCIATE，QUIC / UDP 流量不可代理 |
| 无入站认证 | SOCKS5 握手不校验凭据；默认只监听 `127.0.0.1`，开启 `allow_lan` 前请自行评估风险 |
| 隧道池粒度 | 仅按「上游节点 + 目标」缓存，且只缓存未传数据的干净隧道，命中率取决于访问目标是否集中 |
| 自动切换依据 | 使用节点库中最近一次测速结果评分；未测速的节点不参与 |
| 日志时间戳 | 文件与标准输出使用 **UTC** 时间 |
| 第三方接口 | GeoIP 用 `ip-api.com`、测速用 `speed.cloudflare.com`，不可达时对应列显示「—」 |

---

## 合规声明

本项目是网络协议转换与链路测量的技术实现。请仅在自己拥有管理权限、或已获得网络运营方明确授权的网络中使用，并遵守所在国家/地区的法律法规。`t5_auth` 属于个人凭证，请勿提交到公开仓库或分享给他人。
