#!/usr/bin/env node
/**
 * 生成应用图标的源 PNG（1024×1024），零第三方依赖。
 *
 * 之所以不直接提交 .png/.ico：二进制文件不适合入库，且本地可能没有
 * ImageMagick。本脚本只用 Node 内置的 zlib 手写 PNG 编码，
 * 由 CI 在构建前调用，再用 `tauri icon` 生成全套平台图标。
 *
 * 用法： node scripts/gen-icon.js [输出路径]
 */

const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const SIZE = 1024;
const OUT = process.argv[2] || 'icon-source.png';

// ---------- CRC32 ----------
let crcTable = null;
function crc32(buf) {
  if (!crcTable) {
    crcTable = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      crcTable[n] = c;
    }
  }
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

// ---------- 绘制 ----------
const cx = SIZE / 2;
const cy = SIZE / 2;
const R_OUT = SIZE * 0.355; // 圆环外半径
const R_IN = SIZE * 0.275; // 圆环内半径
const R_DOT = SIZE * 0.115; // 中心圆半径
const R_FRAME = SIZE * 0.455; // 外框（深色底）半径

/** 平滑过渡，返回 0..1 的覆盖度 */
function smooth(edge, d, soft) {
  return Math.max(0, Math.min(1, (edge - d) / soft + 0.5));
}

function pixel(x, y) {
  const dx = x + 0.5 - cx;
  const dy = y + 0.5 - cy;
  const d = Math.sqrt(dx * dx + dy * dy);
  const soft = 2.0;

  // 底色：深蓝 → 亮蓝竖向渐变
  const t = y / SIZE;
  let r = Math.round(0x0a + (0x12 - 0x0a) * t);
  let g = Math.round(0x3d + (0x5a - 0x3d) * t);
  let b = Math.round(0x6b + (0x9a - 0x6b) * t);

  const inFrame = smooth(R_FRAME, d, soft);
  if (inFrame <= 0) return [0, 0, 0, 0];

  // 圆环与中心点用亮青色
  const ring = smooth(R_OUT, d, soft) * smooth(d, R_IN, soft);
  const dot = smooth(R_DOT, d, soft);
  const bright = Math.max(ring, dot);

  if (bright > 0) {
    r = Math.round(r + (0x7d - r) * bright);
    g = Math.round(g + (0xe7 - g) * bright);
    b = Math.round(b + (0xff - b) * bright);
  }

  return [r, g, b, Math.round(255 * inFrame)];
}

function buildPng() {
  const stride = SIZE * 4 + 1;
  const raw = Buffer.alloc(stride * SIZE);
  for (let y = 0; y < SIZE; y++) {
    const row = y * stride;
    raw[row] = 0; // filter: none
    for (let x = 0; x < SIZE; x++) {
      const [r, g, b, a] = pixel(x, y);
      const p = row + 1 + x * 4;
      raw[p] = r;
      raw[p + 1] = g;
      raw[p + 2] = b;
      raw[p + 3] = a;
    }
  }

  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(SIZE, 0);
  ihdr.writeUInt32BE(SIZE, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', idat),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const outPath = path.resolve(OUT);
fs.writeFileSync(outPath, buildPng());
console.log(`已生成图标源文件: ${outPath} (${SIZE}x${SIZE})`);
