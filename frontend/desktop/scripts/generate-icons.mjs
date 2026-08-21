import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { Resvg } from "@resvg/resvg-js";

const outputDirectory = path.resolve(import.meta.dirname, "../assets");
const markPath = path.resolve(import.meta.dirname, "../../../images/mark.svg");
const nativeIconsDirectory = path.resolve(
  import.meta.dirname,
  "../../../crates/agentdesktop/icons",
);
const androidBackground = `<?xml version="1.0" encoding="utf-8"?>
<resources>
  <color name="ic_launcher_background">#8023C3</color>
</resources>
`;

if (process.argv.includes("--finalize")) {
  writeFileSync(
    path.join(
      nativeIconsDirectory,
      "android/values/ic_launcher_background.xml",
    ),
    androidBackground,
  );
  process.exit(0);
}

const mark = readFileSync(markPath, "utf8");
const markViewBox = mark.match(/\bviewBox="([^"]+)"/)?.[1];

if (!markViewBox) {
  throw new Error(`${markPath} does not define a viewBox`);
}

const markBody = mark.replace(/^<svg\b[^>]*>/, "").replace(/<\/svg>\s*$/, "");
const glyph = markBody;
const trayGlyph = glyph.replaceAll('fill="white"', 'fill="#8023C3"');

function render(svg, size) {
  return new Resvg(svg, {
    fitTo: { mode: "width", value: size },
  })
    .render()
    .asPng();
}

function appIcon(size) {
  const svg = `<svg width="${size}" height="${size}" viewBox="${markViewBox}" xmlns="http://www.w3.org/2000/svg">
    <rect width="100%" height="100%" fill="#8023C3"/>
    ${glyph}
  </svg>`;
  return render(svg, size);
}

function statusBadge(state) {
  if (state === "ready") return "";
  const fill = state === "offline" ? "#CF222E" : "#9A6700";
  const cutout =
    state === "offline"
      ? '<path d="M22.7 22.7 29.3 29.3M29.3 22.7 22.7 29.3" stroke="black" stroke-width="1.8" stroke-linecap="round"/>'
      : '<path d="M26 22.4V26.2M26 28.7v.1" stroke="black" stroke-width="1.8" stroke-linecap="round"/>';
  return `<defs>
      <mask id="status-badge">
        <circle cx="26" cy="26" r="6" fill="white"/>
        ${cutout}
      </mask>
    </defs>
    <circle cx="26" cy="26" r="6" fill="${fill}" stroke="white" stroke-width="1.4" mask="url(#status-badge)"/>`;
}

function trayIcon(size, state = "ready") {
  const svg = `<svg width="${size}" height="${size}" viewBox="0 0 32 32" xmlns="http://www.w3.org/2000/svg">
    <svg x="-4" y="-4" width="40" height="40" viewBox="${markViewBox}" preserveAspectRatio="xMidYMid meet">
      ${trayGlyph}
    </svg>
    ${statusBadge(state)}
  </svg>`;
  return render(svg, size);
}

mkdirSync(outputDirectory, { recursive: true });
writeFileSync(path.join(outputDirectory, "tray-icon@2x.png"), trayIcon(32));
writeFileSync(
  path.join(outputDirectory, "tray-icon-attention.png"),
  trayIcon(32, "attention"),
);
writeFileSync(
  path.join(outputDirectory, "tray-icon-offline.png"),
  trayIcon(32, "offline"),
);
writeFileSync(path.join(outputDirectory, "app-icon.png"), appIcon(1024));
