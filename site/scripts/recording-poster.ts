import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';
import type { create as createPlayer } from 'asciinema-player';
import {
  RECORDING_ID, decodeCast, formatEntry, inspectCast, isBlocking, manifestEntry, parseCast, seconds, type PosterSize,
} from './recording-check';

interface Render {
  data: string;
  time: number;
  cols: number;
  rows: number;
}

const RECORDINGS = new URL('../public/recordings/', import.meta.url);
const PLAYER_SCRIPT = 'asciinema-player/dist/bundle/asciinema-player.min.js';
const PLAYER_STYLES = 'asciinema-player/dist/bundle/asciinema-player.css';
// Mirrors --mono and its @font-face rules in src/styles/brand.css, so the poster matches the first played frame.
const SITE_FONT = 'JetBrains Mono';
const SITE_MONO_STACK = `'${SITE_FONT}', ui-monospace, monospace`;
const SITE_FONT_FILES = [
  ['normal', new URL('../public/fonts/jetbrains-mono-latin.woff2', import.meta.url)],
  ['italic', new URL('../public/fonts/jetbrains-mono-latin-italic.woff2', import.meta.url)],
] as const;
const TERMINAL_FONT = 'var(--mono, monospace)';
const TERMINAL = '.ap-term';
const DEVICE_SCALE_FACTOR = 2;
const PNG_WIDTH_OFFSET = 16;
const PNG_HEIGHT_OFFSET = 20;
const USAGE = 'usage: bun run recording:poster <id> <seconds>';

async function pageHtml(): Promise<string> {
  const fonts = await Promise.all(SITE_FONT_FILES.map(async ([style, file]) => {
    const source = `data:font/woff2;base64,${(await readFile(file)).toString('base64')}`;
    return `@font-face { font-family: '${SITE_FONT}'; src: url(${source}) format('woff2'); font-weight: 100 800; font-style: ${style}; }`;
  }));
  const [script, styles] = await Promise.all([PLAYER_SCRIPT, PLAYER_STYLES].map((module) => readFile(fileURLToPath(import.meta.resolve(module)), 'utf8')));
  return `<!doctype html><html><head><meta charset="utf-8"><style>${fonts.join('\n')}
:root { --mono: ${SITE_MONO_STACK}; }
body { margin: 0; }
.ap-overlay { display: none !important; }
${styles}</style><script>${script}</script></head><body></body></html>`;
}

async function renderPoster(render: Render, path: string): Promise<PosterSize> {
  const browser = await chromium.launch();
  try {
    const context = await browser.newContext({ deviceScaleFactor: DEVICE_SCALE_FACTOR });
    await context.route('**/*', (route) => route.abort());
    const page = await context.newPage();
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.setContent(await pageHtml());
    await page.evaluate(async ({ render, font, terminalFont }) => {
      const loaded = await Promise.all([`16px "${font}"`, `italic 16px "${font}"`].map((face) => document.fonts.load(face)));
      if (loaded.some((faces) => faces.length === 0)) throw new Error(`${font} did not load`);
      const { create } = (window as unknown as { AsciinemaPlayer: { create: typeof createPlayer } }).AsciinemaPlayer;
      const player = create({ data: render.data, parser: 'asciicast' }, document.body, {
        cols: render.cols,
        rows: render.rows,
        preload: true,
        controls: false,
        fit: false,
        idleTimeLimit: Infinity,
        cursorMode: 'steady',
        terminalFontFamily: terminalFont,
      });
      await player.seek(render.time);
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    }, { render, font: SITE_FONT, terminalFont: TERMINAL_FONT });
    if (errors.length > 0) throw new Error(`the player failed: ${errors.join('; ')}`);
    const image = await page.locator(TERMINAL).screenshot({ animations: 'disabled' });
    await writeFile(path, image);
    return { width: image.readUInt32BE(PNG_WIDTH_OFFSET), height: image.readUInt32BE(PNG_HEIGHT_OFFSET) };
  } finally {
    await browser.close();
  }
}

async function main(args: string[]): Promise<void> {
  if (args.length !== 2) throw new Error(USAGE);
  const [id, at] = args;
  if (!RECORDING_ID.test(id)) throw new Error(`recording id "${id}" must match ${RECORDING_ID}`);
  const castFile = fileURLToPath(new URL(`${id}.cast`, RECORDINGS));
  const bytes = await readFile(castFile).catch((error: unknown) => {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
    throw new Error(`${castFile} does not exist; copy the checked take there first`);
  });
  const data = decodeCast(bytes);
  const cast = parseCast(data);
  const blocking = inspectCast(cast, bytes.byteLength, cast).filter(isBlocking).length;
  if (blocking > 0) throw new Error(`${castFile} has ${blocking} blocking findings; run bun run recording:check on it first`);
  const time = Number(at);
  if (!at.trim() || !Number.isFinite(time) || time < 0 || time > cast.duration) {
    throw new Error(`seconds must be a number from 0 to ${seconds(cast.duration)}, the length of ${castFile}`);
  }
  const path = fileURLToPath(new URL(`${id}.png`, RECORDINGS));
  const poster = await renderPoster({ data, time, cols: cast.cols, rows: cast.rows }, path);
  console.log(`Wrote ${path} from ${seconds(time)}.\nPoster size: width ${poster.width}, height ${poster.height}.`);
  console.log(`\nManifest entry for src/data/recordings.ts:\n\n${formatEntry(id, manifestEntry(id, cast, poster), false)}`);
}

if (import.meta.main) {
  await main(process.argv.slice(2)).catch((error: unknown) => {
    console.error(`recording:poster: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
