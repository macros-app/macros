import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { ImageResponse } from "next/og";
import { RAMP, INK, rng } from "../_spectrum/textplot";

/* The link-preview card (og:image / twitter:image, wired up in the root
   layout). Rendered once at build time into out/og.png, so it stays a static
   file like the rest of the site. A route rather than an opengraph-image file
   because static export writes those without an extension, and Cloudflare
   then serves them as octet-stream. Same art as the hero: a viridis heat
   field of half-block cells under the tagline, set in the editor's own Lilex. */

export const dynamic = "force-static";

const size = { width: 1200, height: 630 };

const COLS = 60;
const ROWS = 15;
const CELL = size.width / COLS;

/** Two warm hotspots plus a ripple, seeded so every build draws the same field. */
function field(): string[][] {
  const r = rng(7);
  const raw = Array.from({ length: ROWS }, (_, y) =>
    Array.from({ length: COLS }, (_, x) => {
      const a = Math.exp(-((x - 16) ** 2) / 70 - ((y - 10) ** 2) / 20);
      const b = 0.85 * Math.exp(-((x - 44) ** 2) / 55 - ((y - 5) ** 2) / 16);
      return a + b + 0.12 * Math.sin(x * 0.45 + y * 0.8) + r() * 0.08;
    }),
  );
  const flat = raw.flat();
  const lo = Math.min(...flat);
  const hi = Math.max(...flat);
  return raw.map((row) => row.map((v) => RAMP[Math.min(7, Math.floor(((v - lo) / (hi - lo)) * 8))]));
}

export function GET() {
  // Build cwd is web/; the editor's fonts and brand mark live in the monorepo.
  const lilex = readFileSync(resolve(process.cwd(), "..", "assets/fonts/Lilex-Regular.ttf"));
  const mark = readFileSync(resolve(process.cwd(), "public/brand/logo.svg")).toString("base64");
  const cells = field();

  return new ImageResponse(
    (
      <div
        style={{
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          background: INK,
          color: "#f2eff7",
          fontFamily: "Lilex",
        }}
      >
        <div style={{ display: "flex", flexDirection: "column", flex: 1, padding: "56px 72px 0" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 18 }}>
            <img src={`data:image/svg+xml;base64,${mark}`} width={64} height={64} />
            <span style={{ fontSize: 36, letterSpacing: 1 }}>(macros)</span>
          </div>
          <div style={{ display: "flex", flexDirection: "column", marginTop: 44, fontSize: 76, lineHeight: 1.08 }}>
            <span>Emacs ideas.</span>
            <span style={{ color: RAMP[7] }}>A modern engine.</span>
          </div>
          <div style={{ display: "flex", marginTop: 30, fontSize: 28, color: "#b9b2c8" }}>
            Rust · Steel Scheme · spreadsheets · text charts
          </div>
        </div>
        <div style={{ display: "flex", flexDirection: "column" }}>
          {cells.map((row, y) => (
            <div key={y} style={{ display: "flex" }}>
              {row.map((c, x) => (
                <div key={x} style={{ width: CELL, height: CELL / 2, background: c }} />
              ))}
            </div>
          ))}
        </div>
      </div>
    ),
    { ...size, fonts: [{ name: "Lilex", data: lilex, style: "normal", weight: 400 }] },
  );
}
