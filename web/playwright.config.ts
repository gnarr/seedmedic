import { defineConfig } from "@playwright/test";

/**
 * Five viewports, with 320 as the primary rather than an afterthought: it is the
 * narrowest width still in real use and the one the old UI overflowed at.
 *
 * **Firefox, not Chromium.** The bundled Chromium builds need system libraries
 * (`libnspr4.so` and friends) that are not installable without root here, while
 * Firefox ships its own — so Firefox is what actually runs. That is a constraint of
 * this machine, not a preference: CI should add
 * `npx playwright install --with-deps chromium` and run both.
 *
 * `timezoneId` and `locale` are pinned because the redesign moved timestamps from
 * server-formatted UTC to client-formatted local — a better product, and a source
 * of machine-dependent results if left alone.
 */
const shared = { browserName: "firefox" as const, timezoneId: "UTC", locale: "en-GB" };

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false,
  workers: 1,
  timeout: 45_000,
  reporter: [["list"]],
  use: { ...shared, trace: "retain-on-failure" },
  projects: [
    { name: "mobile-320", use: { ...shared, viewport: { width: 320, height: 568 }, hasTouch: true } },
    { name: "mobile-390", use: { ...shared, viewport: { width: 390, height: 844 }, hasTouch: true } },
    { name: "tablet-768", use: { ...shared, viewport: { width: 768, height: 1024 }, hasTouch: true } },
    { name: "desktop-1280", use: { ...shared, viewport: { width: 1280, height: 800 } } },
    {
      name: "desktop-dark",
      use: { ...shared, viewport: { width: 1280, height: 800 }, colorScheme: "dark" },
    },
    {
      name: "reduced-motion",
      use: { ...shared, viewport: { width: 390, height: 844 }, reducedMotion: "reduce" },
    },
  ],
});
