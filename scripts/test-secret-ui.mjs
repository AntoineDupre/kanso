// Optional real-browser integration test. Build `kanso` first; install Playwright
// in your test tooling, or point PLAYWRIGHT_MODULE at its index.mjs. Uses only
// disposable local data and an isolated HOME; never connects to real sessions.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFile, spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, access } from "node:fs/promises";
import { createServer } from "node:http";
import { createServer as createSocket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || "playwright");
const exec = promisify(execFile);
const root = fileURLToPath(new URL("..", import.meta.url));
const binary = join(root, "target/debug/kanso");
const temp = await mkdtemp(join(tmpdir(), "kanso-secret-browser-"));
const home = join(temp, "home");
await mkdir(home, { mode: 0o700 });
const sends = [];
let failSend = false;
const mock = createServer(async (request, response) => {
  let body = "";
  for await (const chunk of request) body += chunk;
  const input = body ? JSON.parse(body) : {};
  if (input.action === "session.send") sends.push(input);
  response.setHeader("Content-Type", "application/json");
  if (input.action === "session.send" && failSend) {
    response.statusCode = 500;
    response.end(JSON.stringify({ error: "UNTRUSTED_UPSTREAM_BODY_MUST_NOT_RENDER" }));
    return;
  }
  response.end(JSON.stringify(input.action === "session.send"
    ? { promptDispatched: true }
    : input.action === "session.list" ? { sessions: [] }
      : { version: "local-test", openchamberVersion: "2.6.0" }));
});
await new Promise(resolve => mock.listen(0, "127.0.0.1", resolve));
const socket = createSocket();
await new Promise(resolve => socket.listen(0, "127.0.0.1", resolve));
const port = socket.address().port;
await new Promise(resolve => socket.close(resolve));
const base = `http://127.0.0.1:${port}`;
const environment = {
  ...process.env,
  HOME: home,
  KANSO_URL: base,
  KANSO_DATA_DIR: join(temp, "data"),
  KANSO_BIND: `127.0.0.1:${port}`,
  KANSO_PASSWORD: "disposable-browser-test-only",
  KANSO_OPENCHAMBER_URL: `http://127.0.0.1:${mock.address().port}`,
  KANSO_OPENCHAMBER_TOKEN: "local-mock-test-only",
  KANSO_LOG: "info",
};
let browser;
const server = spawn(binary, ["serve"], { cwd: root, env: environment, stdio: ["ignore", "pipe", "pipe"] });
try {
  await new Promise((resolve, reject) => {
    let output = "";
    const timer = setTimeout(() => reject(new Error("Local test server did not start")), 15000);
    const read = data => {
      output += data;
      if (output.includes("kanso listening")) { clearTimeout(timer); resolve(); }
    };
    server.stdout.on("data", read);
    server.stderr.on("data", read);
    server.once("error", error => { clearTimeout(timer); reject(error); });
    server.once("exit", () => { clearTimeout(timer); reject(new Error("Local test server exited")); });
  });
  const login = await fetch(`${base}/api/auth/login`, {
    method: "POST", headers: { "Content-Type": "application/json", Origin: base },
    body: JSON.stringify({ password: environment.KANSO_PASSWORD }),
  });
  assert.equal(login.status, 204);
  const cookie = login.headers.get("set-cookie").split(";")[0];
  async function api(path, data) {
    const response = await fetch(`${base}${path}`, {
      method: "POST", headers: { "Content-Type": "application/json", Origin: base, Cookie: cookie },
      body: JSON.stringify(data),
    });
    assert.ok(response.ok, `Local setup request failed: ${path} (${response.status})`);
    return response.status === 204 ? null : response.json();
  }
  const project = await api("/api/projects", { key: "UIT", name: "Local secret handoff verification" });
  const card = await api("/api/tasks", { project_id: project.id, title: "Verify one-time secret UI" });
  const reference = card.reference;
  await api(`/api/tasks/${reference}/session`, { session_id: "ses_disposable_ui", directory: "/local-test", status: "waiting" });
  await api(`/api/tasks/${reference}/move`, { status: "in_progress" });
  environment.KANSO_TOKEN = (await api("/api/tokens", { name: "disposable-ui-agent", scope: "agent" })).token;
  const request = await exec(binary, ["secret", "request", reference, "--label", "Disposable test token", "--purpose", "Verify masked multiline entry and one-time execution"], { env: environment });
  const id = request.stdout.trim();
  assert.match(id, /^[0-9a-f-]{36}$/);
  const capabilityPath = join(home, ".local/state/kanso/secret-handoffs", id);
  const capability = await readFile(capabilityPath, "utf8");
  assert.ok(!request.stdout.includes(capability) && !request.stderr.includes(capability));

  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 960 } });
  const errors = [];
  page.on("pageerror", error => errors.push(error.message));
  const auth = await page.request.post(`${base}/api/auth/login`, {
    headers: { Origin: base }, data: { password: environment.KANSO_PASSWORD },
  });
  assert.equal(auth.status(), 204);
  await page.goto(`${base}/p/UIT/task/1`);
  const field = page.locator("[data-secret-input]");
  await field.waitFor();
  const value = " \ndisposable-ui-value\n ";
  await field.fill(value);
  assert.equal(await field.getAttribute("data-draft"), null);
  assert.equal(await field.evaluate(node => getComputedStyle(node).color), "rgba(0, 0, 0, 0)");
  assert.ok(!(await page.locator(".secret-mask").textContent()).includes(value.trim()));
  await page.getByRole("button", { name: "Reveal value", exact: true }).click();
  assert.notEqual(await field.evaluate(node => getComputedStyle(node).color), "rgba(0, 0, 0, 0)");
  assert.equal(await field.inputValue(), value);
  await page.getByRole("button", { name: "Hide value", exact: true }).click();
  if (process.env.KANSO_UI_SCREENSHOT) await page.screenshot({ path: process.env.KANSO_UI_SCREENSHOT });
  const storage = await page.evaluate(() => JSON.stringify({ local: { ...localStorage }, session: { ...sessionStorage } }));
  assert.ok(!storage.includes(value.trim()) && !storage.includes(capability));
  await page.reload();
  await field.waitFor();
  assert.equal(await field.inputValue(), "");
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => {
    const field = document.querySelector("[data-secret-input]");
    if (!field) return false;
    const box = field.getBoundingClientRect();
    return box.x >= 0 && box.width > 0 && box.right <= 391;
  });
  await page.getByRole("button", { name: "Reveal value", exact: true }).click();
  await page.getByRole("button", { name: "Hide value", exact: true }).click();
  await page.setViewportSize({ width: 1280, height: 960 });
  await field.fill(value);
  await page.getByRole("button", { name: "Submit secret", exact: true }).click();
  await page.getByText("State: ready", { exact: true }).waitFor();
  await page.getByText("Session notification: sent", { exact: true }).waitFor();
  assert.equal(await field.count(), 0);
  assert.equal(sends.length, 1);
  const sent = JSON.stringify(sends[0]);
  assert.ok(sent.includes(id) && !sent.includes(value.trim()) && !sent.includes(capability));
  const digest = createHash("sha256").update(value).digest("hex");
  const childCode = `process.exit(require('node:crypto').createHash('sha256').update(process.env.TEST_TOKEN).digest('hex')==='${digest}'?17:18)`;
  await assert.rejects(
    exec(binary, ["secret", "exec", id, "--env", "TEST_TOKEN", "--", process.execPath, "-e", childCode], { env: environment }),
    error => error.code === 17 && error.stdout === "" && error.stderr === "",
  );
  await assert.rejects(access(capabilityPath));
  await page.getByText("State: consumed", { exact: true }).waitFor();
  assert.equal(await page.getByRole("button", { name: "Retry", exact: true }).count(), 0);

  async function nextRequest() {
    const result = await exec(binary, ["secret", "request", reference, "--label", "Disposable follow-up", "--purpose", "Verify decline, retry and cancellation"], { env: environment });
    await page.getByText("State: waiting", { exact: true }).waitFor();
    return result.stdout.trim();
  }
  const declinedId = await nextRequest();
  await page.getByRole("textbox", { name: "Optional decline explanation — do not include credentials", exact: true }).fill("No access available");
  await page.getByRole("button", { name: "Decline request", exact: true }).click();
  await page.getByText("State: declined", { exact: true }).waitFor();
  await page.getByText("Session notification: sent", { exact: true }).waitFor();
  assert.equal(await field.count(), 0);
  const declinedStatus = await exec(binary, ["secret", "status", declinedId], { env: environment });
  assert.equal(JSON.parse(declinedStatus.stdout).state, "declined");
  await assert.rejects(access(join(home, ".local/state/kanso/secret-handoffs", declinedId)));

  const cancelId = await nextRequest();
  const sendsBeforeCancel = sends.length;
  await page.getByRole("button", { name: "Cancel handoff", exact: true }).click();
  await page.getByText("State: cancelled", { exact: true }).waitFor();
  assert.equal(sends.length, sendsBeforeCancel);
  await exec(binary, ["secret", "cancel", cancelId], { env: environment });
  await assert.rejects(access(join(home, ".local/state/kanso/secret-handoffs", cancelId)));

  const retryId = await nextRequest();
  failSend = true;
  await field.fill("disposable-retry-value");
  await page.getByRole("button", { name: "Submit secret", exact: true }).click();
  await page.getByText("Session notification: failed", { exact: true }).waitFor();
  assert.ok(!(await page.locator("body").innerText()).includes("UNTRUSTED_UPSTREAM_BODY_MUST_NOT_RENDER"));
  failSend = false;
  await page.getByRole("button", { name: "Retry", exact: true }).click();
  await page.getByText("Session notification: sent", { exact: true }).waitFor();
  assert.deepEqual(sends.at(-1), sends.at(-2));
  await page.getByRole("button", { name: "Cancel handoff", exact: true }).click();
  await page.getByText("State: cancelled", { exact: true }).waitFor();
  await exec(binary, ["secret", "cancel", retryId], { env: environment });
  assert.equal(await field.count(), 0);
  assert.deepEqual(errors, []);
  console.log("PASS: desktop/mobile UI masking/reveal, draft/storage isolation, refresh clearing, metadata-only wake-up, exact environment injection, child exit status, one-time capability cleanup, decline/cancellation, safe retry and live SSE status.");
} finally {
  if (browser) await browser.close();
  if (server.exitCode === null) {
    const exited = new Promise(resolve => server.once("exit", resolve));
    server.kill("SIGTERM");
    await exited;
  }
  await new Promise(resolve => mock.close(resolve));
}
