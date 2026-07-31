/**
 * The sweep: properties that must hold on every route at every viewport.
 *
 * These are the assertions the old UI could not have passed — it had no responsive
 * rule of any kind, ~34px buttons, and no focus styles.
 */

import { test, expect, type Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import { scenario, start, stop } from "./serve.mjs";

const ROUTES = ["/", "/repairs", "/review", "/diagnostics", "/repairs/1", "/no-such-page"];

let server: Awaited<ReturnType<typeof start>> | undefined;
let base = "";

test.beforeAll(async () => {
  const config = scenario({ port: 19_921, library: "matching" });
  server = await start(config);
  base = server.base;
  // Let discovery + the first ticks land so the pages have real content.
  await new Promise((resolve) => setTimeout(resolve, 3000));
});

test.afterAll(() => stop(server));

/** Where a horizontal scroll is deliberately allowed, inside its own box. */
async function overflowOffenders(page: Page) {
  return page.evaluate(() => {
    const width = document.documentElement.clientWidth;
    return [...document.querySelectorAll<HTMLElement>("*")]
      .filter((element) => {
        if (element.closest("[data-allow-xscroll]")) return false;
        const box = element.getBoundingClientRect();
        return box.width > 0 && box.right > width + 1;
      })
      .map((element) => `<${element.tagName.toLowerCase()} class="${element.className}">`.slice(0, 90));
  });
}

for (const route of ROUTES) {
  test(`no horizontal overflow at ${route}`, async ({ page }) => {
    await page.goto(`${base}${route}`);
    await page.waitForLoadState("networkidle");

    const offenders = await overflowOffenders(page);
    expect(offenders, `elements wider than the viewport: ${offenders.join(", ")}`).toEqual([]);

    const pageOverflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(pageOverflow).toBeLessThanOrEqual(0);
  });

  test(`no serious accessibility violations at ${route}`, async ({ page }) => {
    await page.goto(`${base}${route}`);
    await page.waitForLoadState("networkidle");

    const results = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"])
      .analyze();

    const serious = results.violations.filter((violation) =>
      ["serious", "critical"].includes(violation.impact ?? ""),
    );
    expect(
      serious.map((violation) => `${violation.id}: ${violation.nodes[0]?.html?.slice(0, 80)}`),
    ).toEqual([]);
  });
}

test("every tappable control clears the 44px target minimum", async ({ page, viewport }) => {
  test.skip((viewport?.width ?? 0) >= 768, "the minimum applies to touch layouts");
  await page.goto(`${base}/`);
  await page.waitForLoadState("networkidle");

  const small = await page.evaluate(() => {
    const selector = "button, input:not([type=hidden]), select, textarea, summary, [role=button]";
    return [...document.querySelectorAll<HTMLElement>(selector)]
      .filter((element) => element.offsetParent !== null)
      .map((element) => {
        const box = element.getBoundingClientRect();
        return { html: element.outerHTML.slice(0, 70), w: Math.round(box.width), h: Math.round(box.height) };
      })
      .filter((row) => row.w < 44 || row.h < 44);
  });

  expect(small).toEqual([]);
});

test("keyboard focus is visible and starts at the skip link", async ({ page }) => {
  await page.goto(`${base}/`);
  await page.waitForLoadState("networkidle");

  await page.keyboard.press("Tab");
  const first = await page.evaluate(() => ({
    text: document.activeElement?.textContent?.trim(),
    outline: getComputedStyle(document.activeElement as Element).outlineWidth,
  }));

  expect(first.text).toBe("Skip to main content");
  expect(first.outline, "a focused control must be visibly focused").not.toBe("0px");
});

test("every destination is reachable by clicking, with no typed URLs", async ({ page }) => {
  // The direct regression test for the old UI's worst defect: /status and
  // /settings could only be reached by typing the URL.
  await page.goto(`${base}/`);
  await page.waitForLoadState("networkidle");

  for (const label of ["Repairs", "Review", "Diagnostics", "Settings"]) {
    const link = page.getByRole("navigation").getByRole("link", { name: new RegExp(label) }).first();
    await expect(link, `${label} must be reachable from the dashboard`).toBeVisible();
  }
});

test("reduced motion disables transitions rather than only shortening them", async ({ page }) => {
  test.skip(test.info().project.name !== "reduced-motion");
  await page.goto(`${base}/`);
  await page.waitForLoadState("networkidle");

  const durations = await page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>("*")]
      .map((element) => getComputedStyle(element).transitionDuration)
      .filter((duration) => duration !== "0s" && !duration.startsWith("0.00001"))
      .slice(0, 5),
  );
  expect(durations).toEqual([]);
});
