/* CloudNProxy 前端逻辑
 * 通过 window.__TAURI__ 的全局对象调用 Rust 侧命令；在普通浏览器中打开时
 * 自动降级为演示数据，方便离线预览界面。 */
(function () {
  'use strict';

  const hasTauri = typeof window.__TAURI__ !== 'undefined' && window.__TAURI__.core;
  const invoke = hasTauri ? window.__TAURI__.core.invoke : mockInvoke;
  const listen = hasTauri ? window.__TAURI__.event.listen : async () => () => {};

  const $ = (id) => document.getElementById(id);
  const LEVELS = { TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4 };

  const S = {
    cfg: null,
    cfgPath: '',
    status: { running: false, addr: '', upstream: '', chain: '' },
    logs: [],
    hist: [],
    sort: { key: 'speed', asc: false },
    filterIsp: '',
    bench: false,
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

    const pill = $('tbState');
    pill.textContent = running ? '运行中' : '已停止';
    pill.className = 'pill ' + (running ? 'ok' : 'mut');

    $('sbDot').classList.toggle('off', !running);
    $('sbState').textContent = running ? '运行中' : '已停止';
    $('sbNode').textContent = '节点 ' + upstream + (chain && chain !== '直连' ? '（经 ' + chain + '）' : ' · 直连');

    $('chainLocal').textContent = addr;
    $('chainNode').textContent = upstream;
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
      const exit = [n.exit_isp, n.exit_asn].filter(Boolean).join(' · ');
      return `<tr class="${isCur ? 'cur' : ''}">
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
      if (S.cfg && Array.isArray(st.nodes) === false) { /* noop */ }
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

  async function benchAll() {
    if (S.bench) { toast('测速已在进行中'); return; }
    try {
      const total = await invoke('benchmark_all', { onlyMissing: false });
      S.bench = true;
      updateBenchButtons();
      toast('开始测速，共 ' + total + ' 个节点');
    } catch (e) {
      toast('无法开始测速：' + e);
    }
  }

  function onBenchProgress(p) {
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
      renderNodes();
      toast(r.speed_mbps != null
        ? `${ip} → ${r.speed_mbps.toFixed(2)} Mbps / ${r.latency_ms} ms`
        : `${ip} → 失败：${r.error || '无数据'}`);
    } catch (e) {
      toast('测速失败：' + e);
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

    $('themeSel').addEventListener('change', () => applyTheme($('themeSel').value));
    $('filterIsp').addEventListener('change', (e) => { S.filterIsp = e.target.value; renderNodes(); });
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

  async function bindEvents() {
    await listen('stats', (e) => onStats(e.payload));
    await listen('log', (e) => {
      S.logs.push(e.payload);
      if (S.logs.length > 3000) S.logs.shift();
      if (S.page === 'logs') renderLogs();
    });
    await listen('bench-progress', (e) => onBenchProgress(e.payload));
    await listen('bench-done', () => {
      S.bench = false;
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
    await listen('tray-bench', benchAll);
    await listen('tray-copy', copySocks);
  }

  async function init() {
    bind();
    try {
      const st = await invoke('get_status');
      S.status = st.engine || { running: false };
      S.cfgPath = st.config_path || '';
      S.cfg = await invoke('get_config');
      S.logs = (await invoke('get_logs')) || [];
    } catch (e) {
      S.cfg = { nodes: [] };
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

    $('aboutVer').textContent = 'CloudNProxy v1.0.0 · Rust + Tauri v2';
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
    chain_enabled: false, chain_addr: '', connect_timeout_ms: 10000,
    tcp_nodelay: true, tunnel_pool: true, auto_reconnect: true, auto_switch: false,
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
      case 'is_autostart_enabled': return false;
      case 'benchmark_all': return demoCfg.nodes.length;
      case 'benchmark_one': return { latency_ms: 30, speed_mbps: 88.8, bytes: 0, region: '广州', entry_isp: '联通', exit_ip: '14.215.185.28', exit_region: '广州', exit_isp: '电信', exit_asn: 'AS4134' };
      default: return null;
    }
  }

  document.addEventListener('DOMContentLoaded', init);
})();
