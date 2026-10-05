import { describe, expect, test } from 'bun:test';
import { defineRecordings } from '../../src/data/recordings';
import {
  DEFAULT_SIZE, DEMO_USER, HEADER_PREFIX, INVALID_EVENT, INVALID_HEADER, MAX_DURATION_SECONDS, MAX_FILE_BYTES, MISSING_SIZE, NOT_UTF8,
  NO_DURATION, PENDING_POSTER, TIME_REVERSAL, TODO_ALT, TODO_SUMMARY, TODO_TITLE, UNSUPPORTED_VERSION,
  decodeCast, formatEntry, inspectCast, isBlocking, manifestEntry, parseCast, roundedDuration, type Finding, type TerminalSize,
} from '../../scripts/recording-check';

type Event = [time: number, code: string, data: string];

const HEADER_V2 = { version: 2, width: DEFAULT_SIZE.cols, height: DEFAULT_SIZE.rows };
const HEADER_V3 = { version: 3, term: { cols: DEFAULT_SIZE.cols, rows: DEFAULT_SIZE.rows } };
const SIDE_CLIP: TerminalSize = { cols: 100, rows: 30 };
const PUBLIC_LINK = '\x1b]8;;https://caudra.ai/docs/queue/\x1b\\queue\x1b]8;;\x1b\\';
const MANIFEST_REJECTION = 'local cast and poster paths';

function cast(header: object, events: readonly Event[], ...extra: string[]): string {
  return [JSON.stringify(header), ...extra, ...events.map((event) => JSON.stringify(event))].join('\n');
}

function inspect(text: string, size: TerminalSize = DEFAULT_SIZE, bytes = Buffer.byteLength(text)): Finding[] {
  return inspectCast(parseCast(text), bytes, size);
}

function rules(text: string): string[] {
  return inspect(text).map((finding) => finding.rule);
}

function output(...chunks: string[]): string {
  return cast(HEADER_V3, chunks.map((chunk): Event => [0.5, 'o', chunk]));
}

function secret(prefix: string, length: number): string {
  return `${prefix}${'x'.repeat(length)}`;
}

describe('asciicast parsing', () => {
  test('v2 events keep absolute times', () => {
    const parsed = parseCast(cast(HEADER_V2, [[0.5, 'o', 'a'], [1.25, 'o', 'b'], [2, 'm', 'chapter']]));
    expect(parsed).toMatchObject({ version: 2, cols: DEFAULT_SIZE.cols, rows: DEFAULT_SIZE.rows, duration: 2 });
    expect(parsed.events.map((event) => event.time)).toEqual([0.5, 1.25, 2]);
  });

  test('v3 intervals accumulate and comments are skipped', () => {
    const parsed = parseCast(cast(HEADER_V3, [[0.5, 'o', 'a'], [0.75, 'o', 'b'], [1, 'x', '0']], '# comment', ''));
    expect(parsed).toMatchObject({ version: 3, cols: DEFAULT_SIZE.cols, rows: DEFAULT_SIZE.rows, duration: 2.25 });
    expect(parsed.events.map((event) => event.time)).toEqual([0.5, 1.25, 2.25]);
  });

  test.each([
    ['an asciicast v1 header', '{"version":1,"width":80,"height":24,"stdout":[]}', UNSUPPORTED_VERSION],
    ['a header that is not an object', '[2,120,34]', INVALID_HEADER],
    ['a v3 header without term', '{"version":3,"width":120,"height":34}', MISSING_SIZE],
    ['a v2 header without height', '{"version":2,"width":120}', MISSING_SIZE],
    ['v2 times that go backwards', cast(HEADER_V2, [[2, 'o', 'a'], [1, 'o', 'b']]), TIME_REVERSAL],
    ['a negative v3 interval', cast(HEADER_V3, [[-1, 'o', 'a']]), INVALID_EVENT],
    ['an event without data', `${JSON.stringify(HEADER_V3)}\n[0.5,"o"]`, INVALID_EVENT],
  ])('rejects %s', (_name, text, message) => {
    expect(() => parseCast(text)).toThrow(message);
  });

  test('rejects bytes that are not UTF-8', () => {
    expect(() => decodeCast(new Uint8Array([0x7b, 0xff, 0x7d]))).toThrow(NOT_UTF8);
  });
});

describe('recording limits', () => {
  test.each([
    ['v2', cast(HEADER_V2, [[0.5, 'o', 'ready\r\n'], [3, 'o', 'done\r\n']])],
    ['v3', cast(HEADER_V3, [[0.5, 'o', 'ready\r\n'], [2.5, 'o', 'done\r\n']])],
  ])('a clean %s take has no findings', (_version, text) => {
    expect(inspect(text)).toEqual([]);
  });

  test('a terminal size other than the expected one is blocking', () => {
    const side = cast({ version: 3, term: SIDE_CLIP }, [[1, 'o', 'x']]);
    expect(rules(side)).toEqual(['size']);
    expect(inspect(side, SIDE_CLIP)).toEqual([]);
  });

  test.each([
    ['v2', cast(HEADER_V2, [[1, 'o', 'x'], [2, 'r', '100x30'], [3, 'r', '90x30']])],
    ['v3', cast(HEADER_V3, [[1, 'o', 'x'], [1, 'r', '100x30'], [1, 'r', '90x30']])],
  ])('%s resize events are reported once at the first resize', (_version, text) => {
    expect(inspect(text)).toMatchObject([{ rule: 'resize', event: 1, time: 2 }]);
  });

  test('duration beyond the limit is blocking', () => {
    expect(rules(cast(HEADER_V2, [[MAX_DURATION_SECONDS + 1, 'o', 'x']]))).toEqual(['duration']);
    expect(rules(cast(HEADER_V2, [[MAX_DURATION_SECONDS, 'o', 'x']]))).toEqual([]);
  });

  test('a recording without events has no duration', () => {
    expect(inspect(cast(HEADER_V3, []))).toMatchObject([{ rule: 'duration', message: NO_DURATION }]);
  });

  test('file size beyond the limit is blocking', () => {
    const text = cast(HEADER_V3, [[1, 'o', 'x']]);
    expect(inspect(text, DEFAULT_SIZE, MAX_FILE_BYTES + 1).map((finding) => finding.rule)).toEqual(['file-size']);
    expect(inspect(text, DEFAULT_SIZE, MAX_FILE_BYTES)).toEqual([]);
  });

  test('keyboard input is reported once with its first event', () => {
    const findings = inspect(cast(HEADER_V3, [[0.5, 'o', '$ '], [0.25, 'i', 'l'], [0.25, 'i', 's']]));
    expect(findings).toMatchObject([{ rule: 'input', event: 1, time: 0.75 }]);
    expect(findings.every(isBlocking)).toBe(true);
  });
});

describe('privacy scan', () => {
  test.each([
    secret('sk-ant-api03-', 40), secret('sk-proj-', 40), secret('ghp_', 36), secret('github_pat_', 40),
    secret('glpat-', 20), secret('xoxb-', 24), secret('AKIA', 16).toUpperCase(), secret('AIza', 35), secret('npm_', 36),
    '-----BEGIN OPENSSH PRIVATE KEY-----', `Authorization: ${secret('Bearer ', 32)}`, 'password=hunter22',
    `export GITHUB_TOKEN=${'1'.repeat(12)}`, `https://api.test/v1?access_token=${'2'.repeat(12)}`,
  ])('finds a likely secret in %#', (text) => {
    const findings = inspect(output(`${text}\r\n`));
    expect(findings.map((finding) => finding.rule)).toContain('secret');
    expect(findings.every((finding) => !finding.message.includes(text.slice(-8)))).toBe(true);
  });

  test.each(['const cursor = encodeCursor(id);', 'max_tokens=4096', 'api_key=$ANTHROPIC_API_KEY', 'task-implement-active-footer-chips'])(
    'leaves ordinary text alone: %s',
    (text) => {
      expect(rules(output(text))).toEqual([]);
    },
  );

  test('finds a secret split by colour codes across events', () => {
    const findings = inspect(output('key \x1b[33msk-ant-', `\x1b[1m${'k'.repeat(30)}\x1b[0m\r\n`));
    expect(findings).toMatchObject([{ rule: 'secret', event: 0, time: 0.5 }]);
  });

  test.each(['/home/alex/pagination', '/Users/alex/pagination', 'C:\\Users\\alex\\pagination', '/mnt/c/Users/alex'])(
    'finds the home path %s',
    (path) => {
      expect(rules(output(`cwd ${path}\r\n`))).toEqual(['home-path']);
    },
  );

  test.each([`/home/${DEMO_USER}/pagination`, `/Users/${DEMO_USER}`, `C:\\Users\\${DEMO_USER}\\pagination`])(
    'allows the neutral demo user in %s',
    (path) => {
      expect(rules(output(`cwd ${path}\r\n`))).toEqual([]);
    },
  );

  test('finds personal email addresses but allows neutral ones', () => {
    expect(rules(output('Author: Alex <alex@company.io>\r\n'))).toEqual(['email']);
    expect(rules(output('Demo <demo@example.com> git@github.com:caudra/caudra.git ci@build.test\r\n'))).toEqual([]);
  });

  test('scans header strings without an event location', () => {
    const text = cast({ ...HEADER_V3, env: { SHELL: '/home/alex/.nix-profile/bin/zsh' } }, [[1, 'o', 'x']]);
    const findings = inspect(text);
    expect(findings.map(({ rule, event }) => ({ rule, event }))).toEqual([{ rule: 'home-path', event: undefined }]);
    expect(findings[0].message.startsWith(HEADER_PREFIX)).toBe(true);
  });

  test('groups repeated occurrences under the first event', () => {
    const findings = inspect(output('a', '/home/alex/x\r\n', '/home/alex/y\r\n', '/home/alex/z\r\n'));
    expect(findings).toMatchObject([{ rule: 'home-path', event: 1, time: 1 }]);
  });

  test('OSC 8 hyperlinks are warnings that name the target', () => {
    const findings = inspect(output(`See ${PUBLIC_LINK}\r\n`));
    expect(findings).toMatchObject([{ rule: 'hyperlink', message: expect.stringContaining('https://caudra.ai/docs/queue/') }]);
    expect(findings.some(isBlocking)).toBe(false);
  });

  test('a hidden hyperlink target is still scanned for private paths', () => {
    expect(rules(output('\x1b]8;;file:///home/alex/notes.md\x1b\\notes\x1b]8;;\x1b\\')).sort()).toEqual(['home-path', 'hyperlink']);
  });

  test('OSC 52 clipboard writes are blocking but clipboard queries are not reported', () => {
    const findings = inspect(output('\x1b]52;c;aGVsbG8gd29ybGQ=\x07'));
    expect(findings).toMatchObject([{ rule: 'clipboard', event: 0 }]);
    expect(findings.every(isBlocking)).toBe(true);
    expect(rules(output('\x1b]52;c;?\x07'))).toEqual([]);
  });
});

describe('manifest entry', () => {
  const take = parseCast(cast(HEADER_V3, [[0.5, 'o', 'x'], [46.7, 'o', 'y']]));

  test('rounds the duration up and fills placeholders the manifest accepts', () => {
    expect(manifestEntry('filter', take, PENDING_POSTER)).toEqual({
      title: TODO_TITLE,
      summary: TODO_SUMMARY,
      src: '/recordings/filter.cast',
      poster: { src: '/recordings/filter.png', alt: TODO_ALT, ...PENDING_POSTER },
      cols: DEFAULT_SIZE.cols,
      rows: DEFAULT_SIZE.rows,
      duration: 48,
      chapters: [],
      maturity: 'stable',
    });
  });

  test.each([[47, 47], [47.0000001, 47], [47.2, 48], [0.4, 1]])('rounds %d seconds up to %d', (duration, rounded) => {
    expect(roundedDuration(duration)).toBe(rounded);
  });

  test('rejects an id the manifest cannot serve', () => {
    expect(() => manifestEntry('Bad_Id', take, PENDING_POSTER)).toThrow(MANIFEST_REJECTION);
  });

  test.each([['filter', true], ['side-question', false]])('formats %s as a pasteable manifest entry', (id, posterPending) => {
    const entry = manifestEntry(id, take, { width: 2184, height: 1408 });
    const pasted: unknown = Function(`return {\n${formatEntry(id, entry, posterPending)}\n};`)();
    expect(pasted).toEqual({ [id]: entry });
    expect(() => defineRecordings(pasted as Parameters<typeof defineRecordings>[0])).not.toThrow();
  });
});
