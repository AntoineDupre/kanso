// Optional real-browser UI check against disposable local Kanso data. Cloud
// responses are intercepted only in this browser, never enabled in production.
// Build first; install Playwright in test tooling or set PLAYWRIGHT_MODULE.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir } from "node:fs/promises";
import { createServer } from "node:net";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || "playwright");
const root = fileURLToPath(new URL("..", import.meta.url));
const cache = join(homedir(), ".cache/tmp/opencode");
await mkdir(cache, { recursive: true });
const temp = await mkdtemp(join(cache, "kanso-imports-browser-"));
const socket = createServer();
await new Promise((resolve) => socket.listen(0, "127.0.0.1", resolve));
const port = socket.address().port;
await new Promise((resolve) => socket.close(resolve));
const base = `http://127.0.0.1:${port}`;
const password = "disposable-import-browser-test";
const env = {
  ...process.env,
  KANSO_BIND: `127.0.0.1:${port}`,
  KANSO_PUBLIC_URL: base,
  KANSO_DATA_DIR: join(temp, "data"),
  KANSO_PASSWORD: password,
  KANSO_OPENCHAMBER_URL: "",
  KANSO_OPENCHAMBER_TOKEN: "",
  KANSO_LOG: "info"
};
const server = spawn(join(root, "target/debug/kanso"), ["serve"], {
  cwd: root, env, stdio: ["ignore", "pipe", "pipe"]
});
let browser;
try {
  await new Promise((resolve, reject) => {
    let output = "";
    const timeout = setTimeout(() => reject(new Error("Local Kanso did not start")), 15000);
    const read = (chunk) => {
      output += chunk;
      if (output.includes("kanso listening")) { clearTimeout(timeout); resolve(); }
    };
    server.stdout.on("data", read);
    server.stderr.on("data", read);
    server.once("error", (error) => { clearTimeout(timeout); reject(error); });
    server.once("exit", () => { clearTimeout(timeout); reject(new Error("Local Kanso exited")); });
  });
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(base);
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByRole("button", { name: "Imports", exact: true }).waitFor();
  const setup = await page.request.post(`${base}/api/projects`, {
    headers: { Origin: base }, data: { key: "UIT", name: "Import browser verification" }
  });
  assert.equal(setup.status(), 201);
  const project = await setup.json();
  const localTask = await page.request.post(`${base}/api/tasks`, {
    headers: { Origin: base },
    data: { project_id: project.id, title: "Verify imported detail", description: "<script>alert(1)</script> **External** task" }
  });
  assert.equal(localTask.status(), 201);
  await page.goto(`${base}/settings/projects`);
  const githubToggle = page.getByRole("button", { name: "GitHub repositories", exact: true });
  let releaseRepositories;
  const blockedRepositories = new Promise((resolve) => { releaseRepositories = resolve; });
  const configPattern = "**/api/projects/*/github-repositories";
  await page.route(configPattern, async (route) => { await blockedRepositories; await route.continue(); });
  const requested = page.waitForRequest(configPattern);
  await githubToggle.click();
  await requested;
  await githubToggle.click();
  const response = page.waitForResponse(configPattern);
  releaseRepositories();
  await response;
  await page.evaluate(() => new Promise(requestAnimationFrame));
  assert.equal(await page.getByRole("button", { name: "Add repository", exact: true }).count(), 0);
  await page.unroute(configPattern);
  await githubToggle.click();
  await page.getByRole("button", { name: "Add repository", exact: true }).click();
  await page.getByLabel("GitHub repository", { exact: true }).fill("fixture/repository");
  let releaseSave;
  const blockedSave = new Promise((resolve) => { releaseSave = resolve; });
  await page.route(configPattern, async (route) => { await blockedSave; await route.continue(); });
  const saving = page.waitForRequest((request) => request.url().endsWith("/github-repositories") && request.method() === "PUT");
  const save = page.getByRole("button", { name: "Save repositories", exact: true });
  await save.click();
  await saving;
  assert.equal(await githubToggle.isDisabled(), true);
  assert.equal(await save.isDisabled(), true);
  releaseSave();
  await page.getByText("GitHub repositories saved", { exact: true }).waitFor();
  assert.equal(await githubToggle.isEnabled(), true);
  await page.unroute(configPattern);
  const configured = await page.request.get(`${base}/api/projects/${project.id}/github-repositories`);
  assert.equal((await configured.json())[0].repository, "fixture/repository");
  await page.goto(`${base}/p/UIT`);
  await page.getByRole("button", { name: "Imports", exact: true }).click();
  await page.getByLabel("Provider", { exact: true }).waitFor();
  assert.equal(await page.getByLabel("Provider", { exact: true }).locator('option[value="github"]').count(), 1);
  assert.equal(await page.getByRole("button", { name: "Import selected", exact: true }).count(), 0);

  const candidate = {
    identity: "fixture-1", title: "<img src=x onerror=alert(1)> Import safely", kind: "issue",
    description: "**External** task", source_url: "https://example.org/issues/1"
  };
  let previewNumber = 0;
  let failPreview = false;
  let confirmNumber = 0;
  await page.route("**/api/imports/providers**", (route) => route.fulfill({ json: [{
    id: "browser-fixture", name: "Browser-only fixture",
    sources: [{ id: "source", name: "Example repository" }], filters: [{ id: "query", label: "Query" }]
  }] }));
  await page.route("**/api/imports/preview", async (route) => {
    const input = route.request().postDataJSON();
    assert.equal(input.project_id, project.id);
    assert.equal(input.provider, "browser-fixture");
    previewNumber += 1;
    if (failPreview) {
      await route.fulfill({ status: 503, json: { error: { code: "provider_error", message: "Provider preview failed" } } });
      return;
    }
    await route.fulfill({ json: {
      preview_id: `preview-${previewNumber}`,
      candidates: [
        { index: 0, valid: true, existing: null, candidate },
        { index: 1, valid: false, existing: null, candidate: {
          ...candidate, identity: "unsafe", title: "Unsafe source URL", source_url: "javascript:alert(1)"
        } }
      ]
    } });
  });
  await page.route("**/api/imports/confirm", async (route) => {
    const input = route.request().postDataJSON();
    assert.deepEqual(input.selected, [0]);
    assert.equal(input.candidates, undefined);
    assert.equal(input.project_id, project.id);
    confirmNumber += 1;
    await route.fulfill({ json: [{ index: 0, status: confirmNumber === 1 ? "created" : "skipped", reference: "UIT-1", message: null }] });
  });
  await page.route("**/api/tasks/UIT-1", async (route) => {
    const response = await route.fetch();
    const detail = await response.json();
    await route.fulfill({ response, json: {
      ...detail, provenance: { provider: "browser-fixture", identity: candidate.identity, kind: "issue", source_url: candidate.source_url }
    } });
  });
  await page.reload();
  const destination = page.getByLabel("Destination project", { exact: true });
  const provider = page.getByLabel("Provider", { exact: true });
  const source = page.getByLabel("Source", { exact: true });
  const preview = page.getByRole("button", { name: "Preview", exact: true });
  assert.equal(await preview.isDisabled(), true);
  await destination.selectOption(project.id);
  await provider.selectOption("browser-fixture");
  assert.equal(await page.getByLabel("GitHub category", { exact: true }).isVisible(), false);
  await source.selectOption("source");
  await destination.selectOption("");
  assert.equal(await provider.inputValue(), "");
  assert.equal(await source.inputValue(), "");
  assert.equal(await source.locator("option").count(), 1);
  assert.equal(await preview.isDisabled(), true);
  await provider.selectOption("browser-fixture");
  assert.equal(await source.locator("option").count(), 1);
  await destination.selectOption(project.id);
  await provider.selectOption("browser-fixture");
  await source.selectOption("source");
  await page.getByLabel("Query", { exact: true }).fill("assigned to me");
  await preview.click();
  const choice = page.getByRole("checkbox", { name: `Select ${candidate.title}`, exact: true });
  const confirm = page.getByRole("button", { name: "Import selected", exact: true });
  await choice.waitFor();
  assert.equal(await choice.isChecked(), false);
  assert.equal(await confirm.isDisabled(), true);
  assert.equal(await page.getByRole("checkbox", { name: "Select Unsafe source URL", exact: true }).isDisabled(), true);
  assert.equal(await page.locator(".import-candidate img, .import-candidate script").count(), 0);
  assert.equal(await page.locator("a[href^='javascript:']").count(), 0);
  await choice.check();
  await confirm.click();
  await page.getByRole("status").getByRole("link", { name: "UIT-1", exact: true }).waitFor();
  assert.equal(await page.getByRole("link", { name: "UIT-1", exact: true }).getAttribute("href"), "/p/UIT/task/1");
  await confirm.click();
  await page.getByRole("status").getByText("skipped", { exact: false }).waitFor();
  const screenshots = process.env.KANSO_IMPORTS_SCREENSHOTS;
  if (screenshots) {
    await mkdir(resolve(screenshots), { recursive: true });
    await page.screenshot({ path: join(resolve(screenshots), "imports-desktop.png"), fullPage: true });
  }
  await page.setViewportSize({ width: 360, height: 780 });
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  if (screenshots) await page.screenshot({ path: join(resolve(screenshots), "imports-mobile.png"), fullPage: true });
  await page.getByLabel("Query", { exact: true }).fill("changed");
  assert.equal(await confirm.count(), 0);
  failPreview = true;
  await preview.click();
  await page.getByRole("alert").getByText("Provider preview failed", { exact: true }).waitFor();
  assert.equal(await confirm.count(), 0);
  failPreview = false;
  await preview.click();
  await choice.check();
  await confirm.click();
  await page.getByRole("status").getByRole("link", { name: "UIT-1", exact: true }).click();
  await page.getByText("Imported from", { exact: true }).waitFor();
  const sourceLink = page.getByRole("link", { name: "Open source", exact: true });
  assert.equal(await sourceLink.getAttribute("href"), candidate.source_url);
  assert.equal(await sourceLink.getAttribute("rel"), "noopener noreferrer");
  assert.equal(await page.locator("#panel-root script").count(), 0);
  assert.deepEqual(errors, []);
  console.log("Imports real-browser verification passed (production registration, selection, safe rendering, retry, invalidation, errors, desktop/mobile).");
} finally {
  if (browser) await browser.close();
  server.kill("SIGTERM");
}
