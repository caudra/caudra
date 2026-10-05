import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/browser',
  fullyParallel: true,
  workers: 2,
  use: { baseURL: 'http://127.0.0.1:4322', trace: 'retain-on-failure', permissions: ['clipboard-read', 'clipboard-write'] },
  projects: [
    { name: 'desktop-light', use: { viewport: { width: 1600, height: 1000 }, colorScheme: 'light' } },
    { name: 'desktop-dark', use: { viewport: { width: 1600, height: 1000 }, colorScheme: 'dark' } },
    { name: 'mobile-light', use: { viewport: { width: 390, height: 844 }, colorScheme: 'light', isMobile: true } },
    { name: 'mobile-dark', use: { viewport: { width: 390, height: 844 }, colorScheme: 'dark', isMobile: true } },
  ],
  webServer: { command: 'bun run preview --host 127.0.0.1 --port 4322', url: 'http://127.0.0.1:4322', reuseExistingServer: false },
});
