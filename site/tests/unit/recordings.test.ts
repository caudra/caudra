import { describe, expect, test } from 'bun:test';
import { defineRecordings, recordings, type Recording } from '../../src/data/recordings';

const fixture: Recording = {
  title: 'Test recording',
  summary: 'Synthetic test data, never published.',
  src: '/recordings/test.cast',
  poster: { src: '/recordings/test.png', alt: 'Test frame', width: 800, height: 400 },
  cols: 80,
  rows: 24,
  duration: 30,
  chapters: [{ title: 'Start', time: 0 }, { title: 'Finish', time: 20 }],
  maturity: 'stable',
};

describe('recording manifest', () => {
  test('ships no footage until reviewed recordings are supplied', () => {
    expect(Object.keys(recordings)).toEqual([]);
  });

  test('accepts complete metadata and ordered chapters', () => {
    expect(defineRecordings({ test: fixture }).test).toEqual(fixture);
  });

  test.each([
    'https://example.com/test.cast', '//example.com/test.cast', 'data:text/plain,test',
    '/recordings/../test.cast', '/recordings/test.cast?url=other.cast',
    '/recordings/test.cast#other.cast', '/recordings/test.%2e%2e/test.cast',
    '/recordings/test.cast/../../other.cast', '/recordings/test\\other.cast',
  ])('rejects unsafe source %s', (src) => {
    expect(() => defineRecordings({ test: { ...fixture, src } })).toThrow('local cast and poster paths');
  });

  test.each(['//example.com/image.png', '/recordings/../image.png', '/recordings/image.svg', '/recordings/image.png?remote'])('rejects unsafe poster %s', (src) => {
    expect(() => defineRecordings({ test: { ...fixture, poster: { ...fixture.poster, src } } })).toThrow('local cast and poster paths');
  });

  test.each([
    { title: '' }, { summary: ' ' }, { cols: 0 }, { rows: 1.5 },
    { duration: Infinity }, { duration: 0 }, { poster: { ...fixture.poster, height: -1 } },
  ])('rejects incomplete metadata %j', (metadata) => {
    expect(() => defineRecordings({ test: { ...fixture, ...metadata } })).toThrow('metadata is incomplete');
  });

  test.each([
    [{ title: 'Before', time: -1 }], [{ title: 'Beyond', time: 30 }],
    [{ title: '', time: 0 }], [{ title: 'Invalid', time: NaN }],
    [{ title: 'Second', time: 20 }, { title: 'First', time: 0 }],
    [{ title: 'First', time: 0 }, { title: 'Duplicate', time: 0 }],
  ].map((chapters) => ({ chapters })))('rejects invalid chapters %j', ({ chapters }) => {
    expect(() => defineRecordings({ test: { ...fixture, chapters } })).toThrow('chapters must be ordered');
  });
});
