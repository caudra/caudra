import { readFile } from 'node:fs/promises';
import { basename, extname } from 'node:path';
import { parseArgs } from 'node:util';
import { defineRecordings, type Recording } from '../src/data/recordings';

export interface TerminalSize {
  cols: number;
  rows: number;
}

export interface PosterSize {
  width: number;
  height: number;
}

export interface CastEvent {
  time: number;
  code: string;
  data: string;
}

export interface Cast extends TerminalSize {
  version: 2 | 3;
  header: Record<string, unknown>;
  events: CastEvent[];
  duration: number;
}

export type Rule = 'size' | 'resize' | 'duration' | 'file-size' | 'input' | 'secret' | 'home-path' | 'email' | 'hyperlink' | 'clipboard';

export interface Finding {
  rule: Rule;
  message: string;
  event?: number;
  time?: number;
}

interface Detector {
  rule: Rule;
  pattern: RegExp;
  describe(match: RegExpExecArray): string | undefined;
}

interface Detection {
  key: string;
  rule: Rule;
  message: string;
  offset: number;
}

export const DEFAULT_SIZE: TerminalSize = { cols: 120, rows: 34 };
export const MAX_DURATION_SECONDS = 120;
export const MAX_FILE_BYTES = 3 * 1024 * 1024;
export const DEMO_USER = 'demo';
export const RECORDING_ID = /^[a-z][a-z0-9-]*$/;
export const PENDING_POSTER: PosterSize = { width: 1, height: 1 };
export const TODO_TITLE = 'TODO title';
export const TODO_SUMMARY = 'TODO summary';
export const TODO_ALT = 'TODO alt text';
export const NOT_UTF8 = 'file is not valid UTF-8';
export const INVALID_HEADER = 'line 1: the header must be a JSON object';
export const UNSUPPORTED_VERSION = 'unsupported asciicast version';
export const MISSING_SIZE = 'line 1: the header has no terminal size';
export const INVALID_EVENT = 'events must be [time, code, data] with a non-negative time';
export const TIME_REVERSAL = 'event time goes backwards';
export const NO_DURATION = 'Recording has no duration';
export const HEADER_PREFIX = 'Header: ';
export const POSTER_TODO = '// TODO: width and height from bun run recording:poster';

const RECORDINGS_URL = '/recordings';
const FILE_LEVEL = -1;
const MICROSECONDS = 1_000_000;
const MILLISECONDS = 1000;
const MEBIBYTE = 1024 * 1024;
const REDACTED_PREFIX = 4;
const WARNING_RULES: ReadonlySet<Rule> = new Set<Rule>(['hyperlink']);
const NEUTRAL_EMAIL_LOCAL_PARTS: ReadonlySet<string> = new Set(['git']);
const NEUTRAL_EMAIL_DOMAIN = /(?:^|\.)(?:example\.(?:com|net|org)|example|invalid|localhost|test)$/i;
const PLAIN_KEY = /^[a-z][a-z0-9]*$/;
const ESCAPE_SEQUENCE = /\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[PX^_][^\x1b]*\x1b\\|[ -/]*[0-~])/g;
const HYPERLINK = /\x1b\]8;[^;\x07\x1b]*;([^\x07\x1b]*)(?:\x07|\x1b\\)/g;
const CLIPBOARD_WRITE = /\x1b\]52;[^;\x07\x1b]*;([^?\x07\x1b][^\x07\x1b]*)(?:\x07|\x1b\\)/g;
const HOME_PATH = /(?:\/home\/|\/Users\/|\b[A-Za-z]:[\\/][Uu]sers[\\/])([A-Za-z0-9._-]+)/g;
const EMAIL = /([A-Za-z0-9._%+-]+)@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,})/g;
const SECRETS: readonly (readonly [label: string, pattern: RegExp])[] = [
  ['Anthropic API key', /\bsk-ant-[A-Za-z0-9_-]{20,}/g],
  ['API key', /\bsk-(?!ant-)[A-Za-z0-9_-]{20,}/g],
  ['GitHub token', /\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})/g],
  ['GitLab token', /\bglpat-[A-Za-z0-9_-]{20,}/g],
  ['Slack token', /\bxox[abeoprs]-[A-Za-z0-9-]{10,}/g],
  ['AWS access key', /\b(?:AKIA|ASIA)[A-Z0-9]{16}\b/g],
  ['Google API key', /\bAIza[A-Za-z0-9_-]{35}/g],
  ['xAI API key', /\bxai-[A-Za-z0-9]{40,}/g],
  ['npm token', /\bnpm_[A-Za-z0-9]{36}/g],
  ['PEM block', /-----BEGIN [A-Z0-9 ]+-----/g],
  ['bearer token', /\bBearer\s+[A-Za-z0-9._~+/-]{16,}=*/g],
  ['credential assignment', /(?<![A-Za-z0-9])(?:[A-Za-z0-9]+[_-])*(?:password|passwd|secret|token|api[_-]?key)=["']?(?!\$)[^\s"'&]{4,}/gi],
];
const TEXT_DETECTORS: readonly Detector[] = [
  ...SECRETS.map(([label, pattern]): Detector => ({
    rule: 'secret',
    pattern,
    describe: ([secret]) => `Possible ${label}: ${secret.slice(0, REDACTED_PREFIX)}… (${secret.length} characters)`,
  })),
  { rule: 'home-path', pattern: HOME_PATH, describe: ([path, user]) => user === DEMO_USER ? undefined : `Home directory path ${path}` },
  { rule: 'email', pattern: EMAIL, describe: ([address, local, domain]) => neutralEmail(local, domain) ? undefined : `Email address ${address}` },
];
const SEQUENCE_DETECTORS: readonly Detector[] = [
  { rule: 'hyperlink', pattern: HYPERLINK, describe: ([, target]) => target ? `OSC 8 hyperlink to ${target}; playback hides the target, so confirm it is public` : undefined },
  { rule: 'clipboard', pattern: CLIPBOARD_WRITE, describe: ([, payload]) => `OSC 52 clipboard write of ${payload.length} base64 characters` },
];
const USAGE = 'usage: bun run recording:check <file.cast> [--id <id>] [--cols <n> --rows <n>]';

export function parseCast(text: string): Cast {
  const [first = '', ...lines] = text.split('\n');
  const header = parseLine(first, 1);
  if (!isRecord(header)) throw new Error(INVALID_HEADER);
  const { version } = header;
  if (version !== 2 && version !== 3) {
    throw new Error(`${UNSUPPORTED_VERSION} ${JSON.stringify(version)}; record with asciinema 2 (v2) or 3 (v3)`);
  }
  const term = isRecord(header.term) ? header.term : {};
  const [cols, rows] = version === 2 ? [header.width, header.height] : [term.cols, term.rows];
  if (!isPositiveInteger(cols) || !isPositiveInteger(rows)) throw new Error(MISSING_SIZE);
  const events: CastEvent[] = [];
  let clock = 0;
  lines.forEach((line, index) => {
    if (!line.trim() || line.startsWith('#')) return;
    const number = index + 2;
    const event = parseLine(line, number);
    if (!Array.isArray(event) || event.length !== 3 || typeof event[0] !== 'number' || !Number.isFinite(event[0])
      || event[0] < 0 || typeof event[1] !== 'string' || typeof event[2] !== 'string') {
      throw new Error(`line ${number}: ${INVALID_EVENT}`);
    }
    const time = Math.round((version === 2 ? event[0] : clock + event[0]) * MICROSECONDS) / MICROSECONDS;
    if (time < clock) throw new Error(`line ${number}: ${TIME_REVERSAL}`);
    clock = time;
    events.push({ time, code: event[1], data: event[2] });
  });
  return { version, cols, rows, header, events, duration: clock };
}

export function inspectCast(cast: Cast, bytes: number, expected: TerminalSize = DEFAULT_SIZE): Finding[] {
  const findings = [
    ...sizeFindings(cast, expected),
    ...limitFindings(cast, bytes),
    ...eventFinding(cast, 'r', 'resize', (size) => `Terminal resized to ${size}`),
    ...eventFinding(cast, 'i', 'input', () => 'Keyboard input recorded'),
    ...contentFindings(cast),
  ];
  return findings.sort((a, b) => (a.event ?? FILE_LEVEL) - (b.event ?? FILE_LEVEL));
}

export function isBlocking(finding: Finding): boolean {
  return !WARNING_RULES.has(finding.rule);
}

export function roundedDuration(seconds: number): number {
  return Math.ceil(Math.round(seconds * MILLISECONDS) / MILLISECONDS);
}

export function manifestEntry(id: string, cast: Cast, poster: PosterSize): Recording {
  const recording: Recording = {
    title: TODO_TITLE,
    summary: TODO_SUMMARY,
    src: `${RECORDINGS_URL}/${id}.cast`,
    poster: { src: `${RECORDINGS_URL}/${id}.png`, alt: TODO_ALT, ...poster },
    cols: cast.cols,
    rows: cast.rows,
    duration: roundedDuration(cast.duration),
    chapters: [],
    maturity: 'stable',
  };
  defineRecordings({ [id]: recording });
  return recording;
}

export function formatEntry(id: string, recording: Recording, posterPending: boolean): string {
  const { poster } = recording;
  const chapters = recording.chapters.map((chapter) => `{ title: ${quote(chapter.title)}, time: ${chapter.time} }`);
  return [
    `  ${PLAIN_KEY.test(id) ? id : quote(id)}: {`,
    `    title: ${quote(recording.title)},`,
    `    summary: ${quote(recording.summary)},`,
    `    src: ${quote(recording.src)},`,
    `    poster: { src: ${quote(poster.src)}, alt: ${quote(poster.alt)}, width: ${poster.width}, height: ${poster.height} },${posterPending ? ` ${POSTER_TODO}` : ''}`,
    `    cols: ${recording.cols},`,
    `    rows: ${recording.rows},`,
    `    duration: ${recording.duration},`,
    `    chapters: [${chapters.join(', ')}],`,
    `    maturity: ${quote(recording.maturity)},`,
    '  },',
  ].join('\n');
}

export function decodeCast(bytes: Uint8Array): string {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    throw new Error(NOT_UTF8);
  }
}

export function seconds(value: number): string {
  return `${value.toFixed(2)} s`;
}

function sizeFindings(cast: Cast, expected: TerminalSize): Finding[] {
  return cast.cols === expected.cols && cast.rows === expected.rows ? [] : [{
    rule: 'size',
    message: `Terminal is ${cast.cols}x${cast.rows}, expected ${expected.cols}x${expected.rows}`,
  }];
}

function limitFindings(cast: Cast, bytes: number): Finding[] {
  const findings: Finding[] = [];
  if (cast.duration <= 0) {
    findings.push({ rule: 'duration', message: NO_DURATION });
  } else if (cast.duration > MAX_DURATION_SECONDS) {
    findings.push({ rule: 'duration', message: `Recording lasts ${seconds(cast.duration)}, the limit is ${MAX_DURATION_SECONDS} s` });
  }
  if (bytes > MAX_FILE_BYTES) {
    findings.push({ rule: 'file-size', message: `File is ${mebibytes(bytes)} MiB, the limit is ${mebibytes(MAX_FILE_BYTES)} MiB` });
  }
  return findings;
}

function eventFinding(cast: Cast, code: string, rule: Rule, describe: (data: string) => string): Finding[] {
  const indices = cast.events.flatMap((event, index) => event.code === code ? [index] : []);
  if (indices.length === 0) return [];
  const first = indices[0];
  const event = cast.events[first];
  const count = indices.length > 1 ? ` (${indices.length} events)` : '';
  return [{ rule, message: `${describe(event.data)}${count}`, event: first, time: event.time }];
}

function contentFindings(cast: Cast): Finding[] {
  const owners: number[] = [];
  const starts: number[] = [];
  let raw = '';
  cast.events.forEach((event, index) => {
    if (event.code !== 'o') return;
    owners.push(index);
    starts.push(raw.length);
    raw += event.data;
  });
  const visible = stripEscapes(raw);
  const header = group(detect(TEXT_DETECTORS, headerStrings(cast.header).join('\n'), (offset) => offset))
    .map(({ rule, message }): Finding => ({ rule, message: `${HEADER_PREFIX}${message}` }));
  const output = group([
    ...detect([...TEXT_DETECTORS, ...SEQUENCE_DETECTORS], raw, (offset) => offset),
    ...detect(TEXT_DETECTORS, visible.text, visible.toRaw),
  ]).map(({ rule, message, offset }): Finding => {
    const event = owners[lastAtOrBefore(starts, offset)];
    return { rule, message, event, time: cast.events[event].time };
  });
  return [...header, ...output];
}

function group(detections: readonly Detection[]): Omit<Detection, 'key'>[] {
  const groups = new Map<string, { first: Detection; offsets: Set<number> }>();
  for (const detection of detections) {
    const existing = groups.get(detection.key);
    if (!existing) {
      groups.set(detection.key, { first: detection, offsets: new Set([detection.offset]) });
      continue;
    }
    existing.offsets.add(detection.offset);
    if (detection.offset < existing.first.offset) existing.first = detection;
  }
  return [...groups.values()].map(({ first: { rule, message, offset }, offsets }) => ({
    rule,
    message: offsets.size > 1 ? `${message} (${offsets.size} occurrences)` : message,
    offset,
  }));
}

function detect(detectors: readonly Detector[], text: string, toRaw: (offset: number) => number): Detection[] {
  return detectors.flatMap(({ rule, pattern, describe }) => [...text.matchAll(pattern)].flatMap((match) => {
    const message = describe(match);
    return message === undefined ? [] : [{ key: `${rule}\0${match[0]}`, rule, message, offset: toRaw(match.index) }];
  }));
}

function stripEscapes(raw: string): { text: string; toRaw(offset: number): number } {
  const visibleStarts = [0];
  const rawStarts = [0];
  let text = '';
  let consumed = 0;
  for (const match of raw.matchAll(ESCAPE_SEQUENCE)) {
    text += raw.slice(consumed, match.index);
    consumed = match.index + match[0].length;
    visibleStarts.push(text.length);
    rawStarts.push(consumed);
  }
  text += raw.slice(consumed);
  return {
    text,
    toRaw: (offset) => {
      const segment = lastAtOrBefore(visibleStarts, offset);
      return rawStarts[segment] + offset - visibleStarts[segment];
    },
  };
}

function lastAtOrBefore(sorted: readonly number[], value: number): number {
  let low = 0;
  let high = sorted.length - 1;
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (sorted[middle] <= value) low = middle;
    else high = middle - 1;
  }
  return low;
}

function headerStrings(value: unknown): string[] {
  if (typeof value === 'string') return [value];
  if (Array.isArray(value)) return value.flatMap(headerStrings);
  return isRecord(value) ? Object.values(value).flatMap(headerStrings) : [];
}

function neutralEmail(local: string, domain: string): boolean {
  return NEUTRAL_EMAIL_LOCAL_PARTS.has(local) || NEUTRAL_EMAIL_DOMAIN.test(domain);
}

function parseLine(line: string, number: number): unknown {
  try {
    return JSON.parse(line);
  } catch (error) {
    throw new Error(`line ${number}: ${(error as Error).message}`);
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isPositiveInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) > 0;
}

function quote(text: string): string {
  return `'${text.replace(/[\\']/g, '\\$&')}'`;
}

function mebibytes(bytes: number): string {
  return (bytes / MEBIBYTE).toFixed(2);
}

function sizeOption(value: string | undefined, fallback: number, flag: string): number {
  if (value === undefined) return fallback;
  const size = Number(value);
  if (!isPositiveInteger(size)) throw new Error(`${flag} must be a positive integer`);
  return size;
}

function formatFinding(finding: Finding): string {
  const severity = isBlocking(finding) ? 'error' : 'warning';
  const where = finding.event === undefined ? '' : `event ${finding.event} at ${seconds(finding.time ?? 0)}: `;
  return `${severity.padEnd(8)} ${finding.rule.padEnd(10)} ${where}${finding.message}`;
}

async function main(args: string[]): Promise<void> {
  const { values, positionals } = parseArgs({
    args,
    allowPositionals: true,
    options: { id: { type: 'string' }, cols: { type: 'string' }, rows: { type: 'string' } },
  });
  if (positionals.length !== 1) throw new Error(USAGE);
  const file = positionals[0];
  const id = values.id ?? basename(file, extname(file));
  if (!RECORDING_ID.test(id)) throw new Error(`recording id "${id}" must match ${RECORDING_ID}; pass --id`);
  const expected = {
    cols: sizeOption(values.cols, DEFAULT_SIZE.cols, '--cols'),
    rows: sizeOption(values.rows, DEFAULT_SIZE.rows, '--rows'),
  };
  const bytes = await readFile(file);
  const cast = parseCast(decodeCast(bytes));
  const findings = inspectCast(cast, bytes.byteLength, expected);
  console.log(`${file}: asciicast v${cast.version}, ${cast.cols}x${cast.rows}, ${seconds(cast.duration)}, ${mebibytes(bytes.byteLength)} MiB, ${cast.events.length} events`);
  for (const finding of findings) console.log(formatFinding(finding));
  const blocking = findings.filter(isBlocking).length;
  if (blocking > 0) {
    console.error(`\n${blocking} blocking finding${blocking === 1 ? '' : 's'}. Fix or re-record the take, then check it again.`);
    process.exitCode = 1;
    return;
  }
  console.log(`\nNo blocking findings. Manifest entry for src/data/recordings.ts:\n\n${formatEntry(id, manifestEntry(id, cast, PENDING_POSTER), true)}`);
  console.log(`\nNext:\n  mkdir -p public/recordings\n  cp ${file} public/recordings/${id}.cast\n  bun run recording:poster ${id} <seconds>`);
}

if (import.meta.main) {
  await main(process.argv.slice(2)).catch((error: unknown) => {
    console.error(`recording:check: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
