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

function markColorAt(pointX, pointY) {
  const strokeRadius = 1.4;
  const onStructure =
    distanceToSegment(pointX, pointY, 9, 7.6, 9, 24.4) <= strokeRadius ||
    distanceToSegment(pointX, pointY, 23, 7.6, 23, 24.4) <= strokeRadius ||
    distanceToSegment(pointX, pointY, 9, 11.75, 23, 11.75) <= strokeRadius ||
    distanceToSegment(pointX, pointY, 9, 20.25, 23, 20.25) <= strokeRadius ||
    Math.hypot(pointX - 9, pointY - 7.6) <= 2.5 ||
    Math.hypot(pointX - 23, pointY - 24.4) <= 2.5;
  const onCenter = Math.hypot(pointX - 16, pointY - 16) <= 2.25;

  if (onCenter) return [91, 22, 142];
  if (onStructure) return [128, 35, 195];
  return null;
}

function colorAt(pointX, pointY, state) {
  if (state !== "ready") {
    const distance = Math.hypot(pointX - 25.5, pointY - 25.5);
    if (distance <= 6.6 && distance > 5.5) return null;
    if (distance <= 5.5) {
      const cutout = state === "offline"
        ? Math.abs(pointX - pointY) <= 0.8 || Math.abs(pointX + pointY - 51) <= 0.8
        : (Math.abs(pointX - 25.5) <= 0.75 && pointY >= 21.7 && pointY <= 25.8) ||
          Math.hypot(pointX - 25.5, pointY - 28.2) <= 0.85;
      if (cutout) return null;
      return state === "offline" ? [207, 34, 46] : [154, 103, 0];
    }
  }
  return markColorAt(pointX, pointY);
}

function createIcon(size, state = "ready") {
  const scanlineLength = size * 4 + 1;
  const pixels = Buffer.alloc(scanlineLength * size);
  const samplesPerAxis = 4;
  const scale = 32 / size;

  for (let pixelY = 0; pixelY < size; pixelY += 1) {
    const rowOffset = pixelY * scanlineLength;
    pixels[rowOffset] = 0;

    for (let pixelX = 0; pixelX < size; pixelX += 1) {
      const accumulated = [0, 0, 0];
      let coveredSamples = 0;

      for (let sampleY = 0; sampleY < samplesPerAxis; sampleY += 1) {
        for (let sampleX = 0; sampleX < samplesPerAxis; sampleX += 1) {
          const pointX = (pixelX + (sampleX + 0.5) / samplesPerAxis) * scale;
          const pointY = (pixelY + (sampleY + 0.5) / samplesPerAxis) * scale;
          const color = colorAt(pointX, pointY, state);
          if (color) {
            coveredSamples += 1;
            accumulated[0] += color[0];
            accumulated[1] += color[1];
            accumulated[2] += color[2];
          }
        }
      }

      const pixelOffset = rowOffset + 1 + pixelX * 4;
      pixels[pixelOffset] = coveredSamples ? Math.round(accumulated[0] / coveredSamples) : 0;
      pixels[pixelOffset + 1] = coveredSamples ? Math.round(accumulated[1] / coveredSamples) : 0;
      pixels[pixelOffset + 2] = coveredSamples ? Math.round(accumulated[2] / coveredSamples) : 0;
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
writeFileSync(path.join(outputDirectory, "tray-icon-attention.png"), createIcon(32, "attention"));
writeFileSync(path.join(outputDirectory, "tray-icon-offline.png"), createIcon(32, "offline"));
writeFileSync(path.join(outputDirectory, "app-icon.png"), createIcon(1024));
