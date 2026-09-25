import init, { openDevice } from "./pkg/tunelith_wasm.js";

const VENDOR_ID = 0x0511;
const $ = (id) => document.getElementById(id);
const log = (line) => ($("log").textContent += `${line}\n`);

await init();
let device;

$("open").onclick = async () => {
  try {
    const file = $("firmware").files[0];
    if (!file) throw new Error("choose it930x-firmware.bin first");
    // The page may reach only the devices the user picked, or a policy allows.
    const devices = await navigator.usb.getDevices();
    if (!devices.some((d) => d.vendorId === VENDOR_ID)) {
      await navigator.usb.requestDevice({ filters: [{ vendorId: VENDOR_ID }] });
    }
    device = await openDevice(new Uint8Array(await file.arrayBuffer()));
    log(`${device.name}, ${device.tunerCount} tuners`);
  } catch (e) {
    log(`error: ${e.message ?? e}`);
  }
};

$("tune").onsubmit = async (event) => {
  event.preventDefault();
  try {
    if (!device) throw new Error("open a device first");
    const system = $("system").value;
    const streamId = $("stream-id").value;
    const tuner = await device.openTuner(Number($("tuner").value));
    try {
      if ($("lnb").checked) await tuner.setLnb(true);
      await tuner.tune(
        system,
        Number($("frequency").value),
        streamId ? Number(streamId) : undefined,
        $("polarization").value,
      );
      const signal = await tuner.signal();
      log(`locked: ${signal.locked}, C/N: ${signal.cnrDb?.toFixed(2)} dB`);

      const chunks = [];
      let bytes = 0;
      const end = performance.now() + Number($("seconds").value) * 1000;
      while (performance.now() < end) {
        const chunk = await tuner.read();
        if (!chunk) break;
        chunks.push(chunk);
        bytes += chunk.length;
      }
      log(`${bytes} bytes`);

      const link = document.createElement("a");
      link.href = URL.createObjectURL(new Blob(chunks));
      link.download = system === "ISDB-S3" ? "stream.tlv" : "stream.ts";
      link.textContent = `Download ${link.download}`;
      $("log").append(link, "\n");
    } finally {
      await tuner.close();
    }
  } catch (e) {
    log(`error: ${e.message ?? e}`);
  }
};
