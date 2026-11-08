// Optional browser test; build target/debug/kanso first and set PLAYWRIGHT_MODULE
// if Playwright is not installed locally. Uses a private fake gh and synthetic
// code only; it never runs the installed gh or reads real credential files.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile, chmod, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || "playwright");
const root = fileURLToPath(new URL("..", import.meta.url));
const baseTemp = join(homedir(), ".cache/tmp/opencode");
await mkdir(baseTemp, { recursive: true });
const temp = await mkdtemp(join(baseTemp, "kanso-github-browser-"));
const bin = join(temp, "bin");
await mkdir(bin);
const fake = join(bin, "gh");
await writeFile(fake, `#!/bin/sh
case "$1 $2" in
 '--version ') printf 'gh version 2.97.0 (fixture)\\n' ;;
 'api --hostname') exit 1 ;;
 'auth status') printf '{"hosts":{}}\\n' ;;
 'auth login') printf '! First copy your one-time code: TEST-1234\\n' >&2; exec sleep 600 ;;
 *) exit 1 ;;
esac
`, { mode: 0o700 });
await chmod(fake, 0o700);
const socket = createServer();
await new Promise(resolve => socket.listen(0, "127.0.0.1", resolve));
const port = socket.address().port;
await new Promise(resolve => socket.close(resolve));
const origin = `http://127.0.0.1:${port}`;
const env = { ...process.env, HOME: temp, PATH: `${bin}:${process.env.PATH}`, GH_CONFIG_DIR: join(temp, "gh-config"), KANSO_DATA_DIR: join(temp, "data"), KANSO_BIND: `127.0.0.1:${port}`, KANSO_PASSWORD: "fixture-password" };
for (const key of ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST", "KANSO_OPENCHAMBER_URL", "KANSO_OPENCHAMBER_TOKEN", "KANSO_PUBLIC_URL"]) delete env[key];
const server = spawn(join(root, "target/debug/kanso"), ["serve"], { cwd: root, env, stdio: "ignore" });
let serverError = null;
server.on("error", error => { serverError = error; });
let browser;
try {
  let ready = false;
  for (let i = 0; i < 100; i++) {
    if (serverError || server.exitCode !== null || server.signalCode !== null) break;
    try { if ((await fetch(`${origin}/api/health`)).ok) { ready = true; break; } } catch {}
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(ready, "isolated fixture server did not start");
  browser = await chromium.launch({ headless: true });
  const owner = await browser.newContext({ viewport: { width: 1440, height: 900 }, permissions: ["clipboard-read", "clipboard-write"] });
  const page = await owner.newPage();
  async function login(tab) {
    await tab.goto(`${origin}/settings/integration`);
    await tab.locator("#login-password").fill("fixture-password");
    await tab.getByRole("button", { name: "Sign in" }).click();
    await tab.getByRole("button", { name: "Settings", exact: true }).waitFor();
    await tab.goto(`${origin}/settings/integration`);
  }
  await login(page);
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByText("TEST-1234", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Copy code" }).click();
  assert.equal(await page.evaluate(() => navigator.clipboard.readText()), "TEST-1234");
  if (process.env.KANSO_GH_SCREENSHOTS) await page.screenshot({ path: join(process.env.KANSO_GH_SCREENSHOTS, "desktop.png") });
  let releaseProgress = false;
  await page.route("**/api/github/login", async route => {
    if (!releaseProgress && route.request().method() === "GET") await route.abort();
    else await route.continue();
  });
  await page.getByText("Login progress is temporarily unavailable; retrying.").waitFor();
  releaseProgress = true;
  await page.getByText("TEST-1234", { exact: true }).waitFor();
  await page.unroute("**/api/github/login");
  await page.goto("about:blank");
  let releaseInitialProgress = false;
  await page.route("**/api/github/login", async route => {
    if (!releaseInitialProgress && route.request().method() === "GET") await route.abort();
    else await route.continue();
  });
  await page.goto(`${origin}/settings/integration`);
  await page.getByText("Login progress is temporarily unavailable; retrying.").waitFor();
  releaseInitialProgress = true;
  await page.getByText("TEST-1234", { exact: true }).waitFor();
  await page.unroute("**/api/github/login");
  await page.setViewportSize({ width: 390, height: 844 });
  if (process.env.KANSO_GH_SCREENSHOTS) await page.screenshot({ path: join(process.env.KANSO_GH_SCREENSHOTS, "mobile.png"), fullPage: true });
  const other = await browser.newContext({ viewport: { width: 390, height: 844 } });
  const second = await other.newPage();
  await login(second);
  await second.getByText("Another person's login is in progress.").waitFor();
  assert.equal(await second.getByText("TEST-1234", { exact: true }).count(), 0);
  assert.equal(await second.getByRole("button", { name: "Cancel", exact: true }).count(), 0);
  await page.getByRole("button", { name: "Cancel", exact: true }).click();
  await page.getByText(/Local login cancelled/).waitFor();
  assert.equal(await page.getByText("TEST-1234", { exact: true }).count(), 0);
  await other.close();
  await owner.close();
  console.log("GitHub settings fixture browser checks passed.");
} finally {
  if (browser) await browser.close();
  if (!serverError && server.exitCode === null && server.signalCode === null) {
    const exited = new Promise(resolve => server.once("exit", resolve));
    server.kill("SIGTERM");
    await exited;
  }
  await rm(temp, { recursive: true, force: true });
}
