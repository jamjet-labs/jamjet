import { describe, it, expect } from "vitest";
import { applyCompaction, CompactionResolver } from "../src/compaction";

const resolver = new CompactionResolver([{ toolPattern: "search.*", maxResultTokens: 5 }]);
const BIG = "x".repeat(400); // ~100 tokens at chars/4

function anthropicReq() {
  return {
    model: "claude-3-5-sonnet",
    messages: [
      { role: "assistant", content: [{ type: "tool_use", id: "tu_1", name: "search.web", input: {} }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_1", content: BIG }] },
      { role: "assistant", content: [{ type: "tool_use", id: "tu_2", name: "db.read", input: {} }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_2", content: BIG }] },
    ],
  };
}
function openaiReq() {
  return {
    model: "gpt-4o",
    messages: [
      { role: "assistant", tool_calls: [{ id: "c1", type: "function", function: { name: "search.kb", arguments: "{}" } }] },
      { role: "tool", tool_call_id: "c1", content: BIG },
    ],
  };
}

describe("applyCompaction", () => {
  it("truncates an oversized matching Anthropic tool_result and leaves non-matching untouched", () => {
    const { mutated, tokensSaved } = applyCompaction(anthropicReq(), resolver);
    const msgs = (mutated as any).messages;
    const r1 = msgs[1].content[0].content as string; // search.web → matched
    const r2 = msgs[3].content[0].content as string; // db.read → not matched
    expect(r1.length).toBeLessThan(BIG.length);
    expect(r1).toContain("truncated by JamJet");
    expect(r1).toContain("search.web");
    expect(r2).toBe(BIG); // untouched
    expect(tokensSaved).toBeGreaterThan(0);
  });

  it("truncates an oversized OpenAI role:tool message", () => {
    const { mutated, tokensSaved } = applyCompaction(openaiReq(), resolver);
    const content = (mutated as any).messages[1].content as string;
    expect(content.length).toBeLessThan(BIG.length);
    expect(content).toContain("truncated by JamJet");
    expect(tokensSaved).toBeGreaterThan(0);
  });

  it("leaves under-cap results untouched (no savings)", () => {
    const small = { model: "gpt-4o", messages: [
      { role: "assistant", tool_calls: [{ id: "c1", type: "function", function: { name: "search.kb", arguments: "{}" } }] },
      { role: "tool", tool_call_id: "c1", content: "short" },
    ] };
    const { mutated, tokensSaved } = applyCompaction(small, resolver);
    expect((mutated as any).messages[1].content).toBe("short");
    expect(tokensSaved).toBe(0);
  });

  it("is idempotent — second run does not re-truncate or double the marker", () => {
    const once = applyCompaction(anthropicReq(), resolver);
    const twice = applyCompaction(once.mutated as any, resolver);
    expect(twice.tokensSaved).toBe(0);
    const r = (twice.mutated as any).messages[1].content[0].content as string;
    expect((r.match(/truncated by JamJet/g) ?? []).length).toBe(1);
  });

  it("does not mutate the input object", () => {
    const req = anthropicReq();
    const before = JSON.stringify(req);
    applyCompaction(req, resolver);
    expect(JSON.stringify(req)).toBe(before);
  });

  it("truncates array-of-text-blocks tool_result content and leaves an under-cap sibling block untouched", () => {
    const small = { type: "tool_result", tool_use_id: "tu_1", content: [{ type: "text", text: "short" }] };
    const big = { type: "tool_result", tool_use_id: "tu_1", content: [{ type: "text", text: BIG }] };
    const req = {
      model: "claude-3-5-sonnet",
      messages: [
        { role: "assistant", content: [{ type: "tool_use", id: "tu_1", name: "search.web", input: {} }] },
        { role: "user", content: [small, big] },
      ],
    };
    const { mutated, tokensSaved } = applyCompaction(req, resolver);
    const blocks = (mutated as any).messages[1].content;
    // the under-cap block is returned by identity (no spurious clone)
    expect(blocks[0]).toBe(small);
    // the oversized array-content block is truncated in place
    const txt = blocks[1].content[0].text as string;
    expect(txt.length).toBeLessThan(BIG.length);
    expect(txt).toContain("truncated by JamJet");
    expect(tokensSaved).toBeGreaterThan(0);
  });
});

describe("CompactionResolver", () => {
  it("glob-matches tool names and gates with hasRules", () => {
    expect(new CompactionResolver([]).hasRules()).toBe(false);
    expect(resolver.hasRules()).toBe(true);
    expect(resolver.ruleFor("search.web")?.maxResultTokens).toBe(5);
    expect(resolver.ruleFor("db.read")).toBeNull();
  });
});
