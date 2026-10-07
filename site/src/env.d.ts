interface ImportMetaEnv {
  readonly VITE_UMAMI_SCRIPT_URL?: string;
  readonly VITE_UMAMI_WEBSITE_ID?: string;
}

interface Window {
  umami?: { track: (event: string, data?: Record<string, string>) => void };
}
