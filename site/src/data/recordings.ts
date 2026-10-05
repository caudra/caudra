export interface RecordingChapter {
  title: string;
  time: number;
}

export interface Recording {
  title: string;
  summary: string;
  src: string;
  poster: { src: string; alt: string; width: number; height: number };
  cols: number;
  rows: number;
  duration: number;
  chapters: readonly RecordingChapter[];
  maturity: 'stable' | 'experimental';
  edits?: string;
}

export function defineRecordings(entries: Record<string, Recording>): Readonly<Record<string, Recording | undefined>> {
  for (const [id, recording] of Object.entries(entries)) {
    if (!/^[a-z][a-z0-9-]*$/.test(id)
      || !/^\/recordings\/[a-zA-Z0-9_-]+(?:\/[a-zA-Z0-9_-]+)*\.cast$/.test(recording.src)
      || !/^\/recordings\/[a-zA-Z0-9_-]+(?:\/[a-zA-Z0-9_-]+)*\.(png|webp|jpg|avif)$/.test(recording.poster.src)) {
      throw new Error(`${id}: recordings require local cast and poster paths`);
    }
    if (![recording.title, recording.summary, recording.poster.alt].every((text) => text.trim())
      || !['stable', 'experimental'].includes(recording.maturity)
      || ![recording.cols, recording.rows, recording.poster.width, recording.poster.height].every((value) => Number.isSafeInteger(value) && value > 0)
      || !Number.isFinite(recording.duration) || recording.duration <= 0) {
      throw new Error(`${id}: recording metadata is incomplete`);
    }
    let previous = -1;
    for (const chapter of recording.chapters) {
      if (!chapter.title.trim() || !Number.isFinite(chapter.time)
        || chapter.time < 0 || chapter.time <= previous || chapter.time >= recording.duration) {
        throw new Error(`${id}: chapters must be ordered within the recording duration`);
      }
      previous = chapter.time;
    }
  }
  return Object.freeze(entries);
}

export const recordings = defineRecordings({});
