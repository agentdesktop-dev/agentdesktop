import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { deflateSync } from "node:zlib";

const outputDirectory = path.resolve("assets");

function crc32(buffer) {
  let crc = 0xffffffff;

  for (const byte of buffer) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
    }
  }

  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const typeBuffer = Buffer.from(type, "ascii");
  const length = Buffer.alloc(4);
  const checksum = Buffer.alloc(4);

  length.writeUInt32BE(data.length);
  checksum.writeUInt32BE(crc32(Buffer.concat([typeBuffer, data])));

  return Buffer.concat([length, typeBuffer, data, checksum]);
}

function distanceToSegment(pointX, pointY, startX, startY, endX, endY) {
  const segmentX = endX - startX;
  const segmentY = endY - startY;
  const projection = Math.max(
    0,
    Math.min(
      1,
      ((pointX - startX) * segmentX + (pointY - startY) * segmentY) /
        (segmentX * segmentX + segmentY * segmentY)
    )
  );
  const deltaX = pointX - (startX + projection * segmentX);
  const deltaY = pointY - (startY + projection * segmentY);

  return Math.hypot(deltaX, deltaY);
}

function alphaAt(pointX, pointY) {
  const center = 16;
  const inDisc = Math.hypot(pointX - center, pointY - center) <= 14;
  const inLeftStroke = distanceToSegment(pointX, pointY, 9.5, 24, 16, 7.5) <= 1.4;
  const inRightStroke = distanceToSegment(pointX, pointY, 16, 7.5, 22.5, 24) <= 1.4;
  const inCrossbar = distanceToSegment(pointX, pointY, 12, 18.5, 20, 18.5) <= 1.15;

  return inDisc && !(inLeftStroke || inRightStroke || inCrossbar);
}

function createIcon(size) {
  const scanlineLength = size * 4 + 1;
  const pixels = Buffer.alloc(scanlineLength * size);
  const samplesPerAxis = 4;
  const scale = 32 / size;

  for (let pixelY = 0; pixelY < size; pixelY += 1) {
    const rowOffset = pixelY * scanlineLength;
    pixels[rowOffset] = 0;

    for (let pixelX = 0; pixelX < size; pixelX += 1) {
      let coveredSamples = 0;

      for (let sampleY = 0; sampleY < samplesPerAxis; sampleY += 1) {
        for (let sampleX = 0; sampleX < samplesPerAxis; sampleX += 1) {
          const pointX = (pixelX + (sampleX + 0.5) / samplesPerAxis) * scale;
          const pointY = (pixelY + (sampleY + 0.5) / samplesPerAxis) * scale;
          coveredSamples += alphaAt(pointX, pointY) ? 1 : 0;
        }
      }

      const pixelOffset = rowOffset + 1 + pixelX * 4;
      pixels[pixelOffset] = 20;
      pixels[pixelOffset + 1] = 122;
      pixels[pixelOffset + 2] = 114;
      pixels[pixelOffset + 3] = Math.round(
        (coveredSamples / (samplesPerAxis * samplesPerAxis)) * 255
      );
    }
  }

  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8;
  header[9] = 6;

  return Buffer.concat([
    Buffer.from("89504e470d0a1a0a", "hex"),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(pixels)),
    chunk("IEND", Buffer.alloc(0))
  ]);
}

mkdirSync(outputDirectory, { recursive: true });
writeFileSync(path.join(outputDirectory, "tray-icon.png"), createIcon(16));
writeFileSync(path.join(outputDirectory, "tray-icon@2x.png"), createIcon(32));
writeFileSync(path.join(outputDirectory, "app-icon.png"), createIcon(1024));