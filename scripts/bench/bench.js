/*
 * SOCKS5 load client.
 *
 * Opens N connections to the t5d inbound port and keeps them alive for the whole
 * scenario, so what gets measured is steady-state resource usage rather than the
 * cost of a connection storm.
 *
 *   idle  handshake, then one byte up and down every 5 seconds
 *   bulk  handshake, then continuously drain downstream data
 *
 * Usage: node bench.js <port> <connections> <mode> <seconds>
 */
'use strict';

const net = require('net');

const port = Number(process.argv[2] || 18080);
const N = Number(process.argv[3] || 100);
const mode = process.argv[4] || 'idle';
const seconds = Number(process.argv[5] || 60);

// Placeholder target - the mock node never resolves it
const TARGET_HOST = '127.0.0.1';
const TARGET_PORT = 19998;

let established = 0;
let failed = 0;
let bytesIn = 0;
let running = true;
const socks = [];

function connectOne() {
  return new Promise((resolve) => {
    const s = net.createConnection({ host: '127.0.0.1', port }, () => {
      // SOCKS5 greeting: version 5, one method, no auth
      s.write(Buffer.from([0x05, 0x01, 0x00]));
    });
    s.setNoDelay(true);

    let stage = 0;

    s.on('data', (d) => {
      if (!running) return;

      if (stage === 0) {
        // Method selection reply: 05 00
        if (d[0] !== 0x05 || d[1] !== 0x00) {
          failed++;
          s.destroy();
          return;
        }
        stage = 1;
        // CONNECT request, ATYP=1 (IPv4)
        const req = Buffer.alloc(10);
        req[0] = 0x05;
        req[1] = 0x01;
        req[2] = 0x00;
        req[3] = 0x01;
        const oct = TARGET_HOST.split('.').map(Number);
        req[4] = oct[0];
        req[5] = oct[1];
        req[6] = oct[2];
        req[7] = oct[3];
        req.writeUInt16BE(TARGET_PORT, 8);
        s.write(req);
        return;
      }

      if (stage === 1) {
        // CONNECT reply: 10 bytes, second byte is the reply code
        if (d[1] !== 0x00) {
          failed++;
          s.destroy();
          return;
        }
        stage = 2;
        established++;
        resolve();

        if (mode === 'bulk') {
          s.on('data', (x) => {
            bytesIn += x.length;
          });
        } else {
          const timer = setInterval(() => {
            if (!running || s.destroyed) return clearInterval(timer);
            s.write(Buffer.from([0x7a]));
          }, 5000);
          timer.unref();
        }
        return;
      }

      bytesIn += d.length;
    });

    s.on('error', () => {
      failed++;
    });
    socks.push(s);
  });
}

async function main() {
  console.log(`starting ${N} connections -> 127.0.0.1:${port}  mode=${mode}  ${seconds}s`);

  // Connect in batches so the accept queue is not overwhelmed at once
  const batch = 50;
  for (let i = 0; i < N; i += batch) {
    const jobs = [];
    for (let j = 0; j < Math.min(batch, N - i); j++) jobs.push(connectOne());
    await Promise.all(jobs);
  }
  console.log(`established ${established} / ${N} (failed ${failed})`);

  const t0 = Date.now();
  const ticker = setInterval(() => {
    const el = ((Date.now() - t0) / 1000).toFixed(0);
    console.log(
      `[bench] ${el}s  established=${established} downstream=${(bytesIn / 1048576).toFixed(1)}MB`
    );
  }, 5000);

  await new Promise((r) => setTimeout(r, seconds * 1000));

  running = false;
  clearInterval(ticker);

  console.log(
    JSON.stringify({
      established,
      failed,
      bytesIn,
      megabitsPerSec: (bytesIn * 8) / seconds / 1e6,
    })
  );

  for (const s of socks) s.destroy();
  setTimeout(() => process.exit(0), 500);
}

main();
