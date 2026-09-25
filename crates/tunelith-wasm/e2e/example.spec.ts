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

test.describe("the example page", () => {
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
});

test("close ends a pending read", async ({ page }) => {
  await page.goto("/");
  const bytes = typeof firmware === "string" ? [...(await readFile(firmware))] : [];
  const read = await page.evaluate(
    async ({ bytes, frequency }) => {
      const url = "/pkg/tunelith_wasm.js";
      const tunelith = await import(url);
      await tunelith.default();
      const device = await tunelith.openDevice(new Uint8Array(bytes));
      const tuner = await device.openTuner(0);
      await tuner.tune("ISDB-T", frequency);
      await tuner.read();
      const pending = tuner.read();
      await tuner.close();
      return await pending;
    },
    { bytes, frequency: Number(frequency) },
  );
  expect(read).toBeUndefined();
});

test("close ends a read started as an aborted one resumes", async ({ page }) => {
  await page.goto("/");
  const bytes = typeof firmware === "string" ? [...(await readFile(firmware))] : [];
  const reads = await page.evaluate(
    async ({ bytes, frequency }) => {
      const url = "/pkg/tunelith_wasm.js";
      const tunelith = await import(url);
      await tunelith.default();
      const device = await tunelith.openDevice(new Uint8Array(bytes));
      const tuner = await device.openTuner(0);
      await tuner.tune("ISDB-T", frequency);
      await tuner.read();
      // The tune aborts the first read, and the second starts before the
      // first resumes; the close is to end the second.
      const first = tuner.read();
      const tuned = tuner.tune("ISDB-T", frequency);
      const second = tuner.read();
      await Promise.all([first, tuned]);
      await tuner.close();
      return [await first, await second];
    },
    { bytes, frequency: Number(frequency) },
  );
  expect(reads).toEqual([undefined, undefined]);
});
