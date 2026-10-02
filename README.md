# CloudNProxy

把**百度 T5 云代理节点**转换成**本地标准 SOCKS5 代理**。提供两种二进制、三种使用形态：

| 形态 | 二进制 | 说明 |
|---|---|---|
| **图形界面版** | `cloudnproxy` | Windows 11 / Linux 桌面，五页界面 + 托盘 |
| **无 GUI 版** | `t5d` | 纯 Rust，**不依赖 WebView 与图形环境**，适合服务器 / 容器 / 最小化发行版 |
| **Web 控制台版** | 同上 `t5d` | 在浏览器里管理，界面与桌面版**完全一致**。不是独立二进制：`t5d` 内建了它，配置里打开即可用（默认关闭） |

三者共用同一套 `ui/` 界面资源与同一份业务逻辑，详见下文「[Web 控制台](#web-控制台在浏览器里管理)」章节。

> 界面设计稿见 [`ui-prototype.html`](./ui-prototype.html)，浏览器直接打开即可点击预览。
> 本项目的前身是一套 9 个文件的 PowerShell 脚本，其协议原理、完整交互时序与历史测速数据已并入本文档（见「协议原理」与「节点选型参考」）。

---

## 协议原理

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

### 一次连接的完整时序

```text
1. SOCKS5 握手
   客户端 → 05 <nmethods> <methods…>
   服务端 ← 05 00                          无条件「无需认证」

2. 请求解析（仅 CONNECT，cmd=0x01）
   VER CMD RSV ATYP | 地址 | 端口(2 字节大端)
   · ATYP=1  IPv4，4 字节
   · ATYP=3  1 字节长度 + 域名
   · ATYP=4  IPv6，16 字节
   · CMD≠1    → rep=7（命令不支持）
   · ATYP 非法 → rep=8（地址类型不支持）

3. 建立到上游的 TCP 连接
   · 启用 Chain 时：连 Chain → 发 CONNECT <上游节点> → 要求响应含「 200 」
   · 否则直连上游节点

4. 发送改写后的 CONNECT（关键一步）
   CONNECT <真实目标>:<端口> HTTP/1.1
   Host: cloudnproxy.baidu.com        ← 伪 Host，不是真实域名
   X-T5-Auth: <凭证>
   Proxy-Connection: Keep-Alive

5. 读取上游响应头
   · 含「 200 」→ 向客户端回 05 00 00 01 0.0.0.0:0（成功）
   · 否则       → 回 rep=5（连接被拒），日志记录 DENIED 与首行原因

6. 双向转发
   两个方向各由独立任务搬运；任一端断开即关闭两侧。
```

### 几个设计取舍

| 取舍 | 原因 |
|---|---|
| 每条 SOCKS 连接对应一条独立的上游 TCP | 隔离性最好，单条连接异常不会波及其它连接 |
| 握手阶段 10 秒超时，转发阶段不超时 | 防止半开连接吊住任务；CONNECT 建立后，长连接与大文件下载不应被误断 |
| 按 `\r\n\r\n` 界定读响应头 | 隧道响应没有可靠的 `Content-Length`，不能按长度读 |
| `TCP_NODELAY` | 关闭 Nagle，降低交互式流量的延迟 |

---

## 功能

| 模块 | 说明 |
|---|---|
| **代理引擎** | 本地 SOCKS5 入站，仅支持 CONNECT；逐连接建立上游隧道，双向转发 |
| **断线重连** | 上游建立失败时按 300ms/600ms 退避重试，最多 3 次 |
| **自动切换** | 上游连续失败 3 次后，自动改选节点库中评分最高的可用节点（速度优先、其次延迟） |
| **隧道池** | 对同一目标预建一条备用隧道；下次同目标连接直接复用，省掉一次握手往返。只缓存**未传数据**的干净隧道 |
| **节点库** | 从指定域名（默认 `cloudnproxy.baidu.com`）解析节点：多源 DoH **+ 多地域 ECS 查询**取并集，并过滤保留地址（详见下节） |
| **测速** | 经隧道 **HTTPS 真实下载**实测带宽，同时给出 CONNECT 延迟、出口 IP 与归属地、运营商、ASN |
| **测速链接可配置** | 界面「节点库」页提供下拉选择 + 自定义输入，可保存多个链接并随时切换 |
| **日志** | 分级（trace/debug/info/warn/error）；内存环形缓冲 + 可选文件落盘；GUI 版只在窗口显示 |
| **出站网卡绑定** | 概览页可指定本程序从哪张网卡出去，**内建绕过 TUN**，无需修改代理软件的设置 |
| **出站路径提示** | 显示到上游节点的实际出站网卡，并提示流量是否被 TUN 模式的代理软件接管 |
| **托盘** | 显示窗口、启动/暂停、测速选优、复制 SOCKS5 地址、退出 |
| **Web 控制台** | 无 GUI 版可在浏览器里操作（默认关闭）：与桌面版**同一套界面**，JWT 登录，首次登录强制改密 |
| **自启** | Windows 注册表 Run 值 · Linux XDG autostart |
| **持久化** | 配置与测速结果写入 `config.toml` |
| **节点热切换** | 切换当前节点不重启引擎、不断开已有连接 |

### 节点解析：为什么需要多地域查询

`cloudnproxy.baidu.com` 是 **GeoDNS 域名**：权威 DNS 按**查询来源**返回就近节点，因此**任何一个解析视角最多只能看到 1~3 个 IP**（用本地默认 DNS 通常只得到 3 个）。

要拿全各地节点，标准手段是 **EDNS Client Subnet（ECS）** —— 在查询里声明「我来自北京电信 / 上海联通 / 广州移动……」，权威 DNS 就会返回对应地域的节点。因此解析分两步：

1. **常规多源**：阿里、腾讯、Google 的公共 DoH 与系统解析并发，取并集；
2. **ECS 多地域**：用 10 个代表性省网段逐个查询（北京 / 上海 / 广州 / 成都 × 电信 / 联通 / 移动）。

全程免费，不需要任何付费 API。日志会打印每个来源贡献了多少个新 IP：

```
解析 cloudnproxy.baidu.com → 12 个节点（来源 阿里 DoH:3 腾讯 DoH:2 ECS 北京电信:1 …）
```

> 若解析结果中出现 `198.18.x.x` / `198.19.x.x`，说明本地网络存在 **DNS 劫持或代理软件接管解析**（该网段是 RFC 2544 保留的基准测试段，并非公网地址）。程序会自动过滤掉这类地址，不参与测速。

### 节点选型参考（历史实测）

下表是对全量节点做过一次完整测速的结果，可作为「优先测哪几个」的先验：

| 节点 IP | 节点归属 | 出口 IP | 出口归属 | 出口 ASN | 速度 |
|---|---|---|---|---|---|
| 14.215.182.75 | 广州 · 电信 | 14.215.185.36 | 广州 · 电信 | AS4134 | 127.76 Mbps |
| 183.240.98.84 | 广州 · 移动 | 14.215.185.62 | 广州 · 电信 | AS4134 | 113.41 Mbps |
| 163.177.17.189 | 广州 · 联通 | 14.215.185.28 | 广州 · 电信 | AS4134 | 71.67 Mbps |
| 110.242.70.68 | 承德 · 联通 | 157.0.147.149 | 苏州 · 联通 | AS140717 | 54.28 Mbps |
| 110.242.70.69 | 承德 · 联通 | 180.101.52.199 | 南京 · 电信 | AS134756 | 54.18 Mbps |
| 220.181.33.174 | 北京 · CNISP | 157.0.147.147 | 苏州 · 联通 | AS140717 | 40.94 Mbps |
| 163.177.17.6 | 广州 · 联通 | 14.215.185.34 | 广州 · 电信 | AS4134 | 31.69 Mbps |
| 180.101.50.208 | 南京 · 电信 | 157.0.147.174 | 苏州 · 联通 | AS140717 | 1.00 Mbps |
| 157.0.146.158 | 苏州 · 联通 | 157.0.147.145 | 苏州 · 联通 | AS140717 | 0.26 Mbps |

不可用（握手或连接失败）：`36.155.169.188`、`220.181.7.1`、`153.3.237.117`、`220.181.111.189`、`180.101.50.249`。

由此得到三点规律：

1. **入口运营商与出口运营商常常不一致** —— 移动/联通的入口经常由电信段出口；
2. 最快的几个都收敛到 **`AS4134`（广州电信）出口**，说明瓶颈在出口段而非入口；
3. `AS140717`、`AS134756` 出口普遍很慢，选型时应优先排除。

> 这是历史数据，节点状态会随时间变化。实际使用请以程序内「全部测速」的当前结果为准。

### 测速：为什么走 HTTPS

`speed.cloudflare.com` 已拒绝对 `/__down` 的**明文 HTTP** 访问（返回 `403 Forbidden`），因此测速改为请求：

```text
https://speed.cloudflare.com/__down?bytes=1000000
```

隧道本身只做裸 TCP，为了完成 TLS，程序会临时在 `127.0.0.1` 的随机端口起一个**只服务被测节点**的轻量 SOCKS5，再让 `reqwest` 通过它请求 —— TLS 由 reqwest 处理，不需要为测速单独引入 TLS 依赖。测速结束即关闭，不占用固定端口。

**测速链接可自定义**：在「节点库」页用下拉框切换，或直接在输入框填入任意 `http://` / `https://` 链接后点「＋ 添加」，它会保存进配置列表供下次选择。「删除」按钮移除当前选中的链接。默认是 1 MB 样本，想要更稳定的结果切到 10 MB / 99 MB 即可（30 秒硬上限，超时按已下载量计算速度）。

> 注意 Cloudflare 的 `/__down` 对 `bytes` 参数有 **99 MB 上限**，写 `100000000` 会被拒绝。

### TUN 模式下的流量走向与绕过

如果本机同时运行 v2rayN / Clash 等软件并开启 **TUN 模式**，它们会改写系统路由表，把全部出站流量导入虚拟网卡 —— 而绕过规则通常只覆盖**它自己进程**与**它自己的服务器 IP**，**本程序不在其中**。默认情况下：

| 后果 | 说明 |
|---|---|
| 多一跳 | 本程序 → TUN → 代理软件节点 → T5 节点 |
| 源 IP 改变 | T5 节点看到的是代理软件的出口 IP，可能触发异地鉴权/限流 |
| **测速失真** | 测出来的是「绕一圈后」的速度，不是节点真实速度 |

程序会查询到节点的实际出站网卡并在「概览」页显示（`✓ 直连（Ethernet）` 或 `⚠ 走 TUN（Wintun）`）；`t5d` 会在启动日志里打印同样信息。

#### 内建绕过：直接绑定物理网卡

「概览」页的**出站网卡**下拉可强制本程序的流量从指定网卡发出，**不需要改代理软件的任何设置**：

| 选项 | 含义 |
|---|---|
| `自动（物理网卡）` | 默认。自动挑一张已启用的物理网卡，绕过 TUN |
| `跟随系统路由` | 不绑定，完全按系统路由表（可能走 TUN） |
| 具体网卡名 | 强制绑定该网卡，如 `WLAN`、`eth0` |

两种平台的实现机制：

| 平台 | 机制 | 权限要求 |
|---|---|---|
| Windows | `IP_UNICAST_IF`（按接口索引，网络字节序） | 一般无需管理员 |
| Linux | `SO_BINDTODEVICE`（按设备名） | 需 root / `CAP_NET_RAW` |

> 只用 `bind()` 绑定源 IP 是**无效的**：路由查找仍按目标地址进行，包照样被送往 TUN，只是源地址与出接口不匹配而被丢弃（实测表现为连接超时）。

#### 如何自动识别 TUN 网卡

程序不靠网卡名字猜测，而是读取**接口类型**：

| 平台 | 判据 | 含义 |
|---|---|---|
| Windows | `NdisMedium == 19`（`NdisMediumIP`） | 纯 IP 层、无以太网封装 —— **TUN 的确切特征** |
| Windows | `Virtual == True` 且 `PhysicalMediaType == Unspecified` | 虚拟网卡（含 TAP 型 VPN 适配器） |
| Linux | `/sys/class/net/<if>/type == 65534`（`ARPHRD_NONE`） | TUN（三层）设备的类型 |
| Linux | 存在 `/sys/devices/virtual/net/<if>` | 虚拟网卡 |

名称与驱动描述里的关键词（`wintun`、`clash`、`tun`…）只作兜底 —— 名字可以被用户改，类型不会。因此无论换成 sing-box、Nekoray、WireGuard 还是自建 TUN，都能被正确识别。

实测（本机）：

| 网卡 | NdisMedium | MediaType | Virtual | 判定 |
|---|---|---|---|---|
| WLAN | 16 | Native 802.11 | False | 物理 ✓ |
| xray_tun | 19 | IP | True | TUN ✗ |
| 本地连接（VPN 适配器） | 0 | 802.3 | True | 虚拟 ✗ |
| 以太网 | 0 | 802.3 | False | 物理，但未连接 ✗ |

#### ⚠️ 绑定之后必须能出网

绑定物理网卡后，流量走该网卡所在网络。**若那个网络本身不能直连外网**（典型场景：企业网必须经 HTTP 代理），就必须同时启用 Chain，否则所有连接都会超时：

```toml
egress_interface = ""             # 绑定物理网卡（绕过 TUN）
chain_enabled = true              # 同时启用一级代理
chain_addr = "10.0.0.200:80"
```

绑定物理网卡但未启用 Chain 时，程序会在日志里给出警告。实测可用的链路：

```
本程序 ──WLAN──> 10.0.0.200:80 ──CONNECT──> T5 节点 ──CONNECT──> 目标
```

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

### Web 控制台（在浏览器里管理）

`t5d` 内建了一个 Web 控制台，界面与桌面版**完全相同** —— 复用同一份 `ui/` 资源，编译期嵌入二进制，部署时不需要额外拷贝静态目录。适合装在 VPS / 服务器上用浏览器管理。

**默认关闭**：把控制台暴露到网络是个安全决定，需要显式开启。

```toml
[web]
enabled = true
listen = "0.0.0.0:10110"     # 只在本机用就写 127.0.0.1:10110
```

也可以用一份最小配置直接跑起来（其余字段自动取默认值）：

```bash
mkdir -p /etc/cloudnproxy
cat > /etc/cloudnproxy/config.toml <<'EOF'
[web]
enabled = true
listen = "0.0.0.0:10110"
EOF
t5d -f /etc/cloudnproxy/config.toml
```

**首次启动**会生成随机初始密码，并**只打印这一次**：

```text
WARN  Web 控制台已初始化，以下是初始凭据（仅显示这一次）
WARN    用户名：admin
WARN    初始密码：k7Rm-2pQx-9tBv-nZ4w
WARN    首次登录后必须修改密码；请立即妥善保存或完成改密
```

浏览器打开 `http://<服务器IP>:10110/`，用该密码登录后会**强制跳转改密页**，改完才能进入控制台。

#### 认证机制

| 项 | 实现 |
|---|---|
| 口令存储 | argon2id 散列（PHC 字符串）写进 `config.toml`，不保存明文 |
| 登录令牌 | 自实现的 **HS256 JWT**，默认 24 小时有效（`token_ttl_hours` 可调），密钥持久化以便重启后令牌仍有效 |
| 首次改密 | 令牌携带 `pwd=true`，未改密前除改密接口外**一律返回 403** |
| 暴力破解 | 按来源 IP 计数，连续 5 次失败锁定 60 秒，之后按次数翻倍（上限 15 分钟） |
| 代理头信任 | 仅当直连方是回环/私网地址时才采信 `X-Forwarded-For`，防止伪造该头绕过限速 |
| 接口鉴权 | 除登录、改密、静态资源外的全部接口都要求 `Authorization: Bearer <JWT>` |

**接口约定**：`POST /api/rpc/<命令名>`，命令名与桌面版的 `invoke()` 一一对应；实时推送走 SSE（`GET /api/events`），每帧形如 `{"type":"…","payload":{…}}`。

#### ⚠️ 公网部署安全提示

控制台本身是**明文 HTTP**，密码与令牌在传输中不加密。对公网暴露时请至少做到一条：

1. 用防火墙 / 安全组限制来源 IP；
2. 或（推荐）套一层 HTTPS 反向代理：

```nginx
location / {
    proxy_pass http://127.0.0.1:10110;
    proxy_set_header X-Forwarded-For $remote_addr;   # 让限速按真实来源 IP 计数
    proxy_http_version 1.1;
    proxy_set_header Connection "";
    proxy_buffering off;                              # SSE 必须关闭缓冲
}
```

`proxy_http_version 1.1` 与 `proxy_buffering off` 是必须的，否则日志和速率推送会被缓冲住不动。

#### Web 端做不到的事

| 命令 | 原因 |
|---|---|
| `set_autostart` | 开机自启属于宿主机的服务管理（systemd），不在本进程能改的范围内；界面会隐藏该开关 |
| `open_config_dir` | 打不开服务器上的文件管理器；界面会隐藏该按钮 |

> 控制台权限只覆盖「管理这个代理进程」，**不等于给 SOCKS5 加了认证** —— SOCKS5 入站依然不校验凭据。

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
egress_interface = ""          # 空 = 自动选物理网卡（绕过 TUN）；"system" = 跟随系统路由
connect_timeout_ms = 10000
tcp_nodelay = true
tunnel_pool = true
auto_reconnect = true
auto_switch = false

# ---- 测速 ----
speed_url = "https://speed.cloudflare.com/__down?bytes=1000000"   # 当前使用
speed_urls = [                 # 下拉可选列表
  "https://speed.cloudflare.com/__down?bytes=1000000",    # 1 MB
  "https://speed.cloudflare.com/__down?bytes=10000000",   # 10 MB
  "https://speed.cloudflare.com/__down?bytes=99000000",   # 99 MB（Cloudflare 上限）
]

# ---- 日志 ----
log_level = "info"
log_file = ""                  # 留空 = 不写文件

# ---- Web 控制台（无 GUI 版专用，默认关闭）----
[web]
enabled = false                # 改成 true 并用浏览器访问 http://<IP>:10110/
listen = "0.0.0.0:10110"
username = "admin"
password_hash = ""             # 留空 = 首次启动生成随机初始密码并打印到日志
must_change_password = false   # 由程序自己维护：生成初始密码后置 true
jwt_secret = ""                # 留空 = 首次启动随机生成并写回
token_ttl_hours = 24

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
2. `smoke` —— 真正启动 `t5d`，校验端口秒级就绪、标准输出、日志文件、**Web 控制台完整认证流程**（未鉴权被拒 → 用日志中的随机密码登录 → 强制改密 → 改密后放行 → 旧密码失效 → 静态界面可访问），以及 SIGTERM 优雅退出，并对代理链路做软检查
3. `build` —— Windows（NSIS）与 Linux（AppImage / deb）
4. `musl` —— 静态链接版 `t5d-musl`，并在 **alpine 容器**中实测可运行
5. `release` —— 打 tag（如 `v0.2.0`）时自动发布 Release

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
| `cloudnproxy.exe` | Windows | ~5.0 MB | 免安装绿色版，双击即可运行 |
| `t5d.exe` | Windows | ~2.7 MB | 无 GUI 版本，命令行工具（含 Web 控制台） |
| `CloudNProxy_x.y.z_amd64.deb` | Linux | ~2.9 MB | Debian / Ubuntu 安装包。**webkit 等依赖由 apt 提供**，所以体积小 |
| `CloudNProxy_x.y.z_amd64.AppImage` | Linux | ~78 MB | 便携版。把 WebKit / GTK 整套运行时打包进文件，拷到任意发行版直接运行，无需安装任何依赖 |
| `cloudnproxy` | Linux | ~6.4 MB | 图形界面版裸二进制（要求系统已装 `libwebkit2gtk-4.1-0`） |
| `t5d` | Linux | ~3.1 MB | 无 GUI 版裸二进制，**动态链接 glibc** |
| `t5d-musl` | Linux | ~3.2 MB | 无 GUI 版**静态链接**，零动态依赖 |

> 体积以 v0.2.0 实测为准。`t5d` 相比 v0.1.x 增大约 0.7 MB，来自内建的 Web 控制台（HTTP 服务、argon2 口令散列、JWT 签名，以及编译期嵌入的界面资源）。

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
| 体积 | 3.09 MB | 3.22 MB |
| `ldd` 结果 | 列出 `libc.so.6` 等一串依赖 | `statically linked`（`static-pie`） |
| 能否跑在 Alpine / BusyBox | ✗（musl 与 glibc 不兼容） | ✓ |
| 能否跑在老发行版（CentOS 7 等） | ✗ 常报 `GLIBC_2.xx not found` | ✓ |
| DNS 行为 | 走 glibc NSS，查找链完整 | musl 自带 resolver，只读 `/etc/resolv.conf`（本项目节点解析以 DoH 为主，影响可忽略） |

### 为什么 AppImage 比 deb 大 28 倍

两者承诺不同：**deb** 只声明"我依赖 webkit2gtk"，库由系统提供；**AppImage** 承诺"拷到任何机器直接跑"，所以把整套图形栈塞进文件 —— `libjavascriptcoregtk`（JS 引擎）、`libwebkit2gtk`、GTK3 全家桶、ICU 数据、GStreamer 核心库等。这部分约占 78 MB 中的 90%+，本项目自己的代码只有约 6 MB。

作为对照：Windows 端只有 1.9–5.0 MB，因为 Windows 11 系统自带 WebView2，不需要打包浏览器内核。

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

### Web 控制台版

在 `config.toml` 里打开 `[web] enabled = true` 后启动 `t5d`，浏览器访问 `http://<主机>:10110/`，用日志中打印的初始密码登录并完成首次改密。之后的界面与操作**与图形界面版完全一致**，配置方式与安全注意事项见上文「[Web 控制台](#web-控制台在浏览器里管理)」。

---

## 项目结构

```
cloudnproxy/
├── Cargo.toml                       # workspace
├── crates/t5-core/                  # 引擎 + 控制层（可独立编译、含单元测试）
│   └── src/{config,logbuf,stats,socks5,outbound,engine,resolver,bench,
│            tunnel_pool,temp_proxy,netinfo,events,control}.rs
├── crates/t5-web/                   # Web 控制台（HTTP + JWT + SSE + 内嵌界面）
│   └── src/{lib,auth,server,assets}.rs
├── crates/t5-daemon/                # 无 GUI 版本 → t5d（内建 Web 控制台）
├── src-tauri/                       # 图形界面版
│   ├── src/{lib,main,commands,tray}.rs
│   └── {tauri.conf.json, capabilities/default.json}
├── ui/                              # 前端（静态 HTML/CSS/JS，无打包器）
├── scripts/gen-icon.js              # 生成图标源 PNG（零依赖）
├── .github/workflows/build.yml
├── README.md                        # 本文件（含协议原理、完整时序、排查表）
└── ui-prototype.html                # 界面设计稿（浏览器可直接打开预览）
```

---

## 已知限制

| 项 | 说明 |
|---|---|
| 仅 TCP | 未实现 SOCKS5 的 UDP ASSOCIATE，QUIC / UDP 流量不可代理 |
| 无入站认证 | SOCKS5 握手不校验凭据；默认只监听 `127.0.0.1`，开启 `allow_lan` 前请自行评估风险。控制台登录**不会**给 SOCKS5 加上认证 |
| Web 控制台默认关闭 | 需在 `config.toml` 的 `[web]` 里显式开启 |
| Web 控制台无内建 TLS | 监听非回环地址时是明文 HTTP，公网部署请套 HTTPS 反向代理并限制来源 |
| 隧道池粒度 | 仅按「上游节点 + 目标」缓存，且只缓存未传数据的干净隧道，命中率取决于访问目标是否集中 |
| 自动切换依据 | 使用节点库中最近一次测速结果评分；未测速的节点不参与 |
| 日志时间戳 | 文件与标准输出使用 **UTC** 时间 |
| 第三方接口 | GeoIP 用 `ip-api.com`、测速用 `speed.cloudflare.com`（1 MB HTTPS），不可达时对应列显示「—」 |
| ECS 覆盖面 | 阿里 DoH 支持 `edns_client_subnet`（实测有效：电信/联通地域匹配准，移动较弱）；腾讯 `doh.pub` 返回 HTTP 400，不支持该参数 |
| Google DoH 可达性 | 它一次就能返回该域名的**全部** A 记录（实测 14 个），但国内常不可达；此时由阿里 ECS 兜底 |
| 测速精度 | 默认 1 MB 样本较小，单次结果受瞬时抖动影响；可在「测速链接」下拉切换到 10 MB / 99 MB，或横向比较全部节点的结果 |
| 测速链接上限 | Cloudflare `/__down` 的 `bytes` 参数最大 **99 MB**（`99000000`），填更大值会返回错误 |

---

## 故障排查

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 启动失败、端口被占用 | 监听端口已被别的进程占用 | 换 `listen_port`，或结束占用进程（Linux：`ss -ltnp \| grep <端口>`） |
| 日志刷 `DENIED (HTTP/1.1 4xx…)` | `t5_auth` 失效，或节点不接受该 `fake_host` | 换有效凭证；换节点 |
| 所有连接被重置（`forcibly closed`） | 目标不被节点 ACL 放行 | 换目标端口 / 换节点 |
| `chain CONNECT failed` | Chain 代理不可达，或不允许连到 T5 节点 | 核对 `chain_addr` 与目标端口 |
| 客户端报 `Could not resolve host` | 客户端用了 `--socks5` 而非 `--socks5-hostname` | 改用后者，让代理端解析域名 |
| 速度远低于测速结果 | 目标路径 / 时段差异 | 多次采样；在「节点库」切到更大的测速链接样本 |
| 测速结果明显偏低 | 出站流量被 TUN 模式代理软件接管 | 看「概览」页的出站网卡提示，见上文 TUN 章节 |
| 解析不到节点 | DoH 被劫持或网络受限 | 日志会提示「解析未获得任何 IP」；检查是否出现 `198.18.x.x` 劫持地址 |
| Web 控制台打不开 | 未启用，或端口 / 防火墙未放行 | 确认 `[web] enabled = true`；`ss -ltn` 查端口；放行防火墙 |
| 忘了 Web 控制台密码 | 口令只存散列，无法反推 | 删掉 `config.toml` 里 `[web]` 段的 `password_hash` 与 `must_change_password` 后重启，会重新生成随机初始密码并打印一次 |
| Web 端日志 / 速率不刷新 | 反向代理缓冲了 SSE | nginx 加 `proxy_buffering off` 和 `proxy_http_version 1.1` |
| 开机自启没生效 | 自启注册被系统或安全软件拦截 | Windows 查注册表 `HKCU\...\Run`；Linux 查 `~/.config/autostart/` 与 `systemctl --user status` |

---

## 合规声明

本项目是网络协议转换与链路测量的技术实现。请仅在自己拥有管理权限、或已获得网络运营方明确授权的网络中使用，并遵守所在国家/地区的法律法规。`t5_auth` 属于个人凭证，请勿提交到公开仓库或分享给他人。
