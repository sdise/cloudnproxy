/* CloudNProxy 前端逻辑
 *
 * 同一份代码服务三种运行环境：
 *  - 桌面版：经 window.__TAURI__ 调用 Rust 命令，用 Tauri 事件接收推送；
 *  - Web 控制台（t5-daemon 内建）：命令走 POST /api/rpc/<cmd>，推送走 SSE，
 *    并带登录 / 首次改密流程；
 *  - 直接双击打开的本地文件：降级为演示数据，便于离线预览界面。
 */
(function () {
  'use strict';

  const isTauri = typeof window.__TAURI__ !== 'undefined' && !!window.__TAURI__.core;
  // 经 http(s) 打开且不是 Tauri，即运行在 Web 控制台里
  const isWeb = !isTauri && location.protocol !== 'file:';

  const TOKEN_KEY = 'cnp.token';
  const SESSION = {
    token: isWeb ? localStorage.getItem(TOKEN_KEY) || '' : '',
    username: 'admin',
  };

  /** Web 控制台：把一个命令映射成一次 HTTP 调用。 */
  async function httpInvoke(cmd, args) {
    const res = await fetch('/api/rpc/' + encodeURIComponent(cmd), {
      method: 'POST',
      headers: Object.assign(
        { 'Content-Type': 'application/json' },
        SESSION.token ? { Authorization: 'Bearer ' + SESSION.token } : {}
      ),
      body: JSON.stringify(args || {}),
    });

    if (res.status === 401) {
      SESSION.token = '';
      localStorage.removeItem(TOKEN_KEY);
      showGate('login');
      throw new Error('登录已过期，请重新登录');
    }
    if (res.status === 403) {
      const body = await res.json().catch(() => ({}));
      if (body.code === 'password_change_required') {
        showGate('password');
        throw new Error(body.error || '请先修改初始密码');
      }
    }
    if (!res.ok) {
      let msg = 'HTTP ' + res.status;
      try { msg = (await res.json()).error || msg; } catch (e) { /* 响应体不是 JSON */ }
      throw new Error(msg);
    }
    return res.json();
  }

  const invoke = isTauri
    ? window.__TAURI__.core.invoke
    : (isWeb ? httpInvoke : mockInvoke);

  /* ---------------- 事件订阅 ---------------- */

  const eventHandlers = new Map();

  function dispatchWebEvent(evt) {
    const list = eventHandlers.get(evt.type);
    if (!list) return;
    list.forEach((fn) => {
      try { fn({ payload: evt.payload }); } catch (e) { /* 单个处理器出错不影响其它 */ }
    });
  }

  /** Web 控制台：用 SSE 接收推送，断线自动重连。 */
  function listenViaSSE() {
    let stopped = false;
    (async () => {
      while (!stopped) {
        try {
          const res = await fetch('/api/events', {
            headers: SESSION.token ? { Authorization: 'Bearer ' + SESSION.token } : {},
          });
          if (res.status === 401) {
            SESSION.token = '';
            localStorage.removeItem(TOKEN_KEY);
            showGate('login');
            return;
          }
          if (!res.ok || !res.body) throw new Error('SSE HTTP ' + res.status);

          const reader = res.body.getReader();
          const decoder = new TextDecoder();
          let buf = '';
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            buf += decoder.decode(value, { stream: true });
            let idx;
            while ((idx = buf.indexOf('\n\n')) >= 0) {
              const frame = buf.slice(0, idx);
              buf = buf.slice(idx + 2);
              for (const line of frame.split('\n')) {
                if (!line.startsWith('data:')) continue;
                const raw = line.slice(5).trim();
                if (!raw) continue;
                try { dispatchWebEvent(JSON.parse(raw)); } catch (e) { /* 忽略坏帧 */ }
              }
            }
          }
        } catch (e) { /* 网络中断：稍后重连 */ }
        if (stopped) break;
        await new Promise((r) => setTimeout(r, 1500));
      }
    })();
    return () => { stopped = true; };
  }

  async function listen(name, cb) {
    if (isTauri) return window.__TAURI__.event.listen(name, cb);
    if (!isWeb) return () => {};
    if (!eventHandlers.has(name)) eventHandlers.set(name, []);
    eventHandlers.get(name).push(cb);
    return () => {
      const list = eventHandlers.get(name) || [];
      const i = list.indexOf(cb);
      if (i >= 0) list.splice(i, 1);
    };
  }

  const $ = (id) => document.getElementById(id);
  const LEVELS = { TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4 };

  const S = {
    cfg: null,
    cfgPath: '',
    version: '',
    status: { running: false, addr: '', upstream: '', chain: '' },
    egress: null,
    logs: [],
    hist: [],
    sort: { key: 'speed', asc: false },
    filterIsp: '',
    bench: false,
    benchIp: '',
    follow: true,
    page: 'overview',
  };

  /* ---------------- 工具 ---------------- */

  function esc(v) {
    return String(v == null ? '' : v)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function time(ms) {
    const d = new Date(ms || Date.now());
    const p = (n) => String(n).padStart(2, '0');
    return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
  }

  function fmtRate(bytesPerSec) {
    if (!bytesPerSec) return ['0', 'KB/s'];
    if (bytesPerSec >= 1048576) return [(bytesPerSec / 1048576).toFixed(2), 'MB/s'];
    if (bytesPerSec >= 1024) return [(bytesPerSec / 1024).toFixed(1), 'KB/s'];
    return [String(bytesPerSec), 'B/s'];
  }

  function fmtTotal(bytes) {
    if (!bytes) return ['0', 'MB'];
    if (bytes >= 1073741824) return [(bytes / 1073741824).toFixed(2), 'GB'];
    return [(bytes / 1048576).toFixed(1), 'MB'];
  }

  function shortPath(p) {
    if (!p) return '';
    const parts = String(p).split(/[\\/]/);
    return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : p;
  }

  let toastTimer = null;
  function toast(msg) {
    const el = $('toast');
    el.textContent = msg;
    el.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('show'), 2200);
  }

  /* ---------------- 导航 ---------------- */

  function goto(page) {
    S.page = page;
    document.querySelectorAll('.nav').forEach((n) =>
      n.classList.toggle('active', n.dataset.page === page));
    document.querySelectorAll('.page').forEach((p) =>
      p.classList.toggle('show', p.id === 'page-' + page));
    if (page === 'logs') renderLogs();
    if (page === 'overview') drawChart();
  }

  /* ---------------- 表单 ---------------- */

  function fillForm(cfg) {
    document.querySelectorAll('[data-bind]').forEach((el) => {
      const key = el.dataset.bind;
      const v = cfg[key];
      if (el.classList.contains('sw')) {
        el.classList.toggle('on', !!v);
      } else if (el.type === 'number') {
        el.value = v == null ? '' : v;
      } else {
        el.value = v == null ? '' : v;
      }
    });
    $('domainInput').value = cfg.resolve_domain || '';
    $('themeSel').value = cfg.theme || 'system';
    applyTheme(cfg.theme);
    syncChainEnabled();
    $('cfgPathRow').textContent = S.cfgPath || '—';
    $('cfgPathText').textContent = shortPath(S.cfgPath);
    renderSpeedUrls();
    renderInterfaces();
  }

  function renderInterfaces() {
    const sel = $('egressSel');
    if (!sel) return;
    const cur = (S.cfg && S.cfg.egress_interface) || '';
    const list = S.ifaces || [];
    const opts = [];

    opts.push(`<option value=""${cur === '' ? ' selected' : ''}>自动（物理网卡，绕过 TUN）</option>`);
    opts.push(`<option value="system"${cur === 'system' ? ' selected' : ''}>跟随系统路由（可能被 TUN 接管）</option>`);

    const seen = [];
    list.forEach((i) => {
      if (!i || !i.name) return;
      seen.push(i.name);
      const kind = i.is_tun ? 'TUN' : (i.is_virtual ? '虚拟' : '物理');
      const extra = i.ipv4 ? '，' + i.ipv4 : '';
      const down = i.is_up ? '' : '，未启用';
      const label = `${i.name}（${kind}${extra}${down}）`;
      opts.push(`<option value="${esc(i.name)}"${cur === i.name ? ' selected' : ''}>${esc(label)}</option>`);
    });

    // 配置里指定的网卡在当前列表中找不到时，仍然显示出来，避免选择被静默重置
    if (cur && cur !== 'system' && !seen.includes(cur)) {
      opts.push(`<option value="${esc(cur)}" selected>${esc(cur)}（未检测到）</option>`);
    }

    sel.innerHTML = opts.join('');
  }

  function renderSpeedUrls() {
    const sel = $('speedUrlSel');
    if (!sel) return;
    const cfg = S.cfg || {};
    const current = cfg.speed_url || '';
    let list = Array.isArray(cfg.speed_urls) ? cfg.speed_urls.slice() : [];
    if (current && !list.includes(current)) list.unshift(current);

    sel.innerHTML = list.length
      ? list.map((u) => {
          const label = u.length > 58 ? u.slice(0, 55) + '…' : u;
          const selected = u === current ? ' selected' : '';
          return `<option value="${esc(u)}" title="${esc(u)}"${selected}>${esc(label)}</option>`;
        }).join('')
      : '<option value="">（未设置）</option>';
  }

  function collectForm() {
    const cfg = JSON.parse(JSON.stringify(S.cfg || {}));
    document.querySelectorAll('[data-bind]').forEach((el) => {
      const key = el.dataset.bind;
      if (el.classList.contains('sw')) {
        cfg[key] = el.classList.contains('on');
      } else if (el.type === 'number') {
        cfg[key] = Number(el.value) || 0;
      } else {
        cfg[key] = el.value;
      }
    });
    cfg.resolve_domain = $('domainInput').value.trim() || cfg.resolve_domain;
    cfg.theme = $('themeSel').value;
    return cfg;
  }

  function syncChainEnabled() {
    const sw = document.querySelector('.sw[data-bind="chain_enabled"]');
    const on = sw && sw.classList.contains('on');
    const input = $('chainAddr');
    if (input) input.disabled = !on;
  }

  function applyTheme(t) {
    document.documentElement.setAttribute('data-theme', t || 'system');
  }

  /* ---------------- 状态渲染 ---------------- */

  function renderStatus() {
    const running = !!(S.status && S.status.running);
    const addr = (S.status && S.status.addr) || '—';
    const upstream = (S.status && S.status.upstream) || '—';
    const chain = (S.status && S.status.chain) || '直连';

    $('powerSw').classList.toggle('on', running);
    $('heroDot').classList.toggle('off', !running);
    $('heroState').textContent = running ? '运行中' : '已停止';
    $('heroMeta').innerHTML = running
      ? `本地 SOCKS5 <b>${esc(addr)}</b> · 当前节点 <b>${esc(upstream)}</b>`
      : '本地 SOCKS5 未运行';

    $('sbDot').classList.toggle('off', !running);
    $('sbState').textContent = running ? '运行中' : '已停止';
    $('sbNode').textContent = '节点 ' + upstream + (chain && chain !== '直连' ? '（经 ' + chain + '）' : ' · 直连');

    // 链路拓扑随 Chain 开关变化：启用时才在 SOCKS5 与 T5 节点之间插入一级代理
    const hasChain = !!(chain && chain !== '直连');
    $('chainLocal').textContent = addr;
    $('chainProxyBox').hidden = !hasChain;
    $('chainArrow').hidden = !hasChain;
    $('chainProxy').textContent = hasChain ? chain : '';
    $('chainNode').textContent = upstream;

    // 出站路径：到节点的流量是否被 TUN 类网卡接管
    const egr = S.egress || {};
    const e = $('heroEgress');
    if (!egr.interface) {
      e.innerHTML = egr.target
        ? `<span class="dim">出站接口：未探测到（目标 ${esc(egr.target)}）</span>`
        : '<span class="dim">出站接口：未探测到</span>';
    } else if (egr.via_tun) {
      e.innerHTML =
        `<span class="egress-warn">⚠ 走 TUN（${esc(egr.interface)}）</span>` +
        `<span class="dim"> · 到 ${esc(egr.target)} 的流量被代理软件接管，测速结果会失真</span>`;
    } else {
      e.innerHTML =
        `<span class="egress-ok">✓ 直连（${esc(egr.interface)}）</span>` +
        `<span class="dim"> · 到 ${esc(egr.target)} 的流量未经过 TUN</span>`;
    }
  }

  function renderStats(tick) {
    const [uv, uu] = fmtRate(tick.up_rate);
    const [dv, du] = fmtRate(tick.down_rate);
    $('stUp').innerHTML = uv + '<small>' + uu + '</small>';
    $('stDown').innerHTML = dv + '<small>' + du + '</small>';
    $('stConns').textContent = tick.conns;
    $('sbConns').textContent = tick.conns;

    const [tv, tu] = fmtTotal(tick.down_bytes + tick.up_bytes);
    $('stTotal').innerHTML = tv + '<small>' + tu + '</small>';
    $('sbTotal').textContent = tv + ' ' + tu;
  }

  function renderNodes() {
    const nodes = (S.cfg && S.cfg.nodes) || [];
    $('navNodeCount').textContent = nodes.length;
    const body = $('nodeBody');

    if (!nodes.length) {
      body.innerHTML = '<tr><td colspan="9" class="empty">尚无节点，请先在顶部输入域名并点击「解析域名」</td></tr>';
      $('nodeFoot').textContent = '尚未解析节点';
      return;
    }

    let list = nodes.slice();
    if (S.filterIsp) {
      list = list.filter((n) => n.entry_isp === S.filterIsp || n.exit_isp === S.filterIsp);
    }

    const key = S.sort.key;
    const dir = S.sort.asc ? 1 : -1;
    list.sort((a, b) => {
      let av, bv;
      if (key === 'latency') { av = a.latency_ms == null ? 1e9 : a.latency_ms; bv = b.latency_ms == null ? 1e9 : b.latency_ms; }
      else if (key === 'recent') { av = a.measured_at || 0; bv = b.measured_at || 0; }
      else { av = a.speed_mbps == null ? -1 : a.speed_mbps; bv = b.speed_mbps == null ? -1 : b.speed_mbps; }
      return (av - bv) * dir;
    });

    const curAddr = (S.cfg && S.cfg.current_node) || '';
    body.innerHTML = list.map((n) => {
      const addr = n.ip + ':' + (n.port || 443);
      const isCur = addr === curAddr;
      const isBench = S.benchIp === n.ip;
      const exit = [n.exit_isp, n.exit_asn].filter(Boolean).join(' · ');
      return `<tr class="${isCur ? 'cur' : ''}${isBench ? ' bench' : ''}">
        <td class="num">${esc(n.ip)}</td>
        <td>${esc(n.region) || '—'}</td>
        <td>${esc(n.entry_isp) || '—'}</td>
        <td class="num">${esc(n.exit_ip) || '—'}</td>
        <td>${esc(n.exit_region) || '—'}</td>
        <td>${esc(exit) || '—'}</td>
        <td class="num">${n.latency_ms == null ? '—' : n.latency_ms + ' ms'}</td>
        <td class="num">${n.speed_mbps == null ? '—' : n.speed_mbps.toFixed(1) + ' Mbps'}</td>
        <td>
          <button class="btn sm" data-act="bench" data-ip="${esc(n.ip)}" data-port="${n.port || 443}">测速</button>
          <button class="btn sm" data-act="use" data-ip="${esc(n.ip)}" data-port="${n.port || 443}" ${isCur ? 'disabled' : ''}>设为当前</button>
        </td>
      </tr>`;
    }).join('');

    const usable = nodes.filter((n) => n.speed_mbps != null || n.latency_ms != null).length;
    $('nodeFoot').textContent = `共 ${nodes.length} 个节点 · ${usable} 个已测 · 当前 ${curAddr || '未设置'}`;
  }

  function renderLogs() {
    const max = LEVELS[$('logLevel').value] ?? 2;
    const kw = $('logFilter').value.trim().toLowerCase();
    const box = $('logBox');
    const lines = S.logs.filter((l) =>
      (LEVELS[l.level] ?? 2) >= max && (!kw || String(l.msg).toLowerCase().includes(kw)));

    box.innerHTML = lines.map((l) => {
      const cls = l.level === 'ERROR' ? 'e' : l.level === 'WARN' ? 'w' : 'i';
      const lv = String(l.level).padEnd(5).replace(/ /g, '&nbsp;');
      return `<div><span class="t">${time(l.ts)}</span> <span class="${cls}">${lv}</span> ${esc(l.msg)}</div>`;
    }).join('');

    if (S.follow) box.scrollTop = box.scrollHeight;
  }

  const MAX_HIST = 60;
  function onStats(tick) {
    S.hist.push({ up: tick.up_rate, down: tick.down_rate });
    if (S.hist.length > MAX_HIST) S.hist.shift();
    renderStats(tick);
    if (S.page === 'overview') drawChart();
  }

  function drawChart() {
    const cv = $('chart');
    if (!cv || !cv.clientWidth) return;
    const dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth;
    const h = 120;
    cv.width = Math.round(w * dpr);
    cv.height = Math.round(h * dpr);
    const ctx = cv.getContext('2d');
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    const css = getComputedStyle(document.documentElement);
    const cGrid = css.getPropertyValue('--border').trim() || '#ddd';
    const cDown = css.getPropertyValue('--down').trim() || '#0f6cbd';
    const cUp = css.getPropertyValue('--up').trim() || '#c239b3';

    ctx.strokeStyle = cGrid;
    ctx.lineWidth = 1;
    for (let i = 1; i <= 4; i++) {
      const y = Math.round((h / 4) * i) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    const data = S.hist;
    if (data.length < 2) return;
    let peak = 64 * 1024;
    data.forEach((d) => { peak = Math.max(peak, d.up, d.down); });
    peak *= 1.15;

    const x = (i) => (i / (MAX_HIST - 1)) * w;
    const y = (v) => h - (Math.min(v, peak) / peak) * (h - 6) - 3;

    const line = (color, getter, dash) => {
      ctx.beginPath();
      ctx.strokeStyle = color;
      ctx.lineWidth = 2;
      ctx.setLineDash(dash || []);
      data.forEach((d, i) => {
        const px = x(i), py = y(getter(d));
        if (i === 0) ctx.moveTo(px, py); else ctx.lineTo(px, py);
      });
      ctx.stroke();
      ctx.setLineDash([]);
    };

    line(cDown, (d) => d.down);
    line(cUp, (d) => d.up, [4, 3]);
  }

  /* ---------------- 操作 ---------------- */

  async function refreshStatus() {
    try {
      const st = await invoke('get_status');
      S.status = st.engine || { running: false };
      S.egress = st.egress || null;
      renderStatus();
    } catch (e) { /* 忽略 */ }
  }

  async function togglePower() {
    try {
      const running = !!(S.status && S.status.running);
      if (running) {
        await invoke('stop_proxy');
        toast('已停止代理');
      } else {
        await invoke('start_proxy');
        toast('已启动代理');
      }
      await refreshStatus();
    } catch (e) {
      toast('操作失败：' + e);
    }
  }

  async function resolveNodes() {
    const domain = $('domainInput').value.trim();
    if (!domain) { toast('请输入域名'); return; }
    toast('正在解析 ' + domain + ' …');
    try {
      const nodes = await invoke('resolve_nodes', { domain });
      S.cfg.nodes = nodes;
      renderNodes();
      toast('解析完成，共 ' + nodes.length + ' 个节点');
    } catch (e) {
      toast('解析失败：' + e);
    }
  }

  function updateBenchButtons() {
    $('btnBenchAll').disabled = S.bench;
    $('btnBenchStop').disabled = !S.bench;
  }

  async function benchAll(autoPick) {
    if (S.bench) { toast('测速已在进行中'); return; }
    try {
      const total = await invoke('benchmark_all', {
        onlyMissing: false,
        autoPick: !!autoPick,
      });
      S.bench = true;
      updateBenchButtons();
      toast(autoPick
        ? `开始测速，结束后自动选优（共 ${total} 个节点）`
        : `开始测速，共 ${total} 个节点`);
    } catch (e) {
      toast('无法开始测速：' + e);
    }
  }

  function onBenchProgress(p) {
    // 这个节点已经测完，撤掉高亮
    S.benchIp = '';
    if (S.cfg && S.cfg.nodes) {
      const n = S.cfg.nodes.find((x) => x.ip === p.ip);
      if (n && p.result) {
        n.latency_ms = p.result.latency_ms;
        n.speed_mbps = p.result.speed_mbps;
        n.region = p.result.region;
        n.entry_isp = p.result.entry_isp;
        n.exit_ip = p.result.exit_ip;
        n.exit_region = p.result.exit_region;
        n.exit_isp = p.result.exit_isp;
        n.exit_asn = p.result.exit_asn;
        n.measured_at = Math.floor(Date.now() / 1000);
      }
    }
    $('chartHint').textContent = `测速中 ${p.index}/${p.total}`;
    renderNodes();
  }

  async function benchOne(ip, port) {
    if (S.bench) { toast('正在批量测速，请等本轮结束'); return; }
    S.benchIp = ip;
    renderNodes();
    toast('正在测速 ' + ip + ' …');
    try {
      const r = await invoke('benchmark_one', { ip, port: Number(port) });
      const n = S.cfg.nodes.find((x) => x.ip === ip);
      if (n) {
        Object.assign(n, {
          latency_ms: r.latency_ms,
          speed_mbps: r.speed_mbps,
          region: r.region,
          entry_isp: r.entry_isp,
          exit_ip: r.exit_ip,
          exit_region: r.exit_region,
          exit_isp: r.exit_isp,
          exit_asn: r.exit_asn,
        });
      }
      toast(r.speed_mbps != null
        ? `${ip} → ${r.speed_mbps.toFixed(2)} Mbps / ${r.latency_ms} ms`
        : `${ip} → 失败：${r.error || '无数据'}`);
    } catch (e) {
      toast('测速失败：' + e);
    } finally {
      S.benchIp = '';
      renderNodes();
    }
  }

  async function useNode(ip, port) {
    try {
      await invoke('set_current_node', { ip, port: Number(port) });
      S.cfg.current_node = ip + ':' + port;
      S.cfg.upstream = S.cfg.current_node;
      renderNodes();
      await refreshStatus();
      toast('已切换节点 ' + S.cfg.current_node);
    } catch (e) {
      toast('切换失败：' + e);
    }
  }

  async function applyConfig() {
    try {
      const cfg = collectForm();
      S.cfg = await invoke('apply_config', { cfg });
      fillForm(S.cfg);
      renderNodes();
      await refreshStatus();
      toast('配置已应用');
    } catch (e) {
      toast('应用失败：' + e);
    }
  }

  async function revertConfig() {
    try {
      S.cfg = await invoke('get_config');
      fillForm(S.cfg);
      renderNodes();
      toast('已恢复为保存的配置');
    } catch (e) { /* 忽略 */ }
  }

  function copySocks() {
    const addr = (S.status && S.status.addr) || '127.0.0.1:10801';
    const text = 'socks5://' + addr.replace('0.0.0.0', '127.0.0.1');
    navigator.clipboard.writeText(text).then(
      () => toast('已复制 ' + text),
      () => toast(text));
  }

  /* ---------------- 事件绑定 ---------------- */

  function bind() {
    document.querySelectorAll('.nav').forEach((el) =>
      el.addEventListener('click', () => goto(el.dataset.page)));

    document.querySelectorAll('.sw[data-bind]').forEach((el) =>
      el.addEventListener('click', () => {
        el.classList.toggle('on');
        if (el.dataset.bind === 'chain_enabled') syncChainEnabled();
      }));

    $('powerSw').addEventListener('click', togglePower);
    $('btnResolve').addEventListener('click', resolveNodes);
    $('btnBenchAll').addEventListener('click', benchAll);
    $('btnBenchStop').addEventListener('click', () => toast('当前版本需等待本轮测速结束'));
    $('btnApply').addEventListener('click', applyConfig);
    $('btnRevert').addEventListener('click', revertConfig);
    $('btnClearLogs').addEventListener('click', async () => {
      S.logs = [];
      await invoke('clear_logs');
      renderLogs();
    });
    $('btnOpenDir').addEventListener('click', async () => {
      try { await invoke('open_config_dir'); } catch (e) { toast('打开失败：' + e); }
    });
    $('btnReset').addEventListener('click', async () => {
      if (!confirm('将删除配置文件与已保存的测速结果，确定继续？')) return;
      try {
        await invoke('reset_data');
        S.cfg = await invoke('get_config');
        fillForm(S.cfg);
        renderNodes();
        await refreshStatus();
        toast('已重置');
      } catch (e) { toast('重置失败：' + e); }
    });

    $('btnCheckUpdate').addEventListener('click', checkUpdate);
    $('btnGithub').addEventListener('click', () => openUrl(REPO_URL));

    $('themeSel').addEventListener('change', () => applyTheme($('themeSel').value));
    $('filterIsp').addEventListener('change', (e) => { S.filterIsp = e.target.value; renderNodes(); });

    $('speedUrlSel').addEventListener('change', async (e) => {
      const url = e.target.value;
      if (!url) return;
      try {
        const list = await invoke('set_speed_url', { url });
        S.cfg.speed_urls = list;
        S.cfg.speed_url = url;
        renderSpeedUrls();
        toast('已切换测速链接');
      } catch (err) { toast('切换失败：' + err); }
    });

    $('btnAddSpeedUrl').addEventListener('click', async () => {
      const url = $('speedUrlInput').value.trim();
      if (!url) { toast('请输入测速链接'); return; }
      if (!/^https?:\/\//i.test(url)) { toast('链接需以 http:// 或 https:// 开头'); return; }
      try {
        const list = await invoke('set_speed_url', { url });
        S.cfg.speed_urls = list;
        S.cfg.speed_url = url;
        $('speedUrlInput').value = '';
        renderSpeedUrls();
        toast('已添加并设为当前测速链接');
      } catch (err) { toast('添加失败：' + err); }
    });

    $('egressSel').addEventListener('change', async (e) => {
      const iface = e.target.value;
      try {
        await invoke('set_egress_interface', { iface });
        S.cfg.egress_interface = iface;
        toast(iface && iface !== 'system'
          ? '出站网卡已绑定，若无法上网请检查 Chain 代理'
          : '出站网卡已切回跟随系统路由');
        await refreshStatus();
      } catch (err) {
        toast('切换失败：' + err);
        renderInterfaces();
      }
    });

    $('btnRefreshIf').addEventListener('click', async () => {
      try {
        S.ifaces = (await invoke('list_interfaces')) || [];
        renderInterfaces();
        toast('网卡列表已刷新');
      } catch (err) { toast('刷新失败：' + err); }
    });

    $('btnDelSpeedUrl').addEventListener('click', async () => {
      const url = $('speedUrlSel').value;
      if (!url) return;
      try {
        const list = await invoke('remove_speed_url', { url });
        S.cfg.speed_urls = list;
        if (!list.includes(S.cfg.speed_url)) S.cfg.speed_url = list[0] || '';
        renderSpeedUrls();
        toast('已删除测速链接');
      } catch (err) { toast('删除失败：' + err); }
    });
    $('logLevel').addEventListener('change', renderLogs);
    $('logFilter').addEventListener('input', renderLogs);
    $('logFollow').addEventListener('click', (e) => {
      S.follow = e.currentTarget.classList.contains('on');
    });

    document.querySelectorAll('th.sortable').forEach((th) =>
      th.addEventListener('click', () => {
        const key = th.dataset.sort;
        if (S.sort.key === key) S.sort.asc = !S.sort.asc;
        else { S.sort.key = key; S.sort.asc = false; }
        document.querySelectorAll('th.sortable').forEach((x) => {
          x.classList.remove('sorted');
          x.querySelector('.ind').textContent = '↕';
        });
        th.classList.add('sorted');
        th.querySelector('.ind').textContent = S.sort.asc ? '▲' : '▼';
        renderNodes();
      }));

    $('nodeBody').addEventListener('click', (e) => {
      const btn = e.target.closest('button[data-act]');
      if (!btn || btn.disabled) return;
      const ip = btn.dataset.ip;
      const port = btn.dataset.port;
      if (btn.dataset.act === 'bench') benchOne(ip, port);
      else useNode(ip, port);
    });

    $('swAutostart').addEventListener('click', async (e) => {
      const want = !e.currentTarget.classList.contains('on');
      e.currentTarget.classList.toggle('on', want);
      try {
        const now = await invoke('set_autostart', { enabled: want });
        e.currentTarget.classList.toggle('on', !!now);
        toast(now ? '已开启开机自启' : '已关闭开机自启');
      } catch (err) {
        e.currentTarget.classList.toggle('on', !want);
        toast('设置失败：' + err);
      }
    });
  }

  let eventsBound = false;
  async function bindEvents() {
    if (eventsBound) return;
    eventsBound = true;
    await listen('stats', (e) => onStats(e.payload));
    await listen('log', (e) => {
      S.logs.push(e.payload);
      if (S.logs.length > 3000) S.logs.shift();
      if (S.page === 'logs') renderLogs();
    });
    await listen('bench-started', (e) => {
      S.benchIp = e.payload.ip;
      renderNodes();
    });
    await listen('bench-progress', (e) => onBenchProgress(e.payload));
    await listen('bench-done', () => {
      S.bench = false;
      S.benchIp = '';
      updateBenchButtons();
      $('chartHint').textContent = '最近 60 秒';
      renderNodes();
      toast('测速完成');
    });
    await listen('nodes-changed', (e) => {
      if (S.cfg) S.cfg.nodes = e.payload;
      renderNodes();
    });
    await listen('node-updated', () => renderNodes());
    await listen('status-changed', () => refreshStatus());
    await listen('config-changed', async () => {
      try {
        S.cfg = await invoke('get_config');
        fillForm(S.cfg);
        renderNodes();
      } catch (err) { /* 忽略 */ }
    });
    await listen('tray-toggle', togglePower);
    // 托盘的「测速并自动选优」：测完自动把评分最高的节点设为当前节点
    await listen('tray-bench', () => benchAll(true));
    await listen('tray-copy', copySocks);

    // Web 控制台用 SSE 接收推送；处理器已注册完毕，此处才建立连接
    if (isWeb) listenViaSSE();
  }

  /* ---------------- 登录（仅 Web 控制台） ---------------- */

  function showGate(which) {
    if (!isWeb) return;
    const gate = $('authGate');
    if (!gate) return;
    gate.hidden = false;
    $('loginForm').hidden = which !== 'login';
    $('pwdForm').hidden = which !== 'password';
    document.body.classList.add('gated');
    setTimeout(() => {
      const el = which === 'login' ? $('loginUser') : $('pwdNew');
      if (el) el.focus();
    }, 30);
  }

  function hideGate() {
    const gate = $('authGate');
    if (gate) gate.hidden = true;
    document.body.classList.remove('gated');
  }

  async function doLogin(e) {
    if (e) e.preventDefault();
    const msg = $('loginMsg');
    msg.textContent = '';
    try {
      const res = await fetch('/api/auth/login', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username: $('loginUser').value.trim(),
          password: $('loginPass').value,
        }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) {
        msg.textContent = body.error || ('登录失败（HTTP ' + res.status + '）');
        return;
      }
      SESSION.token = body.token;
      SESSION.username = body.username || 'admin';
      localStorage.setItem(TOKEN_KEY, body.token);
      $('loginPass').value = '';
      if (body.must_change_password) { showGate('password'); return; }
      hideGate();
      await boot();
    } catch (err) {
      msg.textContent = '无法连接服务：' + err.message;
    }
  }

  async function doChangePassword(e) {
    if (e) e.preventDefault();
    const msg = $('pwdMsg');
    msg.textContent = '';
    const a = $('pwdNew').value;
    const b = $('pwdConfirm').value;
    if (a !== b) { msg.textContent = '两次输入的密码不一致'; return; }

    try {
      const res = await fetch('/api/auth/change-password', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          Authorization: 'Bearer ' + SESSION.token,
        },
        body: JSON.stringify({ new_password: a }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) { msg.textContent = body.error || '修改失败'; return; }

      SESSION.token = body.token;
      localStorage.setItem(TOKEN_KEY, body.token);
      $('pwdNew').value = '';
      $('pwdConfirm').value = '';
      toast('密码已更新');
      hideGate();
      await boot();
    } catch (err) {
      msg.textContent = '请求失败：' + err.message;
    }
  }

  /** Web 控制台管不到宿主进程的自启注册，也打不开服务器上的目录。 */
  function applyRuntimeMode() {
    if (!isWeb) return;
    const autoRow = $('swAutostart') ? $('swAutostart').closest('.row') : null;
    if (autoRow) autoRow.hidden = true;
    const dirBtn = $('btnOpenDir');
    if (dirBtn) dirBtn.hidden = true;
    const logoutRow = $('rowLogout');
    if (logoutRow) logoutRow.hidden = false;
    const tip = document.querySelector('.swtip');
    if (tip) tip.innerHTML = '关闭浏览器<br>不影响代理运行';
  }

  /* ---------------- 版本更新 ---------------- */

  const REPO_URL = 'https://github.com/sdise/cloudnproxy';
  let updateBusy = false;

  async function checkUpdate() {
    if (updateBusy) return;
    updateBusy = true;
    const btn = $('btnCheckUpdate');
    const desc = $('updateDesc');
    btn.disabled = true;
    desc.textContent = '正在检查…';

    try {
      const info = await invoke('check_update');
      if (info.has_update) {
        desc.innerHTML =
          `发现新版本 <b>${esc(info.latest)}</b>（当前 v${esc(info.current)}）· ` +
          `<a href="#" id="updateLink">前往下载</a>`;
        const link = $('updateLink');
        if (link) {
          link.addEventListener('click', (ev) => {
            ev.preventDefault();
            openUrl(info.url);
          });
        }
        toast('有新版本 ' + info.latest);
      } else {
        desc.textContent = `已是最新版本（v${info.current}）`;
      }
    } catch (e) {
      desc.textContent = '检查失败：' + (e && e.message ? e.message : e);
    } finally {
      btn.disabled = false;
      updateBusy = false;
    }
  }

  /**
   * 在浏览器中打开链接。
   *
   * 桌面版必须交给系统 —— WebView 内直接 window.open 可能被拦截；Web 控制台
   * 则相反：程序跑在服务器上，只有访问者自己的浏览器才谈得上「打开」。
   */
  function openUrl(url) {
    if (isTauri) {
      invoke('open_url', { url }).catch((e) => toast('打开失败：' + e));
    } else {
      window.open(url, '_blank', 'noopener');
    }
  }

  /* ---------------- 启动 ---------------- */

  async function init() {
    bind();
    applyRuntimeMode();

    if (isWeb) {
      $('loginForm').addEventListener('submit', doLogin);
      $('pwdForm').addEventListener('submit', doChangePassword);
      $('btnLogout').addEventListener('click', () => {
        SESSION.token = '';
        localStorage.removeItem(TOKEN_KEY);
        showGate('login');
      });

      try {
        const st = await fetch('/api/auth/state').then((r) => r.json());
        if (st.username) $('loginUser').value = st.username;
        if (st.version) S.version = st.version;
      } catch (e) { /* 服务不可达时由登录表单给出提示 */ }

      if (!SESSION.token) { showGate('login'); return; }
    }

    await boot();
  }

  /** 登录完成（或桌面版启动）后加载数据并建立订阅。 */
  async function boot() {
    try {
      const st = await invoke('get_status');
      S.status = st.engine || { running: false };
      S.egress = st.egress || null;
      S.cfgPath = st.config_path || '';
      S.version = st.version || S.version;
      S.cfg = await invoke('get_config');
      S.logs = (await invoke('get_logs')) || [];
    } catch (e) {
      // 令牌失效时 httpInvoke 已经弹出登录页，这里不要再刷一遍界面
      if (isWeb && /登录/.test(String(e && e.message))) return;
      S.cfg = { nodes: [] };
    }

    try {
      S.ifaces = (await invoke('list_interfaces')) || [];
    } catch (e) {
      S.ifaces = [];
    }

    if (S.cfg) {
      fillForm(S.cfg);
      renderNodes();
    }
    renderStatus();
    renderLogs();
    updateBenchButtons();
    await bindEvents();
    drawChart();

    try {
      const auto = await invoke('is_autostart_enabled');
      $('swAutostart').classList.toggle('on', !!auto);
    } catch (e) { /* 忽略 */ }

    const mode = isTauri ? '桌面版 · Tauri v2' : (isWeb ? 'Web 控制台' : '界面预览');
    $('aboutVer').textContent =
      'CloudNProxy v' + (S.version || '0.2.0') + ' · Rust · ' + mode;
    window.addEventListener('resize', drawChart);
  }

  /* ---------------- 浏览器预览用的降级实现 ---------------- */

  const DEMO_NODES = [
    ['14.215.182.75', '广州', '电信', '14.215.185.36', '广州', '电信', 'AS4134', 28, 127.8],
    ['183.240.98.84', '广州', '移动', '14.215.185.62', '广州', '电信', 'AS4134', 35, 113.4],
    ['163.177.17.189', '广州', '联通', '14.215.185.28', '广州', '电信', 'AS4134', 32, 71.7],
    ['110.242.70.68', '承德', '联通', '157.0.147.149', '苏州', '联通', 'AS140717', 61, 54.3],
    ['220.181.33.174', '北京', 'CNISP', '157.0.147.147', '苏州', '联通', 'AS140717', 74, 40.9],
    ['180.101.50.249', '南京', '电信', '', '', '', 'AS134756', null, null],
  ].map((r) => ({
    ip: r[0], port: 443, region: r[1], entry_isp: r[2], exit_ip: r[3],
    exit_region: r[4], exit_isp: r[5], exit_asn: r[6],
    latency_ms: r[7], speed_mbps: r[8],
    measured_at: Math.floor(Date.now() / 1000),
  }));

  const demoCfg = {
    listen_host: '127.0.0.1', listen_port: 10801, allow_lan: false,
    resolve_domain: 'cloudnproxy.baidu.com',
    upstream: '163.177.17.189:443', current_node: '163.177.17.189:443',
    fake_host: 'cloudnproxy.baidu.com', t5_auth: '1050504963', max_conns: 512,
    chain_enabled: false, chain_addr: '', egress_interface: '', connect_timeout_ms: 10000,
    tcp_nodelay: true, tunnel_pool: true, auto_reconnect: true, auto_switch: false,
    speed_urls: [
      'https://speed.cloudflare.com/__down?bytes=1000000',
      'https://speed.cloudflare.com/__down?bytes=10000000',
      'https://speed.cloudflare.com/__down?bytes=99000000',
    ],
    speed_url: 'https://speed.cloudflare.com/__down?bytes=1000000',
    log_level: 'info', log_file: '',
    autostart: false, autostart_connect: true, start_minimized: true,
    close_to_tray: true, floating: false, theme: 'system',
    nodes: DEMO_NODES,
  };

  async function mockInvoke(cmd, args) {
    await new Promise((r) => setTimeout(r, 60));
    switch (cmd) {
      case 'get_status':
        return {
          engine: { running: true, addr: '127.0.0.1:10801', upstream: demoCfg.upstream, chain: '直连' },
          stats: { up_bytes: 0, down_bytes: 0, conns: 0, sessions: 0 },
          nodes: demoCfg.nodes.length,
          resolve_domain: demoCfg.resolve_domain,
          config_path: 'C:/Users/you/AppData/Roaming/dev.cloudnproxy.app/config.toml',
          // 浏览器预览用的演示值：模拟「流量被 TUN 接管」的提示样式
          egress: { target: '163.177.17.189', interface: 'Wintun', via_tun: true },
        };
      case 'get_config': return JSON.parse(JSON.stringify(demoCfg));
      case 'apply_config': Object.assign(demoCfg, args.cfg); return demoCfg;
      case 'resolve_nodes':
        demoCfg.nodes = DEMO_NODES;
        return DEMO_NODES;
      case 'get_logs':
        return [
          { ts: Date.now() - 5000, level: 'INFO', msg: 'engine 已启动：127.0.0.1:10801 → 163.177.17.189:443（直连）' },
          { ts: Date.now() - 3000, level: 'INFO', msg: 'CONNECT speed.cloudflare.com:80 → OK (24ms)' },
          { ts: Date.now(), level: 'INFO', msg: '（浏览器预览模式：未连接后端）' },
        ];
      case 'set_speed_url': {
        if (!demoCfg.speed_urls.includes(args.url)) demoCfg.speed_urls.push(args.url);
        demoCfg.speed_url = args.url;
        return demoCfg.speed_urls.slice();
      }
      case 'remove_speed_url': {
        demoCfg.speed_urls = demoCfg.speed_urls.filter((u) => u !== args.url);
        if (!demoCfg.speed_urls.length) {
          demoCfg.speed_urls.push('https://speed.cloudflare.com/__down?bytes=1000000');
        }
        if (!demoCfg.speed_urls.includes(demoCfg.speed_url)) {
          demoCfg.speed_url = demoCfg.speed_urls[0];
        }
        return demoCfg.speed_urls.slice();
      }
      case 'list_interfaces':
        return [
          { name: 'WLAN', index: 11, ipv4: '192.168.41.159', description: 'Intel(R) Wi-Fi 6 AX200', media: 'Native 802.11', is_up: true, is_tun: false, is_virtual: false, is_loopback: false },
          { name: 'xray_tun', index: 9, ipv4: '172.18.0.1', description: 'Wintun Tunnel', media: 'IP', is_up: true, is_tun: true, is_virtual: true, is_loopback: false },
          { name: '本地连接', index: 7, ipv4: '169.254.150.30', description: 'VPN Client Adapter - VPN', media: '802.3', is_up: false, is_tun: false, is_virtual: true, is_loopback: false },
          { name: '以太网', index: 5, ipv4: '', description: 'Intel(R) I210 Gigabit', media: '802.3', is_up: false, is_tun: false, is_virtual: false, is_loopback: false },
        ];
      case 'set_egress_interface':
        demoCfg.egress_interface = args.iface;
        return null;
      case 'is_autostart_enabled': return false;
      case 'check_update':
        return {
          has_update: true,
          current: '0.2.1',
          latest: 'v0.3.0',
          url: 'https://github.com/sdise/cloudnproxy/releases',
          published_at: '',
          notes: '',
        };
      case 'benchmark_all': return demoCfg.nodes.length;
      case 'benchmark_one': return { latency_ms: 30, speed_mbps: 88.8, bytes: 0, region: '广州', entry_isp: '联通', exit_ip: '14.215.185.28', exit_region: '广州', exit_isp: '电信', exit_asn: 'AS4134' };
      default: return null;
    }
  }

  document.addEventListener('DOMContentLoaded', init);
})();
