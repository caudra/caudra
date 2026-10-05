import type { Player } from 'asciinema-player';
import type { Recording } from '../data/recordings';

type RecordingPlayer = Player & { addEventListener(event: 'error', callback: () => void): void };
let active: CaudraRecording | undefined;
let stylesReady: Promise<void> | undefined;

function loadPlayerStyles() {
  stylesReady ??= import('asciinema-player/dist/bundle/asciinema-player.css?url')
    .then(({ default: href }) => new Promise<void>((resolve, reject) => {
      const link = document.createElement('link');
      link.rel = 'stylesheet';
      link.href = href;
      link.dataset.recordingStyles = '';
      link.addEventListener('load', () => resolve(), { once: true });
      link.addEventListener('error', () => {
        link.remove();
        reject(new Error('Recording stylesheet could not be loaded'));
      }, { once: true });
      document.head.append(link);
    }))
    .catch((error: unknown) => {
      stylesReady = undefined;
      throw error;
    });
  return stylesReady;
}

class CaudraRecording extends HTMLElement {
  private player?: RecordingPlayer;
  private connection?: AbortController;
  private observer?: IntersectionObserver;
  private generation = 0;
  private playing = false;
  private requested = false;

  connectedCallback() {
    this.connection = new AbortController();
    const { signal } = this.connection;
    this.querySelector<HTMLElement>('.recording-actions')!.hidden = false;
    this.addEventListener('click', (event) => {
      const button = (event.target as Element).closest<HTMLButtonElement>('button[data-action], button[data-time]');
      if (!button || button.disabled) return;
      if (button.dataset.action === 'fullscreen') {
        void this.querySelector<HTMLElement>('.recording-stage')!.requestFullscreen().catch(() => {
          this.status('Fullscreen is unavailable. Scroll the terminal horizontally to inspect it.');
        });
      } else if (button.dataset.action === 'play' && this.playing) {
        this.pause();
      } else {
        void this.play(button.dataset.action === 'replay' ? 0 : button.dataset.time ? Number(button.dataset.time) : undefined);
      }
    }, { signal });
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) this.pause();
    }, { signal });
    this.observer = new IntersectionObserver(([entry]) => {
      if (!entry?.isIntersecting) this.pause();
    });
    this.observer.observe(this);
  }

  disconnectedCallback() {
    this.connection?.abort();
    this.observer?.disconnect();
    this.release();
  }

  private status(message: string) {
    this.querySelector<HTMLElement>('.recording-status')!.textContent = message;
  }

  private controls(playing: boolean) {
    this.playing = playing;
    this.querySelector<HTMLButtonElement>('[data-action="play"]')!.textContent = playing ? 'Pause recording' : 'Play recording';
    for (const button of this.querySelectorAll<HTMLButtonElement>('[data-time], [data-action="replay"], [data-action="fullscreen"]')) {
      button.disabled = !this.player || (button.dataset.action === 'fullscreen' && !document.fullscreenEnabled);
    }
  }

  private visible() {
    const bounds = this.getBoundingClientRect();
    return !document.hidden && bounds.bottom > 0 && bounds.top < innerHeight;
  }

  private pause() {
    this.requested = false;
    const player = this.player;
    void player?.pause().catch(() => { if (this.player === player) this.fail(); });
    this.controls(false);
  }

  private release() {
    this.generation++;
    this.requested = false;
    this.player?.dispose();
    this.player = undefined;
    if (active === this) active = undefined;
    this.querySelector<HTMLElement>('.recording-viewport')!.hidden = true;
    this.querySelector<HTMLElement>('.recording-poster')!.hidden = false;
    this.querySelector<HTMLButtonElement>('[data-action="play"]')!.disabled = false;
    this.controls(false);
    this.status('');
  }

  private fail() {
    this.release();
    this.status('Recording could not be loaded. The still frame and summary remain available. Try Play recording again.');
  }

  private async play(time?: number) {
    if (active !== this) active?.release();
    active = this;
    this.requested = true;
    const generation = this.generation;
    const current = () => this.isConnected && generation === this.generation && active === this;
    try {
      if (!this.player) {
        this.querySelector<HTMLButtonElement>('[data-action="play"]')!.disabled = true;
        this.status('Loading recording…');
        const [{ create }] = await Promise.all([
          import('asciinema-player'),
          loadPlayerStyles(),
        ]);
        if (!current()) return;
        const recording: Recording = JSON.parse(this.dataset.recording!);
        this.querySelector<HTMLElement>('.recording-viewport')!.hidden = false;
        this.player = create({
          url: recording.src,
          parser: 'asciicast',
          fetchOpts: { mode: 'same-origin', redirect: 'error', credentials: 'omit' },
        }, this.querySelector<HTMLElement>('.recording-player')!, {
          cols: recording.cols,
          rows: recording.rows,
          preload: false,
          autoplay: false,
          controls: true,
          loop: false,
          speed: 1,
          idleTimeLimit: Infinity,
          keystrokeOverlay: false,
          terminalFontFamily: 'var(--mono, monospace)',
          cursorMode: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'steady' : 'blinking',
          markers: recording.chapters.map((chapter) => [chapter.time, chapter.title]),
        }) as RecordingPlayer;
        this.player.addEventListener('playing', () => {
          if (!current()) return;
          if (!this.requested || !this.visible()) this.pause();
          else this.controls(true);
        });
        this.player.addEventListener('play', () => {
          if (current() && this.visible()) this.requested = true;
        });
        this.player.addEventListener('pause', () => { if (current()) this.controls(false); });
        this.player.addEventListener('ended', () => { if (current()) this.controls(false); });
        this.player.addEventListener('error', () => { if (current()) this.fail(); });
      }
      if (!this.requested || !this.visible()) {
        this.release();
        return;
      }
      if (time !== undefined) await this.player.seek(time);
      if (!current() || !this.requested || !this.visible()) return;
      await this.player.play();
      if (!current()) return;
      this.querySelector<HTMLElement>('.recording-poster')!.hidden = true;
      this.querySelector<HTMLButtonElement>('[data-action="play"]')!.disabled = false;
      this.status('');
      this.controls(this.requested && this.visible());
    } catch {
      if (current()) this.fail();
    }
  }
}

if (!customElements.get('caudra-recording')) customElements.define('caudra-recording', CaudraRecording);
