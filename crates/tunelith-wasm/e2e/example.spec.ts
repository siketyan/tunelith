// Drives the example page: on the mock device by default, built with
// `./build.sh --features mock`, or on a real one with the environment below,
// built with `./build.sh` and allowed to the page by the policy
// `WebUsbAllowDevicesForUrls`.
//
// - TUNELITH_FIRMWARE: the path of it930x-firmware.bin
// - TUNELITH_FREQUENCY: an ISDB-T channel on air, in kHz
import { readFile } from "node:fs/promises";
import { expect, test } from "@playwright/test";

const firmware = process.env.TUNELITH_FIRMWARE ?? {
  name: "it930x-firmware.bin",
  mimeType: "application/octet-stream",
  buffer: Buffer.alloc(0),
};
const frequency = process.env.TUNELITH_FREQUENCY ?? "515143";

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await page.locator("#firmware").setInputFiles(firmware);
  await page.getByRole("button", { name: "Open" }).click();
  await expect(page.locator("#log")).toContainText(/\d+ tuners/);
});

test("records a TS", async ({ page }) => {
  const log = page.locator("#log");
  await page.getByLabel("Frequency").fill(frequency);
  await page.getByLabel("For").fill("3");
  await page.getByRole("button", { name: "Record" }).click();
  await expect(log).toContainText("locked: true");

  const download = page.waitForEvent("download");
  await page.getByText("Download stream.ts").click({ timeout: 10_000 });
  const ts = await readFile(await (await download).path());
  expect(ts.length).toBeGreaterThan(0);
  expect(ts.length % 188).toBe(0);
  const unsynced = ts.findIndex((byte, i) => i % 188 === 0 && byte !== 0x47);
  expect(unsynced, "the first packet out of sync").toBe(-1);
  await expect(log).not.toContainText("error");
});

test("reports invalid parameters", async ({ page }) => {
  await page.getByRole("combobox").first().selectOption("ISDB-S");
  await page.getByLabel("Frequency").fill("11727480");
  await page.getByRole("button", { name: "Record" }).click();
  await expect(page.locator("#log")).toContainText("error: invalid tuning parameters");
});
