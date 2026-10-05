import { expect, test } from 'bun:test';
import * as home from '../../src/data/home';

const BANNED_PUNCTUATION = /[—;]/;
const CONTRACTION = /\b(?:\w+n't|it's|that's|there's|here's|what's|let's|(?:I|you|we|they)(?:'m|'re|'ve|'ll|'d))\b/i;
const STOCK_VOCABULARY = /\b(?:delve|robust|seamless(?:ly)?|leverage|landscape|moreover|furthermore)\b/i;
const RECORDING_ID = /^[a-z][a-z0-9-]*$/;
const LABEL_ECHO = /^Illustration\b/;
const CAPABILITY_HREF = /^(?:\/docs\/[a-z0-9-]+\/(?:#[a-z0-9_-]+)?|https:\/\/\S+)$/;

function strings(value: unknown): string[] {
  if (typeof value === 'string') return [value];
  if (Array.isArray(value)) return value.flatMap(strings);
  if (value && typeof value === 'object') return Object.values(value).flatMap(strings);
  return [];
}

test('homepage copy follows the site voice', () => {
  const copy = strings(home);
  expect(copy.length).toBeGreaterThan(0);
  for (const text of copy) {
    expect(text, text).not.toMatch(BANNED_PUNCTUATION);
    expect(text, text).not.toMatch(CONTRACTION);
    expect(text, text).not.toMatch(STOCK_VOCABULARY);
  }
});

test('every recording slot has a unique manifest id and a summary that does not repeat its label', () => {
  const clips = [home.heroClip, ...home.stories.flatMap((story) => story.clips)];
  const ids = clips.map((clip) => clip.id);
  expect(new Set(ids).size).toBe(ids.length);
  for (const clip of clips) {
    expect(clip.id).toMatch(RECORDING_ID);
    expect(clip.summary, clip.id).not.toMatch(LABEL_ECHO);
  }
});

test('stories carry claims, a destination, and a stage', () => {
  const ids = home.stories.map((story) => story.id);
  expect(new Set(ids).size).toBe(ids.length);
  for (const story of home.stories) {
    expect(story.points.length).toBeGreaterThanOrEqual(2);
    expect(story.points.length).toBeLessThanOrEqual(4);
    expect(story.clips.length).toBeGreaterThan(0);
    expect(story.link.href).toMatch(CAPABILITY_HREF);
  }
});

test('capabilities link to documentation and count themselves', () => {
  const items = home.capabilityGroups.flatMap((group) => group.items);
  expect(home.capabilityCount).toBe(items.length);
  for (const item of items) expect(item.href, item.name).toMatch(CAPABILITY_HREF);
  for (const experiment of home.experiments) {
    expect(items.some((item) => item.experimental && item.href === experiment.href), experiment.name).toBe(true);
  }
});
