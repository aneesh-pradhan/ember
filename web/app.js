"use strict";

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const STAGES = [
  ["fetch", "Fetch"], ["setup", "Sandbox"], ["triage", "Triage"], ["investigate", "Investigate"],
  ["reproduce", "Reproduce"], ["plan", "Plan"], ["fix", "Fix"], ["review", "Review"],
];
const TIERS = { fast: "Nano", coder: "Super", reasoner: "Ultra" };
const BRANCH_TONE = { passed: "green", failing: "red", regressed: "red", invalid: "amber", gave_up: "red", error: "red", stopped: "", testing: "blue", running: "blue" };
const STATUS_TEXT = { fixed: "Fixed", not_reproduced: "Not reproduced", not_fixed: "No passing fix", not_a_bug: "Not a bug", error: "Error" };

let config = null;
let source = null;
let state = null;

// ------------------------------------------------------------------ routing

function route() {
  const match = location.hash.match(/^#\/run\/([\w-]+)/);
  if (match) openRun(match[1]);
  else showHome();
}
window.addEventListener("hashchange", route);

function showHome() {
  closeStream();
  $("run").hidden = true;
  $("home").hidden = false;
  loadRuns();
}

// ------------------------------------------------------------------ home

async function init() {
  config = await (await fetch("/api/config")).json();
  $("stack").innerHTML =
    Object.entries(config.models).map(([role, model]) =>
      `<span class="chip ${role}" title="${esc(model)}"><b>${TIERS[role]}</b> ${esc(model.split("/").pop())}</span>`).join("");
  $("branches").value = String(config.branches);
  if (!config.live_runs) {
    $("run-btn").disabled = true;
    $("live-note").textContent = "Live runs are off on this deployment. Replay a recorded run below.";
  }
  route();
}

async function loadRuns() {
  const runs = await (await fetch("/api/runs")).json();
  if (!runs.length) {
    $("runs").innerHTML = `<p class="muted">No runs yet.</p>`;
    return;
  }
  $("runs").innerHTML = runs.map((r) => `
    <a class="run-card" href="#/run/${esc(r.id)}">
      <div class="meta">${statusPill(r.status)}${r.recorded ? `<span class="pill">recorded</span>` : ""}</div>
      <div class="title">${esc(r.title)}</div>
      <div class="meta"><span>${esc(r.repo || "")}</span><span>·</span><span>${fmtTime(r.seconds)}</span></div>
    </a>`).join("");
}

$("run-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = { branches: Number($("branches").value) };
  const url = $("issue-url").value.trim();
  if (url) body.issue_url = url;
  else {
    body.repo = $("repo-url").value.trim();
    body.description = $("description").value.trim();
  }
  $("form-error").hidden = true;
  $("run-btn").disabled = true;
  try {
    const res = await fetch("/api/runs", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
    const data = await res.json();
    if (!res.ok) throw new Error(data.detail || "Could not start the run");
    location.hash = `#/run/${data.id}`;
  } catch (err) {
    $("form-error").textContent = err.message;
    $("form-error").hidden = false;
  } finally {
    $("run-btn").disabled = !config.live_runs;
  }
});

// ------------------------------------------------------------------ run view

function freshState(id) {
  return {
    id, t: 0, done: false, stages: {}, issue: null, investigation: null, repros: [], reproTab: null,
    strategies: [], branches: {}, activity: [], usage: {}, execs: 0, result: null, error: null, models: {},
  };
}

function openRun(id) {
  closeStream();
  state = freshState(id);
  $("home").hidden = true;
  $("run").hidden = false;
  window.scrollTo(0, 0);
  renderAll();
  // Saved runs replay at 4x their real pacing; live runs stream as they happen.
  source = new EventSource(`/api/runs/${encodeURIComponent(id)}/events?speed=4`);
  source.onmessage = (msg) => handle(JSON.parse(msg.data));
  source.addEventListener("end", () => { state.done = true; closeStream(); renderHead(); });
  source.onerror = () => { if (state.done) closeStream(); };
}

function closeStream() {
  if (source) source.close();
  source = null;
}

function handle(ev) {
  state.t = ev.t;
  switch (ev.type) {
    case "run":
      state.models = ev.models;
      log(ev.t, `Run started · sandbox: <b>${esc(ev.sandbox)}</b>`);
      break;
    case "stage":
      state.stages[ev.stage] = ev;
      renderStages();
      break;
    case "issue":
      state.issue = ev;
      renderHead();
      break;
    case "triage":
      log(ev.t, `<b>Triage:</b> ${esc(ev.summary || "")}`);
      break;
    case "investigate_step":
      if (ev.action !== "finish") log(ev.t, `<b>${esc(ev.action)}</b> <span class="loc">${esc(ev.target || "")}</span> ${ev.thought ? "· " + esc(trim(ev.thought, 120)) : ""}`);
      break;
    case "investigation":
      state.investigation = ev;
      renderInvestigation();
      break;
    case "llm_call": {
      const u = state.usage[ev.role] || (state.usage[ev.role] = { calls: 0, tokens: 0, seconds: 0, model: ev.model });
      u.calls += 1;
      u.tokens += (ev.prompt_tokens || 0) + (ev.completion_tokens || 0);
      u.seconds += ev.seconds;
      log(ev.t, `<span class="tag ${ev.role}">${TIERS[ev.role] || ev.role}</span>${esc(ev.purpose)} <span class="muted">${ev.seconds}s</span>`);
      renderUsage();
      break;
    }
    case "exec":
      state.execs += 1;
      log(ev.t, `<span class="tag box">SANDBOX</span>${ev.branch ? `<b>${esc(ev.branch)}</b> ` : ""}${esc(ev.label)} · <span class="${ev.exit_code === 0 ? "exit0" : "exitN"}">exit ${ev.exit_code}</span> <span class="muted">${ev.seconds}s</span>`);
      renderUsage();
      break;
    case "checkpoint":
      log(ev.t, `<span class="tag ckpt">CHECKPOINT</span><b>${esc(ev.name)}</b> <span class="loc">${esc(ev.id)}</span> ${esc(ev.detail || "")}`);
      break;
    case "note":
      log(ev.t, esc(ev.text));
      break;
    case "repro":
      state.repros.push(ev);
      state.reproTab = state.repros.length - 1;
      renderRepro();
      break;
    case "strategies":
      state.strategies = ev.items;
      for (const s of ev.items) state.branches[s.branch] = { id: s.branch, strategy: s, status: "pending", steps: [], diff: "" };
      renderBranches();
      break;
    case "branch": {
      const b = state.branches[ev.branch] || (state.branches[ev.branch] = { id: ev.branch, strategy: {}, steps: [], diff: "" });
      b.status = ev.status;
      if (ev.diff) b.diff = ev.diff;
      if (ev.status !== "running" || !b.steps.length) b.steps.push({ status: ev.status, note: ev.note, round: ev.round });
      renderBranches();
      break;
    }
    case "result":
      state.result = ev;
      $("run-clock").textContent = fmtTime(ev.t);
      renderResult();
      renderBranches();
      renderHead();
      break;
    case "error":
      state.error = ev.message;
      renderBanner();
      break;
  }
}

function log(t, html) {
  state.activity.push({ t, html });
  const li = document.createElement("li");
  li.innerHTML = `<span class="t">${fmtTime(t)}</span><span class="what">${html}</span>`;
  const list = $("activity");
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  list.appendChild(li);
  if (atBottom) list.scrollTop = list.scrollHeight;
  $("run-clock").textContent = fmtTime(t);
}

// ------------------------------------------------------------------ renderers

function renderAll() {
  $("activity").innerHTML = "";
  for (const id of ["panel-investigation", "panel-repro", "panel-branches", "panel-result"]) $(id).hidden = true;
  $("banner").innerHTML = "";
  renderHead();
  renderStages();
  renderUsage();
}

function renderHead() {
  $("run-title").textContent = state.issue ? state.issue.title : "Starting…";
  $("run-repo").innerHTML = state.issue
    ? `${esc(state.issue.repo)}${state.issue.url ? ` · <a href="${esc(state.issue.url)}" target="_blank" rel="noopener">issue</a>` : ""}`
    : "";
  const status = state.result ? state.result.status : "running";
  $("run-status").outerHTML = `<span id="run-status">${statusPill(status, !state.result)}</span>`;
}

function statusPill(status, live = false) {
  const tone = { fixed: "green", running: "blue", not_reproduced: "amber", not_a_bug: "amber", not_fixed: "red", error: "red" }[status] || "";
  const text = STATUS_TEXT[status] || "Running";
  return `<span class="pill ${tone} ${live ? "live" : ""}"><span class="dot"></span>${text}</span>`;
}

function renderStages() {
  $("stages").innerHTML = STAGES.map(([key, name]) => {
    const s = state.stages[key];
    return `<li class="stage ${s ? s.status : ""}" title="${esc(s?.detail || "")}"><b>${name}</b><small>${esc(s?.detail || "")}</small></li>`;
  }).join("");
}

function renderBanner() {
  $("banner").innerHTML = state.error ? `<div class="banner red"><b>Run failed.</b> ${esc(state.error)}</div>` : "";
}

function renderInvestigation() {
  const inv = state.investigation;
  $("panel-investigation").hidden = false;
  $("investigation").innerHTML = `
    <dl class="kv">
      <dt>Root cause</dt><dd>${esc(inv.root_cause)}</dd>
      <dt>Where</dt><dd>${(inv.locations || []).map((l) => `<span class="loc">${esc(l.path)}${l.symbol ? `:${esc(l.symbol)}` : ""}</span>`).join(" ") || "—"}</dd>
      <dt>Trigger</dt><dd>${esc(inv.how_to_trigger || "—")}</dd>
    </dl>`;
}

function renderRepro() {
  $("panel-repro").hidden = false;
  const r = state.repros[state.reproTab];
  const tabs = state.repros.length > 1
    ? `<div class="tabs">${state.repros.map((x, i) => `<button class="tab ${i === state.reproTab ? "active" : ""}" data-tab="${i}">Attempt ${x.attempt} ${x.verdict === "confirmed" ? "✓" : "✗"}</button>`).join("")}</div>`
    : "";
  const verdict = r.verdict === "confirmed"
    ? `<span class="pill red"><span class="dot"></span>Bug reproduced</span><span>Fails on the current code: ${esc(r.reason)}</span>`
    : `<span class="pill amber"><span class="dot"></span>Rejected</span><span>${esc(r.reason)}</span>`;
  $("repro").innerHTML = `${tabs}
    <div class="verdict">${verdict}</div>
    <div class="split">
      <div><p class="label">${esc(r.path)}</p><pre class="code">${esc(r.code)}</pre></div>
      <div><p class="label">pytest · exit ${r.exit_code}</p><pre class="output">${colorOutput(r.output)}</pre></div>
    </div>`;
  $("repro").querySelectorAll("[data-tab]").forEach((btn) => btn.addEventListener("click", () => { state.reproTab = Number(btn.dataset.tab); renderRepro(); }));
}

function renderBranches() {
  const list = Object.values(state.branches);
  if (!list.length) return;
  $("panel-branches").hidden = false;
  const fixStage = state.stages.fix;
  $("branch-hint").textContent = fixStage?.detail || "";
  const winner = state.result?.winner;
  $("branches-view").innerHTML = list.map((b) => {
    const tone = BRANCH_TONE[b.status] ?? "";
    const label = b.id === winner ? "winner" : (b.status || "pending").replace("_", " ");
    return `
      <article class="branch ${b.status} ${b.id === winner ? "winner" : ""}">
        <div class="branch-head"><span class="branch-id">${esc(b.id)}</span><span class="pill ${b.id === winner ? "green" : tone} ${["running", "testing"].includes(b.status) ? "live" : ""}"><span class="dot"></span>${esc(label)}</span></div>
        <h3>${esc(b.strategy.title || "")}</h3>
        <p>${esc(trim(b.strategy.approach || "", 220))}</p>
        <ul class="steps">${b.steps.map((s) => `<li class="${stepTone(s.status)}">${s.round ? `R${s.round} · ` : ""}${esc(s.note || s.status)}</li>`).join("")}</ul>
        ${b.diff ? `<details><summary>Patch</summary><pre class="diff">${colorDiff(b.diff)}</pre></details>` : ""}
      </article>`;
  }).join("");
}

function stepTone(status) {
  if (status === "passed") return "ok";
  if (["failing", "regressed", "gave_up", "error", "invalid"].includes(status)) return "bad";
  if (["testing", "running"].includes(status)) return "run";
  return "";
}

function renderResult() {
  const r = state.result;
  $("panel-result").hidden = false;
  if (r.status !== "fixed") {
    $("result").innerHTML = `<div class="result-head">${statusPill(r.status)}</div><p class="prose">${esc(r.message || "")}</p>`;
    return;
  }
  $("result").innerHTML = `
    <div class="result-head">${statusPill("fixed")}<h3>${esc(r.pr_title)}</h3></div>
    <div class="prose">
      <p><b>Selected ${esc(r.winner)}</b> (${esc(r.strategy)}) out of ${r.candidates.length} passing candidate${r.candidates.length > 1 ? "s" : ""}. ${esc(r.rationale)}</p>
      ${r.risk ? `<p><b>Reviewer note:</b> ${esc(r.risk)}</p>` : ""}
    </div>
    <div class="actions">
      <a class="ghost-link" href="/api/runs/${esc(state.id)}/patch"><button class="ghost" type="button">Download .patch</button></a>
      <button class="ghost" type="button" id="copy-diff">Copy diff</button>
      <button class="ghost" type="button" id="copy-pr">Copy PR description</button>
    </div>
    <pre class="diff">${colorDiff(r.diff)}</pre>`;
  $("copy-diff").onclick = (e) => copy(r.diff, e.target);
  $("copy-pr").onclick = (e) => copy(`# ${r.pr_title}\n\n${r.pr_body}`, e.target);
}

function renderUsage() {
  const rows = Object.entries(state.usage).map(([role, u]) => `
    <div class="usage-row"><span class="tier ${role}">${TIERS[role] || role}</span><span class="model" title="${esc(u.model)}">${esc(u.model.split("/").pop())}</span><span class="num">${u.calls}× · ${fmtTokens(u.tokens)}</span></div>`);
  rows.push(`<div class="usage-row"><span class="tier">Sandbox</span><span class="model">isolated runs</span><span class="num">${state.execs}</span></div>`);
  $("usage").innerHTML = rows.join("");
}

// ------------------------------------------------------------------ formatting

function colorDiff(diff) {
  return esc(diff).split("\n").map((line) => {
    if (line.startsWith("+++") || line.startsWith("---")) return `<span class="file">${line}</span>`;
    if (line.startsWith("@@")) return `<span class="hunk">${line}</span>`;
    if (line.startsWith("+")) return `<span class="add">${line}</span>`;
    if (line.startsWith("-")) return `<span class="del">${line}</span>`;
    return line;
  }).join("\n");
}

function colorOutput(text) {
  return esc(text).split("\n").map((line) => {
    if (/^(E |FAILED|ERROR)|\d+ failed|Error\b/.test(line)) return `<span class="fail">${line}</span>`;
    if (/\d+ passed/.test(line) && !/failed/.test(line)) return `<span class="pass">${line}</span>`;
    return line;
  }).join("\n");
}

function fmtTime(s) {
  s = Number(s) || 0;
  return s < 60 ? `${s.toFixed(1)}s` : `${Math.floor(s / 60)}m ${String(Math.round(s % 60)).padStart(2, "0")}s`;
}
const fmtTokens = (n) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k tok` : `${n} tok`);
const trim = (s, n) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

async function copy(text, button) {
  try {
    await navigator.clipboard.writeText(text);
    const old = button.textContent;
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = old), 1200);
  } catch { /* clipboard unavailable */ }
}

init();
