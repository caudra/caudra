import { spawnSync, type SpawnSyncReturns } from 'node:child_process';
import { mkdir, readdir, realpath, writeFile } from 'node:fs/promises';
import { devNull } from 'node:os';
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const USAGE = 'usage: bun run recording:demo <dir>';
const REPO_ROOT = fileURLToPath(new URL('../../', import.meta.url));
const EXAMPLE_DIR = '~/caudra-recordings/orders';
const NOISY_COMMAND = 'make check';
const FILTER_STAGES = ['make', 'progress'];
const SEED_SCRIPT = 'scripts/seed.ts';
const BRANCH = 'main';
const AUTHOR_NAME = 'Demo';
const AUTHOR_EMAIL = 'demo@example.com';
const COMMIT_DATE = '2026-01-05T09:00:00Z';
const COMMIT_MESSAGE = 'Add cursor-paginated orders listing';
const LOCAL_GIT_CONFIG = [['user.name', AUTHOR_NAME], ['user.email', AUTHOR_EMAIL], ['commit.gpgsign', 'false']] as const;
const GIT_ENV = {
  ...process.env,
  GIT_CONFIG_GLOBAL: devNull,
  GIT_CONFIG_NOSYSTEM: '1',
  GIT_AUTHOR_DATE: COMMIT_DATE,
  GIT_COMMITTER_DATE: COMMIT_DATE,
};

const AGENTS = `# Agent notes

- Bun project without dependencies. \`make check\` reseeds the fixtures and runs the tests; \`bun test\` runs only the tests.
- Tests sit beside the code in \`src/\`. Fix the code, not the tests.
- Keep diffs small and name the root cause in one sentence.
`;

const README = `# Orders API

A small cursor-paginated orders listing.

\`listOrders(limit, cursor)\` in \`src/orders.ts\` returns one page of orders and a \`nextCursor\`. Clients follow \`nextCursor\` until it is \`null\`.

## Development

- \`make check\` reseeds \`fixtures/orders.json\` and runs the tests.
- \`bun test\` runs the tests alone.
`;

const MAKEFILE = `.PHONY: check seed test

check:
\t$(MAKE) seed
\t$(MAKE) test

seed:
\tbun ${SEED_SCRIPT}

test:
\tbun test
`;

const SEED = `import { mkdirSync, writeFileSync } from 'node:fs';
import type { Order } from '../src/orders';

const TOTAL = 240;
const STEP = 10;
const BAR_WIDTH = 20;
const CUSTOMERS = 12;
const FIXTURE = 'fixtures/orders.json';

const orders: Order[] = [];
for (let id = 1; id <= TOTAL; id += 1) {
  orders.push({ id, customer: 'customer-' + ((id % CUSTOMERS) + 1), totalCents: ((id * 1237) % 50000) + 500 });
  if (id % STEP === 0) console.log(progress(id));
}
mkdirSync(new URL('../fixtures', import.meta.url), { recursive: true });
writeFileSync(new URL('../' + FIXTURE, import.meta.url), '[\\n' + orders.map((order) => '  ' + JSON.stringify(order)).join(',\\n') + '\\n]\\n');
console.log('wrote ' + TOTAL + ' orders to ' + FIXTURE);

function progress(done: number): string {
  const bar = '='.repeat(Math.round((done / TOTAL) * BAR_WIDTH)).padEnd(BAR_WIDTH);
  const percent = String(Math.round((done / TOTAL) * 100)).padStart(3);
  return 'seed orders [' + bar + '] ' + String(done).padStart(3) + '/' + TOTAL + ' ' + percent + '%';
}
`;

const PAGINATION = `export interface Page<T> {
  items: T[];
  nextCursor: string | null;
}

export interface PageRequest {
  limit: number;
  cursor?: string | null;
}

const CURSOR_PREFIX = 'id:';

export function encodeCursor(id: number): string {
  return Buffer.from(CURSOR_PREFIX + id).toString('base64url');
}

export function decodeCursor(cursor: string): number {
  const decoded = Buffer.from(cursor, 'base64url').toString('utf8');
  const id = Number(decoded.slice(CURSOR_PREFIX.length));
  if (!decoded.startsWith(CURSOR_PREFIX) || !Number.isInteger(id)) throw new Error('invalid cursor: ' + cursor);
  return id;
}

export function paginate<T extends { id: number }>(rows: readonly T[], request: PageRequest): Page<T> {
  const after = request.cursor ? decodeCursor(request.cursor) : 0;
  const remaining = rows.filter((row) => row.id > after);
  const items = remaining.slice(0, request.limit);
  const last = items.at(-1);
  return { items, nextCursor: last ? encodeCursor(last.id) : null };
}
`;

const PAGINATION_TEST = `import { describe, expect, test } from 'bun:test';
import { decodeCursor, encodeCursor, paginate } from './pagination';

const rows = Array.from({ length: 10 }, (_, index) => ({ id: index + 1 }));
const ids = (page: { items: { id: number }[] }) => page.items.map((row) => row.id);

describe('paginate', () => {
  test('the first page returns the limit and a cursor', () => {
    const page = paginate(rows, { limit: 4 });
    expect(ids(page)).toEqual([1, 2, 3, 4]);
    expect(page.nextCursor).toBe(encodeCursor(4));
  });

  test('a cursor continues after the last item it saw', () => {
    expect(ids(paginate(rows, { limit: 4, cursor: encodeCursor(4) }))).toEqual([5, 6, 7, 8]);
  });

  test('the final page has no next cursor', () => {
    const page = paginate(rows, { limit: 4, cursor: encodeCursor(8) });
    expect(ids(page)).toEqual([9, 10]);
    expect(page.nextCursor).toBeNull();
  });

  test('a listing that fills its last page exactly ends there', () => {
    const page = paginate(rows, { limit: 5, cursor: encodeCursor(5) });
    expect(ids(page)).toEqual([6, 7, 8, 9, 10]);
    expect(page.nextCursor).toBeNull();
  });

  test('cursors round-trip', () => {
    expect(decodeCursor(encodeCursor(42))).toBe(42);
  });
});
`;

const ORDERS = `import orders from '../fixtures/orders.json';
import { paginate, type Page } from './pagination';

export interface Order {
  id: number;
  customer: string;
  totalCents: number;
}

export const MAX_LIMIT = 100;

export function listOrders(limit: number, cursor?: string | null): Page<Order> {
  return paginate(orders, { limit: Math.min(limit, MAX_LIMIT), cursor });
}
`;

const ORDERS_TEST = `import { expect, test } from 'bun:test';
import { MAX_LIMIT, listOrders } from './orders';

test('the limit is capped', () => {
  expect(listOrders(MAX_LIMIT * 5).items).toHaveLength(MAX_LIMIT);
});

test('following every cursor visits each order once without an empty page', () => {
  const pageSizes: number[] = [];
  let cursor: string | null = null;
  do {
    const page = listOrders(50, cursor);
    pageSizes.push(page.items.length);
    cursor = page.nextCursor;
  } while (cursor);
  expect(pageSizes).toEqual([50, 50, 50, 50, 40]);
});
`;

const PACKAGE = `${JSON.stringify({ name: 'orders-api', private: true, type: 'module', scripts: { seed: `bun ${SEED_SCRIPT}`, test: 'bun test' } }, null, 2)}\n`;

const FILES: Record<string, string> = {
  'AGENTS.md': AGENTS,
  'README.md': README,
  Makefile: MAKEFILE,
  'package.json': PACKAGE,
  '.gitignore': 'node_modules/\n',
  [SEED_SCRIPT]: SEED,
  'src/pagination.ts': PAGINATION,
  'src/pagination.test.ts': PAGINATION_TEST,
  'src/orders.ts': ORDERS,
  'src/orders.test.ts': ORDERS_TEST,
};

interface Location {
  existing: string;
  target: string;
}

function errorCode(error: unknown): string | undefined {
  return (error as NodeJS.ErrnoException).code;
}

function isInside(parent: string, child: string): boolean {
  const path = relative(parent, child);
  return path === '' || (path !== '..' && !path.startsWith(`..${sep}`) && !isAbsolute(path));
}

async function locate(path: string): Promise<Location> {
  try {
    const existing = await realpath(path);
    return { existing, target: existing };
  } catch (error) {
    const parent = dirname(path);
    if (parent === path || errorCode(error) !== 'ENOENT') throw error;
    const located = await locate(parent);
    return { existing: located.existing, target: join(located.target, basename(path)) };
  }
}

function spawn(command: string, args: string[], cwd: string, env: NodeJS.ProcessEnv = process.env): SpawnSyncReturns<string> {
  const result = spawnSync(command, args, { cwd, env, encoding: 'utf8' });
  if (result.error) throw new Error(`${command} did not start: ${result.error.message}`);
  return result;
}

function run(command: string, args: string[], cwd: string, env?: NodeJS.ProcessEnv): void {
  const result = spawn(command, args, cwd, env);
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed in ${cwd}:\n${result.stderr.trim()}`);
}

async function refuseRepository(target: string): Promise<void> {
  if (isInside(await realpath(REPO_ROOT), target)) {
    throw new Error(`${target} is inside this repository; create the demo outside it, for example ${EXAMPLE_DIR}`);
  }
}

async function ensureEmpty(target: string): Promise<void> {
  const entries = await readdir(target).catch((error: unknown) => {
    if (errorCode(error) === 'ENOENT') return [];
    if (errorCode(error) === 'ENOTDIR') throw new Error(`${target} is not a directory`);
    throw error;
  });
  if (entries.length > 0) throw new Error(`${target} is not empty; pass a new or empty directory`);
}

function refuseWorkTree({ existing, target }: Location): void {
  const workTree = spawn('git', ['rev-parse', '--show-toplevel'], existing);
  if (workTree.status === 0) {
    throw new Error(`${target} is inside the git work tree ${workTree.stdout.trim()}; the demo needs its own repository, for example ${EXAMPLE_DIR}`);
  }
}

async function writeProject(target: string): Promise<void> {
  for (const [path, content] of Object.entries(FILES)) {
    const file = join(target, path);
    await mkdir(dirname(file), { recursive: true });
    await writeFile(file, content);
  }
}

function commitProject(target: string): void {
  run('git', ['init', '--quiet', `--initial-branch=${BRANCH}`], target, GIT_ENV);
  for (const [key, value] of LOCAL_GIT_CONFIG) run('git', ['config', key, value], target, GIT_ENV);
  run('git', ['add', '--all'], target, GIT_ENV);
  run('git', ['commit', '--quiet', '--message', COMMIT_MESSAGE], target, GIT_ENV);
}

async function main(args: string[]): Promise<void> {
  if (args.length !== 1) throw new Error(USAGE);
  const location = await locate(resolve(args[0]));
  const { target } = location;
  await refuseRepository(target);
  await ensureEmpty(target);
  refuseWorkTree(location);
  await writeProject(target);
  run(process.execPath, [SEED_SCRIPT], target);
  commitProject(target);
  console.log(`Created ${target} on branch ${BRANCH} with one commit by ${AUTHOR_NAME} <${AUTHOR_EMAIL}>.
bun test fails because paginate() returns a nextCursor on the final page.
${NOISY_COMMAND} is the noisy command: Workcell's shell filter reduces it with the ${FILTER_STAGES.join(' and ')} stages.`);
  if (!Bun.which('make')) console.warn(`make is not on PATH; install it before recording ${NOISY_COMMAND}.`);
}

if (import.meta.main) {
  await main(process.argv.slice(2)).catch((error: unknown) => {
    console.error(`recording:demo: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
