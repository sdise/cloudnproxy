# CloudNProxy

把**百度 T5 云代理节点**转换成**本地标准 SOCKS5 代理**的桌面应用。

> 本文件是 Rust + Tauri v2 工程的使用说明。
> 早期 PowerShell 脚本的原理分析与完整时序图见 [`READ.md`](./READ.md)；
> 界面设计稿见 [`ui-prototype.html`](./ui-prototype.html)。

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
| **节点库** | 从指定域名（默认 `cloudnproxy.baidu.com`）解析节点，多 DoH 源并发取并集 |
| **测速** | 单节点/全部测速，输出延迟、下行带宽、出口 IP 与归属、运营商、ASN |
| **连接配置** | 监听地址端口、上游节点、FakeHost、T5Auth、Chain、并发上限、超时 |
| **日志** | 内存环形缓冲（2000 条），只在窗口内显示，不写入文件 |
| **托盘** | 显示窗口、启动/暂停、测速选优、复制 SOCKS5 地址、退出 |
| **自启** | Windows 注册表 Run 值 · Linux XDG autostart |
| **持久化** | 配置与测速结果写入 `config.toml`；日志不落盘 |

界面共五页：**概览 · 节点库 · 连接 · 日志 · 设置**。

---

## 技术栈

| 层 | 选型 |
|---|---|
| 引擎 | Rust + `tokio`（异步 TCP、双向转发、原子统计） |
| 解析 | `reqwest`（rustls）并发查询阿里 / 腾讯 DoH，系统解析兜底 |
| 外壳 | **Tauri v2**（系统 WebView，Windows 11 / Linux 桌面） |
| 前端 | 原生静态 HTML/CSS/JS（无打包器，`withGlobalTauri` 直连 IPC） |
| 构建 | GitHub Actions，产出 NSIS 安装包 / AppImage / deb |

仓库结构：

```
cloudnproxy/
├── Cargo.toml                  # workspace
├── crates/t5-core/             # 转发引擎（可独立编译、含单元测试）
│   └── src/{config,logbuf,stats,socks5,outbound,engine,resolver,bench}.rs
├── src-tauri/                  # Tauri 应用层
│   ├── src/{lib,main,state,commands,tray}.rs
│   ├── tauri.conf.json
│   └── capabilities/default.json
├── ui/                         # 前端（静态）
│   ├── index.html  style.css  app.js
├── scripts/gen-icon.js         # 生成图标源 PNG（零依赖）
├── .github/workflows/build.yml # 双平台构建
├── README.md                   # 本文件
├── READ.md                     # 早期 PowerShell 实现的原理分析
└── ui-prototype.html           # 界面设计稿（可点击预览）
```

---

## 获取可执行文件

**本地不需要 Rust 环境**——编译全部在 GitHub Actions 完成：

1. 推送代码或手动触发 `build` 工作流；
2. 在 Actions 页面下载 artifact：
   - `cloudnproxy-windows-x64` → NSIS 安装包 / 裸 exe
   - `cloudnproxy-linux-x64` → `.AppImage` / `.deb`
3. 打 tag（如 `v1.0.0`）会自动创建 GitHub Release 并附带全部产物。

Linux 使用 AppImage：`chmod +x CloudNProxy_*.AppImage && ./CloudNProxy_*.AppImage`
（若托盘图标不显示，需安装 `libayatana-appindicator3-1`）

---

## 自行编译

需要 Rust 工具链与平台依赖：

| 平台 | 依赖 |
|---|---|
| Windows 11 | Visual Studio 2022 Build Tools（C++ 桌面开发 + Windows SDK）；WebView2 已内置 |
| Linux | `libwebkit2gtk-4.1-dev`、`libayatana-appindicator3-dev`、`librsvg2-dev`、`patchelf`、`libgtk-3-dev`、`build-essential` |

```bash
# 运行单元测试（不需要 WebView 依赖）
cargo test -p t5-core --lib

# 生成图标（首次）
node scripts/gen-icon.js icon-source.png
npm install -g "@tauri-apps/cli@^2"
tauri icon icon-source.png -o src-tauri/icons

# 开发 / 打包
tauri dev
tauri build --bundles nsis          # Windows
tauri build --bundles appimage,deb  # Linux
```

---

## 使用

1. 启动应用，托盘出现图标；
2. 打开**节点库**页，输入域名（默认 `cloudnproxy.baidu.com`）→ 点「解析域名」；
3. 点「全部测速」，等待结果（每个节点约 1–30 秒）；
4. 在最优节点行点「设为当前」，或在**连接**页设置 FakeHost / T5Auth；
5. 回**概览**页打开总开关；
6. 客户端配置 `socks5://127.0.0.1:10801`，域名解析交给代理（`curl --socks5-hostname`）。

### 配置项

配置文件位置：

- Windows：`%APPDATA%\dev.cloudnproxy.app\config.toml`
- Linux：`~/.config/dev.cloudnproxy.app/config.toml`

关键字段：

```toml
listen_host = "127.0.0.1"
listen_port = 10801
resolve_domain = "cloudnproxy.baidu.com"
upstream = "163.177.17.189:443"   # 当前节点
fake_host = "cloudnproxy.baidu.com"
t5_auth = "1050504963"
chain_enabled = false             # 默认直连节点
chain_addr = ""
max_conns = 512
connect_timeout_ms = 10000
```

---

## 已知限制

| 项 | 说明 |
|---|---|
| 仅 TCP | 未实现 SOCKS5 的 UDP ASSOCIATE，QUIC / UDP 流量不可代理 |
| 无认证 | 入站不校验凭据；监听默认绑定 `127.0.0.1`，开启局域网前请自行评估风险 |
| 上游预连接池 | `tunnel_pool` 开关已预留，当前版本尚未生效 |
| 断线重连 / 自动切换 | 同上，界面中已标注为预留 |
| 依赖第三方接口 | GeoIP 使用 `ip-api.com`、测速使用 `speed.cloudflare.com`，不可达时对应列显示「—」 |

---

## 合规声明

本项目是网络协议转换与链路测量的技术实现。请仅在自己拥有管理权限、或已获得网络运营方明确授权的网络中使用，并遵守所在国家/地区的法律法规。`t5_auth` 属于个人凭证，请勿提交到公开仓库或分享给他人。
