# CloudNProxy — 百度 T5 云代理 → 本地 SOCKS5 转换工具集

一套纯 PowerShell（零第三方依赖）的 Windows 工具集，用于把**百度 T5 云代理节点**（`cloudnproxy.baidu.com` 体系，靠 `Host` + `X-T5-Auth` 做鉴权的 HTTP CONNECT 代理）转换成**标准本地 SOCKS5 代理**，从而让 `curl`、浏览器、任何支持 SOCKS5 的程序都能直接使用这条链路；并附带一整套用于**节点筛选、出口探测、运营商识别、带宽测速、DNS 泄漏校验**的测试脚本。

---

## 1. 背景与要解决的问题

百度 T5 节点（如 `163.177.17.189:443`）本质上是一个 HTTP 正向代理，但它**不是标准代理**，有两个私有约定：

1. 客户端发出的 `CONNECT` 请求里，`Host` 头必须填写特定伪造域名 `cloudnproxy.baidu.com`（而不是目标域名），服务端据此判断这是一次"合法云代理"请求；
2. 必须携带 `X-T5-Auth: <用户ID>` 作为身份/计费凭证。

因此：**任何标准 SOCKS5 / HTTP 代理客户端都无法直接使用它**（浏览器发的是 `CONNECT target:443 HTTP/1.1` + `Host: target`，会被节点拒绝）。

`cloudnproxy` 的作用就是在本地做一层**协议翻译网关**：

```
本地应用 ──SOCKS5──► [baidu-t5-socks.ps1] ──改写后的 CONNECT──► 百度 T5 节点 ──► 目标站点
                      127.0.0.1:10801        带 Host/X-T5-Auth
                             │
                             └─（可选）经内网 HTTP 代理跳转 -Chain
```

---

## 2. 目录结构与文件说明

| 文件 | 类型 | 作用 |
|---|---|---|
| `baidu-t5-socks.ps1` | **核心** | 内联 C# 实现的 SOCKS5 服务端，完成协议转换与流量转发 |
| `start-t5-bg.ps1` | 运维 | 后台无窗口启动转换代理，写 PID 到 `t5-bg.pid` |
| `start-t5-bg.bat` | 运维 | 上面脚本的 `.bat` 包装（双击即用） |
| `stop-t5-bg.ps1` | 运维 | 按 PID 停止后台代理，并清理占用 10801 端口的残留进程 |
| `stop-t5-bg.bat` | 运维 | 停止脚本的 `.bat` 包装 |
| `test-all-ips.ps1` | 测试 | 批量遍历候选节点 IP：探测出口归属 + 测速，输出 TSV 报表 |
| `resolve-check.ps1` | 测试 | 代理链路下的 DoH / GeoIP / Cloudflare trace / DNS 泄漏检查 |
| `resolve-baidu-doh.ps1` | 测试 | 手写 DNS-over-TCP 报文经 SOCKS5 发出，探测节点 ACL 与 CDN 调度 |
| `cloudnproxy.txt` | 数据 | `test-all-ips.ps1` 产出的 TSV 结果报表 |
| `t5-bg.pid` | 运行时 | 后台代理进程 PID（`start` 生成、`stop` 消费，可删除） |

---

## 3. 核心组件详解：`baidu-t5-socks.ps1`

### 3.1 实现方式

脚本用 `Add-Type -TypeDefinition` 把一段完整 C# 代码（`public static class BaiduT5Socks`）**即时编译并加载进当前 PowerShell 进程**，然后调用 `[BaiduT5Socks]::Start($ListenPort)`。

这样做的目的：避开 PowerShell 管道/对象的开销，用裸 `NetworkStream` + 线程做高性能字节级转发；同时保持"单文件、无依赖、无需管理员安装"的分发优势。

> 首次运行会有 1–3 秒的 `csc` 编译等待。

### 3.2 参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `-ListenPort` | `10801` | 本地 SOCKS5 监听端口，绑定回环 `127.0.0.1` |
| `-Upstream` | `163.177.17.189:443` | 百度 T5 上游节点地址（`IP:端口`） |
| `-FakeHost` | `cloudnproxy.baidu.com` | 注入的伪 `Host` 头，即 T5 鉴权域名 |
| `-T5Auth` | `1050504963` | 注入的 `X-T5-Auth` 头内容（T5 用户标识） |
| `-Chain` | 空 | 可选的**一级 HTTP 代理**（`host:port`），用于先跳到能访问 T5 节点的网络 |

### 3.3 完整交互时序

```
1. SOCKS5 握手
   客户端  -> 05 <nmethods> <methods...>
   服务端  <- 05 00                          （无条件"无需认证"）

2. 请求解析（仅支持 CONNECT，cmd=0x01）
   VER CMD RSV ATYP | 地址 | 端口(2字节大端)
   - ATYP=1  4 字节 IPv4
   - ATYP=3  1 字节长度 + 域名
   - ATYP=4  16 字节 IPv6
   - CMD≠1   回复 rep=7（命令不支持）
   - ATYP 非法 回复 rep=8（地址类型不支持）

3. 建立到上游的 TCP 连接
   a) 指定 -Chain 时：
      连接 Chain → 发 "CONNECT <Upstream> HTTP/1.1\r\nHost: <Upstream>\r\n\r\n"
      → 读取响应头，必须包含 " 200 "，否则抛错
   b) 否则直连 Upstream

4. 发送改写后的 CONNECT（关键一步）
      CONNECT <真实目标host>:<port> HTTP/1.1\r\n
      Host: cloudnproxy.baidu.com\r\n          ← 伪 Host，不是真实域名
      X-T5-Auth: 1050504963\r\n                ← 凭证
      Proxy-Connection: Keep-Alive\r\n
      \r\n

5. 读取上游响应头
   - 含 " 200 " → 回客户端 05 00 00 01 0.0.0.0:0（成功）
   - 否则        → 回 rep=5（连接被拒），控制台打印 DENIED 及首行原因

6. 双向转发
   两条后台/前台线程各自执行 NetworkStream.CopyTo：
      downstream(目标→客户端) 单独线程；upstream(客户端→目标) 占用当前线程
   任一端断开 → 关闭两侧，线程结束
```

### 3.4 设计细节

- **每条 SOCKS 连接独立一条上游 TCP 连接**：隔离性最好，某条连接异常不会影响其它连接。
- **超时策略**：握手/CONNECT 响应阶段 `ReceiveTimeout = 10s`，防止半开连接吊死线程；一旦 CONNECT 成功就置为 `0`（不超时），避免长连接/大文件下载被误断开。
- **`NoDelay = true`**：关闭 Nagle，降低交互式流量延迟。
- **逐字节读响应头**：`ReadHeaders()` 按 `\r\n\r\n` 界定读取，不依赖 `Content-Length`，符合 HTTP 隧道响应特点。
- **日志**：每条连接打印一行 `[HH:mm:ss] 目标 -> OK / DENIED(...)`，异常打印 `[!] 目标 : 原因`。

---

## 4. 使用方法

### 4.1 前台运行（推荐调试时使用）

```powershell
cd e:\code\sdise\cloudnproxy
powershell -ExecutionPolicy Bypass -File .\baidu-t5-socks.ps1
```

可自定义节点与端口：

```powershell
powershell -ExecutionPolicy Bypass -File .\baidu-t5-socks.ps1 `
  -ListenPort 10801 `
  -Upstream 14.215.182.75:443 `
  -FakeHost cloudnproxy.baidu.com `
  -T5Auth 1050504963
```

若本机无法直连 T5 节点，需先经过内网 HTTP 代理：

```powershell
powershell -ExecutionPolicy Bypass -File .\baidu-t5-socks.ps1 `
  -Upstream 180.101.50.208:443 -Chain 10.0.0.200:80
```

### 4.2 后台运行（日常使用）

双击 `start-t5-bg.bat`，或：

```powershell
powershell -ExecutionPolicy Bypass -File .\start-t5-bg.ps1
```

启动流程：读取 `t5-bg.pid` → 若旧进程仍在则先杀掉 → 以 `-WindowStyle Hidden` 拉起 `baidu-t5-socks.ps1`（默认带上 `-Chain 10.0.0.200:80`）→ 写入新 PID → 等待 1.5s 确认存活。

停止：

```powershell
powershell -ExecutionPolicy Bypass -File .\stop-t5-bg.ps1   # 或双击 stop-t5-bg.bat
```
先 `taskkill /PID <pid> /T /F` 杀整棵进程树，再扫描 `Get-NetTCPConnection -LocalPort 10801 -State Listen`，兜底清掉任何占端口的残留进程。

### 4.3 验证链路是否通

```powershell
curl.exe --socks5-hostname 127.0.0.1:10801 https://api.ip.sb/geoip/ --max-time 20
curl.exe --socks5-hostname 127.0.0.1:10801 http://speed.cloudflare.com/cdn-cgi/trace --max-time 20
```

> `--socks5-hostname`（而不是 `--socks5`）表示**域名交给代理端解析**，可避免本地 DNS 泄漏。

---

## 5. 测试脚本详解

### 5.1 `test-all-ips.ps1` — 批量选型（主报表脚本）

对内置 14 个候选节点 IP 依次执行：启动代理 → 查出口 GeoIP → 查节点自身 GeoIP → 拉取 100MB 数据测速 → 输出一行 TSV → 停止代理。

内部函数：

| 函数 | 职责 |
|---|---|
| `Stop-BgProxy` | 清 PID + 杀占 10801 端口的进程 |
| `Start-BgProxy($ip)` | 以该 IP 为 Upstream 后台起代理，3 秒后校验端口监听是否成功 |
| `Classify-ISP($org)` | 中英文运营商归一化：`telecom/chinanet/电信/AS4134`→电信，`mobile/cmnet/移动/AS9808`→移动，`unicom/china169/联通/AS4837`→联通 |
| `Query-Geo($url,$viaProxy)` | 调 `api.ip.sb/geoip/...` 并 `ConvertFrom-Json`，可选走代理 |
| `Test-Speed` | `curl.exe` 拉 `http://speed.cloudflare.com/__down?bytes=99999999`，`--max-time 30`，从 `-w` 模板正则提取 `speed_download / size_download / time_total / http_code`，换算成 Mbps 输出 |

输出 `cloudnproxy.txt`，列依次为：
`代理IP · 代理IP归属地 · 代理IP所属组织 · 出口IP · 出口IP归属地 · 出口IP归属组织 · 出口IP的ASN · 网络速度`

### 5.2 `resolve-check.ps1` — 链路体检

代理起来后做 5 组对照测试：

| 段 | 内容 | 目的 |
|---|---|---|
| A | 阿里 DoH `dns.alidns.com/resolve` | 代理连通性 + 远端 DNS 解析结果 |
| B | 腾讯 DoH `doh.pub/dns-query`（`accept: application/dns-json`） | 交叉验证 DNS |
| C | `api.ip.sb/geoip/speed.cloudflare.com` | 远端视角下域名落到哪个 IP/城市/ASN |
| D | `http://speed.cloudflare.com/cdn-cgi/trace` | 实际连到的 Cloudflare 边缘节点（`colo=` 字段） |
| E | 本地 `Resolve-DnsName ... -Server 223.5.5.5` | 本地 vs 代理解析结果对比，判断 DNS 是否泄漏 |

### 5.3 `resolve-baidu-doh.ps1` — 裸 DNS-over-TCP 探测

不依赖 curl/DoH，**手工构造 DNS 查询报文**（事务 ID `0xABCD`，标准 flags `0x0100`，QDCOUNT=1，`QTYPE=1`/`QCLASS=1`），在外层封装 TCP 的 2 字节长度前缀，经 SOCKS5 发给 DNS 服务器的 53 端口。

- 被测 DNS：`180.76.76.76`（百度）、`223.5.5.5`（阿里）、`119.29.29.29`（腾讯）
- 被测域名：`speed.cloudflare.com`、`cloudnproxy.baidu.com`
- 自实现响应解析器：在 scriptblock 里做 DNS 名称压缩指针（`0xC0`）跳转跳过，遍历 Answer 段提取 A 记录

用途：检验 T5 节点是否放行任意 IP:53 的目标（即后端 ACL 范围），以及不同地域节点对同一域名拿到的 CDN IP 是否不同。

---

## 6. 已测速结果（摘自 `cloudnproxy.txt`）

按速度排序的可用节点：

| 代理 IP | 节点归属 | 出口 IP | 出口归属 | 出口 ASN | 速度 |
|---|---|---|---|---|---|
| 14.215.182.75 | 广州 · 电信 | 14.215.185.36 | 广州 · 电信 | 4134 | **127.76 Mbps** |
| 183.240.98.84 | 广州 · 移动 | 14.215.185.62 | 广州 · 电信 | 4134 | **113.41 Mbps** |
| 163.177.17.189 | 广州 · 联通 | 14.215.185.28 | 广州 · 电信 | 4134 | **71.67 Mbps** |
| 110.242.70.68 | 承德 · 联通 | 157.0.147.149 | 苏州 · 联通 | 140717 | 54.28 Mbps |
| 110.242.70.69 | 承德 · 联通 | 180.101.52.199 | 南京 · 电信 | 134756 | 54.18 Mbps |
| 220.181.33.174 | 北京 · CNISP | 157.0.147.147 | 苏州 · 联通 | 140717 | 40.94 Mbps |
| 163.177.17.6 | 广州 · 联通 | 14.215.185.34 | 广州 · 电信 | 4134 | 31.69 Mbps |
| 180.101.50.208 | 南京 · 电信 | 157.0.147.174 | 苏州 · 联通 | 140717 | 1 Mbps |
| 157.0.146.158 | 苏州 · 联通 | 157.0.147.145 | 苏州 · 联通 | 140717 | 0.26 Mbps |

不可用（握手/连接失败，`code=000`）：`36.155.169.188`、`220.181.7.1`、`153.3.237.117`、`220.181.111.189`、`180.101.50.249`。

**结论**：
- 入口（节点）所属 operator 与出口所属 operator **不必一致**——移动/联通入口常常由电信段出口；
- 速度最快的都收敛到 `AS4134`（广州电信）出口，说明带宽瓶颈在出口段而非入口；
- ASN `134756`、`140717` 出口普遍慢或不可用，选型时应优先排除。

---

## 7. 已知局限

1. **仅支持 TCP**：SOCKS5 的 `UDP ASSOCIATE`（cmd=3）未实现，QUIC / DNS-over-UDP / 游戏 UDP 流量无法代理。
2. **无访问控制**：监听器虽绑定 `127.0.0.1`，但 SOCKS5 握手不校验任何凭据，同机任意用户进程都可复用这条隧道。
3. **明文携带凭证**：`X-T5-Auth` 以 HTTP 头明文写在 CONNECT 请求里，链路若被中间设备观测即可提取。
4. **线程模型朴素**：主线程 accept + 每连接 2 个 OS 线程，高并发（数千连接）下资源消耗偏高；无连接池复用，每条新连接都要重新握手、重建 TLS。
5. **IPv6 支持不完整**：ATYP=4 只做了地址解析，上游 `CONNECT` 时直接以 `[ipv6]` 形式发送，兼容性取决于 T5 节点是否支持。
6. **无自动故障转移**：上游节点挂掉或超时后只能人工换 `-Upstream`；`test-all-ips.ps1` 是离线选型，运行时不会自动挑最佳节点。
7. **Windows 专属**：依赖 PowerShell、`Add-Type`、`Get-NetTCPConnection`、`taskkill`。

---

## 8. 故障排查

| 现象 | 可能原因 | 处理 |
|---|---|---|
| 启动报"代理启动失败" | 10801 被占用 / 首次编译超时 | 跑 `stop-t5-bg.ps1` 清端口，再重试（首次给足 3s 以上） |
| 控制台刷 `DENIED (HTTP/1.1 4xx...)` | `X-T5-Auth` 失效，或节点不接受该 `FakeHost` | 换有效的 `-T5Auth`；换 `-Upstream` 节点 |
| 所有连接 `An existing connection was forcibly closed` | Target HOST 不被节点 ACL 放行（只允许白名单端口/域名） | 先用 `resolve-baidu-doh.ps1` 探测 ACL 范围 |
| `chain CONNECT failed` | `-Chain` 的一级代理不可达或不允许连到 T5 节点 | 核对内网代理地址与目标端口 |
| `curl` 报 `Could not resolve host` | 用了 `--socks5` 而非 `--socks5-hostname` | 改用后者，让代理端解析域名 |
| 速度远低于报表值 | 目标路径/时段差异，`__down` 只有 100MB 上限 | 多次采样取均值，更换上层测速目标 |

---

## 9. 安全与合规声明

本仓库内容是**网络协议转换与链路质量测量的技术验证代码**，请仅在自己拥有管理权限、或已获得网络运营方明确授权的网络环境中使用；运行前请确认符合所在单位的网络管理规定与所在国家/地区的法律法规。`X-T5-Auth` 等标识属于个人/组织凭证，请勿提交真实凭证到公开仓库、勿分享给他人。作者不对任何滥用行为负责。
