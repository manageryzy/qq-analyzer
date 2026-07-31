import { defineConfig, devices } from '@playwright/test'

const baseURL = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:18765'

export default defineConfig({
  testDir: './e2e',
  timeout: 30_000,
  fullyParallel: false,
  use: { baseURL, trace: 'retain-on-failure' },
  webServer: process.env.PLAYWRIGHT_BASE_URL ? undefined : {
    command: 'python ../scripts/create_web_fixture.py && cargo run --manifest-path ../rust-msg3-parser/Cargo.toml --features image-index-qdrant,web-ui --bin qq_analyzer_rs -- serve --root ../output/_web-fixture-workspace --account 10001 --port 18765',
    cwd: '.',
    url: `${baseURL}/api/status`,
    timeout: 300_000,
    reuseExistingServer: true,
  },
  projects: [
    { name: 'desktop', use: { ...devices['Desktop Chrome'] } },
    { name: 'mobile', use: { ...devices['Pixel 7'] } },
  ],
})
