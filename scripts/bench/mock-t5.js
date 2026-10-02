/*
 * Mock T5 node - isolates the load test from the outside world.
 *
 * A real T5 node answers a CONNECT carrying the fake Host and X-T5-Auth header with
 * "200 Connection Established", then blindly relays. This mock does exactly the same
 * handshake but never dials out: it just pushes data back towards t5d at a
 * configurable rate. That covers the full protocol path while removing external
 * bandwidth and node availability from the equation.
 *
 * Usage: node mock-t5.js <port> <mode> [rateKb]
 *   mode = idle  reply 200, then stay quiet (client drives the traffic)
 *   mode = sink  reply 200, then stream data downstream
 *   rateKb       per-connection downstream cap in KB/s, 0 = unlimited
 */
'use strict';

const net = require('net');

const port = Number(process.argv[2] || 19990);
const mode = process.argv[3] || 'sink';
const rateKb = Number(process.argv[4] || 0);

let conns = 0;
let totalOut = 0;

// 64 KiB per write, close to what real bulk transfer looks like
const CHUNK = Buffer.alloc(64 * 1024, 0x41);

const server = net.createServer((sock) => {
  conns++;
  sock.setNoDelay(true);
  let closed = false;
  let pending = Buffer.alloc(0);
  let established = false;

  sock.on('data', (d) => {
    if (established) {
      // Tunnel is up: upstream bytes from t5d are simply dropped
      return;
    }
    pending = Buffer.concat([pending, d]);
    if (pending.indexOf('\r\n\r\n') < 0) return;

    established = true;
    pending = Buffer.alloc(0);
    sock.write('HTTP/1.1 200 Connection Established\r\n\r\n');

    if (mode === 'sink') pump(sock, () => closed);
  });

  const done = () => {
    if (!closed) {
      closed = true;
      conns--;
      sock.destroy();
    }
  };
  sock.on('error', done);
  sock.on('close', done);
});

function pump(sock, isClosed) {
  const write = () => {
    if (isClosed() || sock.destroyed) return;
    if (!sock.write(CHUNK)) {
      // Kernel buffer full: wait for drain instead of queueing in Node memory
      sock.once('drain', write);
      return;
    }
    totalOut += CHUNK.length;
    if (rateKb > 0) {
      setTimeout(write, (CHUNK.length / 1024 / rateKb) * 1000);
    } else {
      setImmediate(write);
    }
  };
  write();
}

server.listen(port, '127.0.0.1', () => {
  console.log(`mock-t5 listening on 127.0.0.1:${port} mode=${mode} rate=${rateKb}KB/s`);
});

// Per-second throughput report, handy to confirm the load is real
setInterval(() => {
  console.log(`[mock] conns=${conns} out=${(totalOut / 1048576).toFixed(1)}MB`);
  totalOut = 0;
}, 1000).unref();

for (const sig of ['SIGTERM', 'SIGINT']) {
  process.on(sig, () => {
    server.close();
    process.exit(0);
  });
}
