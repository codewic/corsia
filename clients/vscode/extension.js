// Corsia in VS Code's chat: a language model chat provider ("Corsia" in the model picker)
// for the models Corsia runs on this machine. Plain JavaScript, no build step.
//
// - Lists every model in Corsia's models folder (the daemon's /api/models); the running one
//   is marked. Picking another switches Corsia to it and waits while it loads.
// - Chat and agent mode: VS Code's messages, tools, tool calls and tool results go to
//   Corsia's OpenAI-style endpoint, and the answer streams back.
// - When Corsia is stopped, the first request starts it (`corsia start`).
"use strict";
const vscode = require("vscode");
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const os = require("os");

const cfg = () => vscode.workspace.getConfiguration("corsia");
const endpoint = () => cfg().get("endpoint").replace(/\/+$/, "");
const daemon = () => cfg().get("daemon").replace(/\/+$/, "");
const agentStatus = () => endpoint().replace(/\/v1$/, "") + "/api/agent";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const short = (name) => name.replace(/\.gguf$/, "");

async function getJson(url, ms = 3000) {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), ms);
  try {
    const r = await fetch(url, { signal: ctl.signal, cache: "no-store" });
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  } finally {
    clearTimeout(t);
  }
}

async function ready() {
  return (await getJson(`${daemon()}/api/models`)) !== null && (await getJson(agentStatus())) !== null;
}

function corsiaCommand() {
  const local = path.join(os.homedir(), ".local", "bin", "corsia");
  try {
    fs.accessSync(local, fs.constants.X_OK);
    return local;
  } catch {
    return "corsia";
  }
}

async function waitUntil(test, token, limitMs) {
  const end = Date.now() + limitMs;
  while (Date.now() < end) {
    if (token && token.isCancellationRequested) return false;
    if (await test()) return true;
    await sleep(2000);
  }
  return false;
}

// Corsia running with this model, starting or switching it when needed
async function ensureModel(name, token) {
  if (!(await ready())) {
    if (!cfg().get("startIfStopped")) throw new Error("Corsia is not running. Start it with: corsia start");
    spawn(corsiaCommand(), ["start"], { detached: true, stdio: "ignore" }).unref();
    if (!(await waitUntil(ready, token, 10 * 60 * 1000))) throw new Error("Corsia did not start within 10 minutes. Run `corsia status` in a terminal to see why.");
  }
  if (!name || name === "local") return;
  const m = await getJson(`${daemon()}/api/models`);
  if (!m || m.serving === name) return;
  const r = await fetch(`${daemon()}/api/models/use`, { method: "POST", headers: { "X-Corsia": "1", "Content-Type": "text/plain" }, body: name });
  if (!r.ok) {
    const j = await r.json().catch(() => ({}));
    throw new Error(j.error || `Corsia could not switch to ${short(name)} (${r.status}).`);
  }
  const switched = await waitUntil(async () => {
    const s = await getJson(`${daemon()}/api/models`);
    return !!s && s.serving === name && (await getJson(agentStatus())) !== null;
  }, token, 15 * 60 * 1000);
  if (!switched) throw new Error(`${short(name)} did not come up within 15 minutes. The management page shows what happened.`);
}

// VS Code's messages in the OpenAI chat format
function toOpenAI(messages) {
  const out = [];
  const { LanguageModelChatMessageRole: Role, LanguageModelTextPart, LanguageModelToolCallPart, LanguageModelToolResultPart } = vscode;
  for (const msg of messages) {
    const role = msg.role === Role.User ? "user" : msg.role === Role.Assistant ? "assistant" : "system";
    let text = "";
    const calls = [];
    for (const part of msg.content) {
      if (part instanceof LanguageModelTextPart) text += part.value;
      else if (part instanceof LanguageModelToolCallPart) calls.push({ id: part.callId, type: "function", function: { name: part.name, arguments: JSON.stringify(part.input ?? {}) } });
      else if (part instanceof LanguageModelToolResultPart) {
        const content = (part.content || []).map((c) => (c instanceof LanguageModelTextPart ? c.value : typeof c === "string" ? c : c && c.value !== undefined ? String(c.value) : JSON.stringify(c))).join("");
        out.push({ role: "tool", tool_call_id: part.callId, content });
      }
    }
    if (role === "assistant" && calls.length) out.push({ role, content: text || null, tool_calls: calls });
    else if (text) out.push({ role, content: text });
  }
  return out;
}

class CorsiaProvider {
  constructor() {
    this._changed = new vscode.EventEmitter();
    this.onDidChangeLanguageModelChatInformation = this._changed.event;
  }

  refresh() {
    this._changed.fire();
  }

  async provideLanguageModelChatInformation(_options, _token) {
    const m = await getJson(`${daemon()}/api/models`);
    const s = await getJson(`${daemon()}/api/settings`);
    const ctx = s && s.saved && +s.saved.CTX ? +s.saved.CTX : 32768;
    const info = (id, name, detail, tooltip) => ({
      id, name, family: "corsia", version: "1", detail, tooltip,
      maxInputTokens: Math.max(2048, ctx - 4096), maxOutputTokens: 4096,
      capabilities: { toolCalling: true, imageInput: false },
    });
    if (!m || !m.models.length) {
      return [info("local", "Corsia", "starts on use", "Corsia is not running; the first message starts it with the chosen model.")];
    }
    const list = m.models.slice().sort((a, b) => (b.name === m.serving) - (a.name === m.serving));
    return list.map((x) => info(
      x.name,
      short(x.name),
      x.name === m.serving ? "running" : "loads on use",
      `${(x.bytes / 1073741824).toFixed(1)} GiB, ${x.fits ? "fits in memory" : "streams experts from the drive"}${x.name === m.serving ? "" : ". Picking it switches Corsia to this model (about a minute)."}`
    ));
  }

  async provideLanguageModelChatResponse(model, messages, options, progress, token) {
    await ensureModel(model.id, token);
    const body = { model: "local", stream: true, messages: toOpenAI(messages) };
    const tools = (options.tools || []).map((t) => ({ type: "function", function: { name: t.name, description: t.description || "", parameters: t.inputSchema || { type: "object", properties: {} } } }));
    if (tools.length) {
      body.tools = tools;
      body.tool_choice = options.toolMode === vscode.LanguageModelChatToolMode.Required ? "required" : "auto";
    }
    const ctl = new AbortController();
    const sub = token.onCancellationRequested(() => ctl.abort());
    try {
      const r = await fetch(`${endpoint()}/chat/completions`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body), signal: ctl.signal });
      if (!r.ok || !r.body) throw new Error(`Corsia answered ${r.status}: ${(await r.text().catch(() => "")).slice(0, 300)}`);
      const calls = new Map();
      const decoder = new TextDecoder();
      let buf = "";
      for await (const chunk of r.body) {
        buf += decoder.decode(chunk, { stream: true });
        let nl;
        while ((nl = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, nl).trim();
          buf = buf.slice(nl + 1);
          if (!line.startsWith("data:")) continue;
          const data = line.slice(5).trim();
          if (data === "[DONE]") continue;
          let ev;
          try { ev = JSON.parse(data); } catch { continue; }
          const delta = ev.choices && ev.choices[0] && ev.choices[0].delta;
          if (!delta) continue;
          if (delta.content) progress.report(new vscode.LanguageModelTextPart(delta.content));
          for (const tc of delta.tool_calls || []) {
            const c = calls.get(tc.index ?? 0) || { id: "", name: "", args: "" };
            if (tc.id) c.id = tc.id;
            if (tc.function && tc.function.name) c.name += tc.function.name;
            if (tc.function && tc.function.arguments) c.args += tc.function.arguments;
            calls.set(tc.index ?? 0, c);
          }
        }
      }
      for (const [i, c] of [...calls.entries()].sort((a, b) => a[0] - b[0])) {
        let input = {};
        try { input = c.args ? JSON.parse(c.args) : {}; } catch { input = { raw: c.args }; }
        progress.report(new vscode.LanguageModelToolCallPart(c.id || `call_${i}`, c.name, input));
      }
    } finally {
      sub.dispose();
    }
  }

  async provideTokenCount(_model, text, _token) {
    const s = typeof text === "string" ? text : (text.content || []).map((p) => (p && p.value !== undefined ? String(p.value) : "")).join("");
    return Math.ceil(s.length / 3.5);
  }
}

function activate(context) {
  const provider = new CorsiaProvider();
  context.subscriptions.push(
    vscode.lm.registerLanguageModelChatProvider("corsia", provider),
    vscode.commands.registerCommand("corsia.manage", () => vscode.env.openExternal(vscode.Uri.parse(daemon() + "/"))),
    vscode.commands.registerCommand("corsia.refresh", () => provider.refresh()),
    vscode.workspace.onDidChangeConfiguration((e) => { if (e.affectsConfiguration("corsia")) provider.refresh(); })
  );
}

function deactivate() {}

module.exports = { activate, deactivate };
