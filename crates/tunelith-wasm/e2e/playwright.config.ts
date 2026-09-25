import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  // One device, which a test takes the tuners of.
  workers: 1,
  use: {
    baseURL: "http://localhost:8000",
    // The full Chromium in its new headless mode, which has WebUSB.
    channel: "chromium",
  },
  webServer: {
    command: "python3 -m http.server -d ../example 8000",
    url: "http://localhost:8000",
    reuseExistingServer: !process.env.CI,
  },
});
