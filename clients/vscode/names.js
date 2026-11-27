// Readable model names. Model files carry their whole history in the name
// ("huihui-ai_Huihui-gpt-oss-20b-BF16-abliterated-Q4_K_M.gguf"); people need
// the model and its size ("gpt-oss 20B"). The same rules as Zazu's
// (zazu-vscode/src/corsia.ts), so both pickers agree.
"use strict";

// Who published the file, not what the model is.
const UPLOADERS = /^(huihui-ai|huihui|unsloth|bartowski|lmstudio-community|mradermacher|thebloke|second-state|ggml-org|qwen|google|openai)[_-](?=.)/i;
// Words that describe packaging or training detail, not the model.
const NOISE = new Set(["instruct", "it", "chat", "qat", "unquantized", "i1", "ud", "gguf", "imatrix", "cpu", "hf", "base", "abliterated", "uncensored", "k", "m", "s", "l", "xl", "xs", "xxs"]);
const QUANT = /(?:^|[-_.])((?:I?Q\d(?:_[A-Z0-9]+)*)|BF16|F16|F32|FP16|FP8|MXFP4)(?=$|[-_.])/gi;

function shortQuant(q) {
  if (!q) return "";
  const u = q.toUpperCase();
  if (/^(BF16|F16|FP16)$/.test(u)) return "16-bit";
  if (u === "F32") return "32-bit";
  if (u === "FP8") return "8-bit";
  if (u === "MXFP4") return "MXFP4";
  const m = /^(I?Q\d)/.exec(u);
  return m ? m[1] : u;
}

function modelNameParts(file) {
  let s = (file || "").replace(/\.gguf$/i, "").replace(/-cpu$/i, "");
  const uncensored = /abliterated|uncensored/i.test(s);
  for (let i = 0; i < 2; i++) s = s.replace(UPLOADERS, "");
  let quant = "";
  for (const m of s.matchAll(QUANT)) quant = m[1];
  s = s.replace(QUANT, "-");
  const tokens = s.split(/[-_]|\.(?=[A-Za-z])/).filter(Boolean);
  const words = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i];
    const lower = t.toLowerCase();
    // Packaging words, and active-parameter counts ("A3B").
    if (NOISE.has(lower) || /^A\d+(\.\d+)?B$/i.test(t)) continue;
    if (lower === "gpt" && (tokens[i + 1] || "").toLowerCase() === "oss") {
      words.push("gpt-oss");
      i++;
    } else if (/^\d+(\.\d+)?[bm]$/i.test(t)) words.push(t.slice(0, -1) + t.slice(-1).toUpperCase());
    else if (/^[a-z]/.test(t) && !/\d/.test(t.slice(1, 2))) words.push(t[0].toUpperCase() + t.slice(1));
    else words.push(t);
  }
  return { base: words.join(" ") || (file || "").replace(/\.gguf$/i, ""), quant: shortQuant(quant), uncensored };
}

function friendlyModelName(file) {
  return modelNameParts(file).base;
}

// The plain name where it is unique, and just enough extra (how it is
// stored, then "uncensored") where two models would otherwise read the same.
function friendlyModelNames(files) {
  const parts = new Map(files.map((f) => [f, modelNameParts(f)]));
  const out = new Map();
  const groups = new Map();
  for (const [f, p] of parts) groups.set(p.base, [...(groups.get(p.base) || []), f]);
  for (const [base, group] of groups) {
    if (group.length === 1) {
      out.set(group[0], base);
      continue;
    }
    const byQuant = new Map();
    for (const f of group) byQuant.set(parts.get(f).quant, (byQuant.get(parts.get(f).quant) || 0) + 1);
    for (const f of group) {
      const p = parts.get(f);
      const extra = [p.quant];
      if (byQuant.get(p.quant) > 1 && p.uncensored) extra.push("uncensored");
      out.set(f, [base, extra.filter(Boolean).join(" ")].filter(Boolean).join(" · "));
    }
  }
  return out;
}

module.exports = { friendlyModelName, friendlyModelNames, modelNameParts };
