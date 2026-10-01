import test from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const run = promisify(execFile);

test("both browser tests fail and exit without leaked fixtures when Chromium is missing", async () => {
  const directory = await mkdtemp(join(tmpdir(), "temote-missing-chromium-"));
  try {
    const env = { ...process.env, PLAYWRIGHT_BROWSERS_PATH: directory };
    delete env.NODE_TEST_CONTEXT;
    await assert.rejects(run(process.execPath, [
      "--test", "--test-reporter=tap",
      fileURLToPath(new URL("./fabric-app-browser.test.mjs", import.meta.url)),
    ], { env, timeout: 15_000, maxBuffer: 128 * 1024 }), (error) => {
      assert.equal(error.killed, false, "browser tests must exit themselves, not hit the subprocess timeout");
      assert.equal(error.code, 1, "a missing browser must remain a test failure");
      assert.match(error.stdout, /Executable doesn't exist/);
      assert.match(error.stdout, /# fail 2\b/);
      return true;
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
