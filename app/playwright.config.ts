import { defineConfig } from "@playwright/test";

// The harness is the frontend against a stubbed backend, served by Vite; the
// tests read what it renders. Port 1421 so a running `tauri dev` on 1420 is
// left alone.
export default defineConfig({
  testDir: "tests",
  // The snapshots are computed styles in JSON, the same on every platform.
  snapshotPathTemplate: "{testDir}/__snapshots__/{testFileName}/{arg}{ext}",
  fullyParallel: true,
  // The hosted macOS runners are slow enough for an interaction to race the
  // page now and then; a retry there is reported as flaky, not hidden.
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: "http://localhost:1421",
    trace: "on-first-retry",
    viewport: { width: 1280, height: 800 },
    colorScheme: "dark",
    timezoneId: "Europe/Zurich",
  },
  webServer: {
    command: "pnpm exec vite --port 1421 --strictPort",
    url: "http://localhost:1421/harness.html",
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
