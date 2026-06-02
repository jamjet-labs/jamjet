// Pure, universal-safe (no node:*). Mirrors cache-inject.ts: shallow-clone,
// preserve unknown fields, idempotent. Token heuristic: tokens ~= chars / 4
// (no tokenizer in the universal bundle).

const MARKER = "truncated by JamJet";

export interface CompactionRule {
  toolPattern: string;
  maxResultTokens: number;
}

function globToRegExp(glob: string): RegExp {
  const escaped = glob.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
  return new RegExp(`^${escaped}$`);
}

export class CompactionResolver {
  private readonly rules: Array<{ re: RegExp; rule: CompactionRule }>;
  constructor(rules: CompactionRule[]) {
    this.rules = (rules ?? []).map((r) => ({ re: globToRegExp(r.toolPattern), rule: r }));
  }
  hasRules(): boolean {
    return this.rules.length > 0;
  }
  ruleFor(toolName: string): CompactionRule | null {
    for (const { re, rule } of this.rules) if (re.test(toolName)) return rule;
    return null;
  }
}

interface AnyMsg {
  role?: string;
  content?: unknown;
  tool_calls?: Array<{ id?: string; function?: { name?: string } }>;
  tool_call_id?: string;
}

/** chars kept for a given token cap */
function capChars(maxTokens: number): number {
  return Math.max(0, Math.floor(maxTokens * 4));
}

/** Truncate a string to `keep` chars + a marker naming the tool; return [newText, tokensSaved]. */
function truncateText(text: string, keep: number, tool: string): [string, number] {
  if (text.includes(MARKER)) return [text, 0]; // idempotent
  if (text.length <= keep) return [text, 0];
  const removed = text.length - keep;
  const out = `${text.slice(0, keep)}\n…[+~${Math.floor(removed / 4)} tokens ${MARKER} — call ${tool} for the full result]`;
  return [out, Math.floor(removed / 4)];
}

export function applyCompaction(
  arg0: Record<string, unknown>,
  resolver: CompactionResolver,
): { mutated: Record<string, unknown>; tokensSaved: number } {
  const messages = Array.isArray(arg0["messages"]) ? (arg0["messages"] as AnyMsg[]) : null;
  if (!messages || !resolver.hasRules()) return { mutated: arg0, tokensSaved: 0 };

  // 1) map tool_use_id / tool_call_id -> tool name
  const idToName = new Map<string, string>();
  for (const m of messages) {
    if (Array.isArray(m.content)) {
      for (const block of m.content as Array<Record<string, unknown>>) {
        if (block && block["type"] === "tool_use" && typeof block["id"] === "string" && typeof block["name"] === "string") {
          idToName.set(block["id"] as string, block["name"] as string);
        }
      }
    }
    if (Array.isArray(m.tool_calls)) {
      for (const tc of m.tool_calls) {
        if (tc?.id && tc.function?.name) idToName.set(tc.id, tc.function.name);
      }
    }
  }

  let tokensSaved = 0;
  let changed = false;

  const newMessages = messages.map((m): AnyMsg => {
    // OpenAI: role:"tool" with string content
    if (m.role === "tool" && typeof m.tool_call_id === "string" && typeof m.content === "string") {
      const name = idToName.get(m.tool_call_id);
      const rule = name ? resolver.ruleFor(name) : null;
      if (rule) {
        const [txt, saved] = truncateText(m.content, capChars(rule.maxResultTokens), name!);
        if (saved > 0) { tokensSaved += saved; changed = true; return { ...m, content: txt }; }
      }
      return m;
    }
    // Anthropic: user message whose content[] has tool_result blocks
    if (Array.isArray(m.content)) {
      let blockChanged = false;
      const newContent = (m.content as Array<Record<string, unknown>>).map((block) => {
        if (block && block["type"] === "tool_result" && typeof block["tool_use_id"] === "string") {
          const name = idToName.get(block["tool_use_id"] as string);
          const rule = name ? resolver.ruleFor(name) : null;
          if (!rule) return block;
          const cap = capChars(rule.maxResultTokens);
          const c = block["content"];
          if (typeof c === "string") {
            const [txt, saved] = truncateText(c, cap, name!);
            if (saved > 0) { tokensSaved += saved; blockChanged = true; return { ...block, content: txt }; }
          } else if (Array.isArray(c)) {
            // content is an array of {type:'text', text} blocks
            const nb = (c as Array<Record<string, unknown>>).map((cb) => {
              if (cb && cb["type"] === "text" && typeof cb["text"] === "string") {
                const [txt, saved] = truncateText(cb["text"] as string, cap, name!);
                if (saved > 0) { tokensSaved += saved; blockChanged = true; return { ...cb, text: txt }; }
              }
              return cb;
            });
            if (blockChanged) return { ...block, content: nb };
          }
        }
        return block;
      });
      if (blockChanged) { changed = true; return { ...m, content: newContent }; }
    }
    return m;
  });

  if (!changed) return { mutated: arg0, tokensSaved: 0 };
  return { mutated: { ...arg0, messages: newMessages }, tokensSaved };
}
