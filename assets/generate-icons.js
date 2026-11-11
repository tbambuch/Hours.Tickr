'use strict';

// Generates all icons programmatically (no external assets): node assets/generate-icons.js
// Output: icon.png, icon.ico, icon.icns, tray-idle*.png, tray-recording*.png, trayTemplate*.png, trayRecordingTemplate*.png

const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');

const OUT = __dirname;

// ---- minimal PNG encoder ----
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function encodePng(size, rgba) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// ---- tiny anti-aliased shape renderer (coordinates in 0..1 units) ----
const circle = (cx, cy, r) => (x, y) => (x - cx) ** 2 + (y - cy) ** 2 <= r * r;
const ring = (cx, cy, r, w) => (x, y) => {
  const d = Math.hypot(x - cx, y - cy);
  return d <= r + w / 2 && d >= r - w / 2;
};
const roundRect = (x0, y0, x1, y1, rad) => (x, y) => {
  const qx = Math.max(x0 + rad - x, 0, x - (x1 - rad));
  const qy = Math.max(y0 + rad - y, 0, y - (y1 - rad));
  return x >= x0 && x <= x1 && y >= y0 && y <= y1 && qx * qx + qy * qy <= rad * rad;
};
const line = (ax, ay, bx, by, w) => (x, y) => {
  const dx = bx - ax;
  const dy = by - ay;
  const t = Math.max(0, Math.min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(x - (ax + t * dx), y - (ay + t * dy)) <= w / 2;
};

function render(size, layers) {
  const buf = Buffer.alloc(size * size * 4);
  const SS = size >= 128 ? 3 : 4;
  for (let py = 0; py < size; py++) {
    for (let px = 0; px < size; px++) {
      let r = 0, g = 0, b = 0, a = 0; // premultiplied accumulation, painter's algorithm
      for (const { test, color } of layers) {
        let hit = 0;
        for (let sy = 0; sy < SS; sy++)
          for (let sx = 0; sx < SS; sx++) if (test((px + (sx + 0.5) / SS) / size, (py + (sy + 0.5) / SS) / size)) hit++;
        const cov = (hit / (SS * SS)) * (color[3] / 255);
        if (cov === 0) continue;
        r = color[0] * cov + r * (1 - cov);
        g = color[1] * cov + g * (1 - cov);
        b = color[2] * cov + b * (1 - cov);
        a = cov + a * (1 - cov);
      }
      const o = (py * size + px) * 4;
      if (a > 0) {
        buf[o] = Math.round(r / a);
        buf[o + 1] = Math.round(g / a);
        buf[o + 2] = Math.round(b / a);
        buf[o + 3] = Math.round(a * 255);
      }
    }
  }
  return buf;
}

// ---- designs ----
const RED = [229, 57, 53, 255];

function clockLayers(color, { dot = false, ringW = 0.11 } = {}) {
  const layers = [
    { test: ring(0.5, 0.5, 0.38, ringW), color },
    { test: line(0.5, 0.5, 0.5, 0.26, ringW * 0.9), color },
    { test: line(0.5, 0.5, 0.67, 0.58, ringW * 0.9), color },
  ];
  if (dot) {
    layers.push({ test: circle(0.78, 0.78, 0.22), color: RED });
  }
  return layers;
}

function appIconLayers() {
  return [
    { test: roundRect(0.04, 0.04, 0.96, 0.96, 0.22), color: [38, 50, 92, 255] },
    { test: ring(0.5, 0.5, 0.3, 0.07), color: [255, 255, 255, 255] },
    { test: line(0.5, 0.5, 0.5, 0.31, 0.065), color: [255, 255, 255, 255] },
    { test: line(0.5, 0.5, 0.64, 0.57, 0.065), color: [255, 255, 255, 255] },
    { test: circle(0.74, 0.74, 0.13), color: [38, 50, 92, 255] },
    { test: circle(0.74, 0.74, 0.1), color: RED },
  ];
}

function write(name, buf) {
  fs.writeFileSync(path.join(OUT, name), buf);
  console.log('wrote', name, buf.length, 'bytes');
}

const GRAY = [150, 156, 166, 255];
const BLACK = [0, 0, 0, 255];

for (const [suffix, size] of [['', 16], ['@2x', 32]]) {
  write(`tray-idle${suffix}.png`, encodePng(size, render(size, clockLayers(GRAY))));
  write(`tray-recording${suffix}.png`, encodePng(size, render(size, clockLayers(GRAY, { dot: true }))));
  // macOS template images: black + alpha only
  write(`trayTemplate${suffix}.png`, encodePng(size, render(size, clockLayers(BLACK))));
  const recTpl = [
    ...clockLayers(BLACK),
    { test: circle(0.78, 0.78, 0.22), color: BLACK },
  ];
  write(`trayRecordingTemplate${suffix}.png`, encodePng(size, render(size, recTpl)));
}

const sizes = [16, 32, 48, 64, 128, 256, 512];
const pngs = Object.fromEntries(sizes.map((s) => [s, encodePng(s, render(s, appIconLayers()))]));
write('icon.png', pngs[512]);

// ICO with PNG-compressed images (supported since Windows Vista)
{
  const icoSizes = [16, 32, 48, 64, 128, 256];
  const head = Buffer.alloc(6);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(icoSizes.length, 4);
  const dir = Buffer.alloc(16 * icoSizes.length);
  let offset = 6 + dir.length;
  icoSizes.forEach((s, i) => {
    const o = i * 16;
    dir[o] = s >= 256 ? 0 : s;
    dir[o + 1] = s >= 256 ? 0 : s;
    dir.writeUInt16LE(1, o + 4); // planes
    dir.writeUInt16LE(32, o + 6); // bpp
    dir.writeUInt32LE(pngs[s].length, o + 8);
    dir.writeUInt32LE(offset, o + 12);
    offset += pngs[s].length;
  });
  write('icon.ico', Buffer.concat([head, dir, ...icoSizes.map((s) => pngs[s])]));
}

// ICNS with PNG payloads (ic07=128, ic08=256, ic09=512)
{
  const entries = [['ic07', 128], ['ic08', 256], ['ic09', 512]].map(([type, s]) => {
    const h = Buffer.alloc(8);
    h.write(type, 0, 'ascii');
    h.writeUInt32BE(8 + pngs[s].length, 4);
    return Buffer.concat([h, pngs[s]]);
  });
  const body = Buffer.concat(entries);
  const h = Buffer.alloc(8);
  h.write('icns', 0, 'ascii');
  h.writeUInt32BE(8 + body.length, 4);
  write('icon.icns', Buffer.concat([h, body]));
}
