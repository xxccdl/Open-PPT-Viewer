/**
 * 只读 zip 的极简实现。
 *
 * # 为什么不装依赖
 *
 * 服务端零依赖是这套东西的底线（见 server.mjs 开头）。后台只需要
 * 「列出包里有什么」和「读一个小文本文件」两件事，为此拉一个 unzip
 * 库不划算。这里按 ZIP 的中央目录走一遍，用 Node 自带的 zlib 解压。
 *
 * 支持的压缩方式：0（存储）、8（deflate）—— 客户端打出来的包就是这两种。
 */
import { promises as fs } from 'node:fs';
import zlib from 'node:zlib';

const EOCD_SIG = 0x06054b50;
const CEN_SIG = 0x02014b50;
const LOC_SIG = 0x04034b50;

/** 中央目录要整段读进来，给个上限：真有 20 万个文件也不是给人看的。 */
const MAX_ENTRIES = 20000;

export class Zip {
  constructor(fh, entries) {
    this.fh = fh;
    this.entries = entries;
  }

  static async open(file) {
    const fh = await fs.open(file, 'r');
    const { size } = await fh.stat();
    // EOCD 在末尾，后面可能跟着最多 64KB 的注释
    const tailLen = Math.min(size, 65557);
    const tail = Buffer.alloc(tailLen);
    await fh.read(tail, 0, tailLen, size - tailLen);

    let eocd = -1;
    for (let i = tail.length - 22; i >= 0; i--) {
      if (tail.readUInt32LE(i) === EOCD_SIG) {
        eocd = i;
        break;
      }
    }
    if (eocd < 0) {
      await fh.close();
      throw new Error('不是 zip（找不到中央目录）');
    }

    const count = tail.readUInt16LE(eocd + 10);
    const cenSize = tail.readUInt32LE(eocd + 12);
    const cenOffset = tail.readUInt32LE(eocd + 16);
    if (count > MAX_ENTRIES) {
      await fh.close();
      throw new Error('文件太多，不像是一个报告包');
    }

    const cen = Buffer.alloc(cenSize);
    await fh.read(cen, 0, cenSize, cenOffset);

    const entries = [];
    let p = 0;
    for (let i = 0; i < count && p + 46 <= cen.length; i++) {
      if (cen.readUInt32LE(p) !== CEN_SIG) break;
      const flags = cen.readUInt16LE(p + 8);
      const method = cen.readUInt16LE(p + 10);
      const compressedSize = cen.readUInt32LE(p + 20);
      const size_ = cen.readUInt32LE(p + 24);
      const nameLen = cen.readUInt16LE(p + 28);
      const extraLen = cen.readUInt16LE(p + 30);
      const commentLen = cen.readUInt16LE(p + 32);
      const localOffset = cen.readUInt32LE(p + 42);
      // 有些打包工具（Windows 的 Compress-Archive 就是）用反斜杠当分隔符，
      // 统一成 `/`，否则按 `logs/openpptview.log` 去读会读不到。
      const name = cen.toString('utf8', p + 46, p + 46 + nameLen).replace(/\\/g, '/');
      // 目录项（以 / 结尾）不用出现
      if (!name.endsWith('/')) {
        entries.push({
          name,
          size: size_,
          compressedSize,
          method,
          localOffset,
          encrypted: (flags & 0x1) !== 0,
        });
      }
      p += 46 + nameLen + extraLen + commentLen;
    }

    return new Zip(fh, entries);
  }

  /** 读一个条目；超过 maxBytes 的只读前 maxBytes（日志用得上）。 */
  async read(name, maxBytes = 4 * 1024 * 1024) {
    const entry = this.entries.find((e) => e.name === name);
    if (!entry) return null;
    if (entry.encrypted) throw new Error('加密的 zip 读不了');

    const head = Buffer.alloc(30);
    await this.fh.read(head, 0, 30, entry.localOffset);
    if (head.readUInt32LE(0) !== LOC_SIG) throw new Error('zip 结构不对（本地头）');
    const nameLen = head.readUInt16LE(26);
    const extraLen = head.readUInt16LE(28);
    const start = entry.localOffset + 30 + nameLen + extraLen;

    const want = Math.min(entry.compressedSize, maxBytes + 64 * 1024);
    const raw = Buffer.alloc(want);
    await this.fh.read(raw, 0, want, start);

    let out;
    if (entry.method === 0) out = raw;
    else if (entry.method === 8) out = zlib.inflateRawSync(raw);
    else throw new Error(`不支持的压缩方式 ${entry.method}`);

    return out.subarray(0, Math.min(out.length, maxBytes));
  }

  async readText(name, maxBytes) {
    const buf = await this.read(name, maxBytes);
    if (!buf) return null;
    return buf.toString('utf8');
  }

  list() {
    return this.entries.map((e) => ({ name: e.name, size: e.size }));
  }

  async close() {
    await this.fh.close().catch(() => {});
  }
}

/** 给「一次调用就完事」的场景用。 */
export async function withZip(file, fn) {
  const zip = await Zip.open(file);
  try {
    return await fn(zip);
  } finally {
    await zip.close();
  }
}
