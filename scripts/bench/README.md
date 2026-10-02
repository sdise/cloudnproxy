# 压测脚本

用于测量 `t5d` 在不同负载下的 CPU 与内存占用。实测数据见主 README 的
「[性能测试](../../README.md#性能测试)」章节。

## 为什么用假 T5 节点

直接压真实节点不行 —— 节点带宽（实测约 128 Mbps）和可用性会成为瓶颈，
测出来的是节点的上限，而不是引擎自身的开销。所以这里在本地起一个假节点，
它对 `CONNECT` 的响应与真实节点一致（回 `200 Connection Established`），
之后按指定速率往引擎灌数据：

```text
压测客户端 ──SOCKS5──► t5d ──CONNECT──► 假 T5 节点（本地）
   并发连接           被测对象            持续下发数据
```

流量不出机器，因此可以一直压到回环带宽的上限（实测 12.6 Gbps）。

## 文件

| 文件 | 作用 |
|---|---|
| `mock-t5.js` | 假 T5 节点：接受 CONNECT 后回 200，并按模式持续下发数据 |
| `bench.js` | SOCKS5 压测客户端：建立 N 条连接并维持到场景结束 |
| `sample.ps1` | Windows 采样器：记录目标进程的 CPU / 内存 → CSV |
| `sample.sh` | Linux 采样器：同上（读 `/proc`） |
| `run-scenario.ps1` | Windows 编排：一键跑一个场景（起停 + 采样 + 加压） |
| `run-scenario.sh` | Linux 编排：同上 |

## 前置条件

- **Node.js** —— 压测客户端与假节点用它写
- **已构建的 `t5d`**：

  ```bash
  cargo build --release -p t5-daemon     # 产物 target/release/t5d
  ```

  或直接从 Release 下载。默认在仓库的 `target/release/` 下查找，
  也可以用 `-T5d` / 第三个环境变量指定路径。

## 用法

### Windows

```powershell
# 场景 A：500 连接，建连后空闲（测连接数本身的开销）
powershell -File scripts\bench\run-scenario.ps1 -Name A -Conns 500 -Mode idle -MockMode idle -Seconds 60

# 场景 B：8 连接，满速下行（测吞吐的开销）
powershell -File scripts\bench\run-scenario.ps1 -Name B -Conns 8 -Mode bulk -MockMode sink -Seconds 60

# 场景 C：300 连接，每连接限速 500 KB/s（并发 + 流量叠加）
powershell -File scripts\bench\run-scenario.ps1 -Name C -Conns 300 -Mode bulk -MockMode sink -MockRateKb 500 -Seconds 60

# 指定 t5d 路径
powershell -File scripts\bench\run-scenario.ps1 -Name A -T5d D:\tools\t5d.exe
```

### Linux

```bash
chmod +x scripts/bench/*.sh

scripts/bench/run-scenario.sh A 500  idle idle 0   60
scripts/bench/run-scenario.sh B 8    bulk sink 0   60
scripts/bench/run-scenario.sh C 300  bulk sink 500 60

# 环境变量可覆盖默认值
T5D=/usr/local/bin/t5d PROXY_PORT=18080 scripts/bench/run-scenario.sh A 500 idle idle 0 60
```

结果写入 `scripts/bench/out/`：`result-<名字>.csv` 与若干 `.log`。

## 参数

| 参数（PowerShell / shell 位置参数） | 说明 |
|---|---|
| `-Name` / `$1` | 场景名，决定输出文件名 |
| `-Conns` / `$2` | 并发连接数 |
| `-Mode` / `$3` | 客户端模式：`idle`（建连后空闲，每 5 秒一个心跳字节）/ `bulk`（持续读取下行） |
| `-MockMode` / `$4` | 假节点模式：`idle`（只回 200）/ `sink`（回 200 后持续下发） |
| `-MockRateKb` / `$5` | 假节点每连接限速（KB/s），`0` = 不限 |
| `-Seconds` / `$6` | 加压时长（秒） |
| `-ProxyPort` | t5d 监听端口，默认 `18080`（刻意避开常用的 10801） |
| `-MockPort` | 假节点端口，默认 `19990` |
| `-T5d` | t5d 可执行文件路径 |

## 采样指标

`sample.ps1` / `sample.sh` 每 500 ms 记录一次，CSV 表头：

```text
t,workingSetMB,privateMB,cpuPercent,threads,handles
```

CPU 由**累计 CPU 时间差分**得到，比瞬时读数稳定得多：

```text
cpuPercent = ΔCPU时间 / Δ墙钟时间 / 逻辑核心数 × 100
```

> **刻意不统计 TCP 连接数**。Windows 的 `Get-NetTCPConnection` 要枚举整张系统
> TCP 表，连接上千时单次耗时数秒 —— 既把采样频率从 2 Hz 拉到 0.2 Hz，
> 又给正在被测的机器平白增加负载。并发数由压测端自己记录即可。

## ⚠️ 如何解读结果

**压出来的是引擎的能力上限，不是实际使用时的占用。** 回环网络没有带宽限制，
所以才能压到 12.6 Gbps；而真实 T5 节点只有约 128 Mbps，相差约 100 倍。
换算到实际带宽下，CPU 占用约为本测试的 1/100，基本可以忽略。

**内存才是唯一需要关注的资源**，而它只取决于并发连接数 —— 实测每连接约 67 KB，
与带宽无关。

## 其他说明

- 编排脚本会在开始前清理同名进程与端口残留，避免上一次的场景干扰这一次。
- 测试配置由编排脚本自动生成：**关闭隧道池与自动重连**（避免预建连接干扰测量）、
  日志级别设为 `warn`、`egress_interface` 设为 `system`（目标是回环地址，
  绑定物理网卡反而是错的）。
- `out/` 目录已在 `.gitignore` 中忽略。
