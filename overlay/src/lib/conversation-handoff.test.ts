import { replyAgent } from "./conversation-handoff"
import { describe, expect, it, vi } from "vitest"
import {
  handoffConversation,
  handoffSessionId,
  supportsAgentHandoff,
} from "./conversation-handoff"
import type { AgentType, DbConversationDetail } from "./types"

vi.mock("./api", () => ({
  acpSwitchAgent: vi.fn(),
  getFolderConversation: vi.fn(),
}))

const detail = (agent: AgentType, sid: string | null) =>
  ({
    summary: { id: 7, agent_type: agent, external_id: sid, title: "Same chat" },
    turns: [],
  }) as unknown as DbConversationDetail

describe("conversation handoff", () => {
  it("changes the UI only after the persisted switch", async () => {
    const events: string[] = []
    const switchAgent = vi.fn(async () => {
      events.push("commit")
    })
    const apply = vi.fn(async () => {
      events.push("ui")
    })
    await handoffConversation({
      conversationId: 7,
      expectedAgent: "codex",
      targetAgent: "antigravity",
      read: vi.fn().mockResolvedValue(detail("codex", "a1")),
      switchAgent,
      apply,
    })
    expect(switchAgent).toHaveBeenCalledWith(7, "codex", "a1", "antigravity")
    expect(events).toEqual(["commit", "ui"])
    expect(apply).toHaveBeenCalledWith("antigravity", "Same chat")
  })

  it("leaves the old connection usable when the backend refuses", async () => {
    const apply = vi.fn()
    const switchAgent = vi.fn().mockRejectedValue(new Error("busy"))
    await expect(
      handoffConversation({
        conversationId: 7,
        expectedAgent: "codex",
        targetAgent: "antigravity",
        read: vi.fn().mockResolvedValue(detail("codex", "a1")),
        switchAgent,
        apply,
      })
    ).rejects.toThrow("busy")
    expect(apply).not.toHaveBeenCalled()
    expect(switchAgent).toHaveBeenCalledTimes(1)
  })

  it("recovers a committed switch after a lost acknowledgement without replay", async () => {
    const read = vi
      .fn()
      .mockResolvedValueOnce(detail("codex", "a1"))
      .mockResolvedValueOnce(detail("antigravity", null))
    const switchAgent = vi.fn().mockRejectedValue(new Error("network dropped"))
    const apply = vi.fn()
    await handoffConversation({
      conversationId: 7,
      expectedAgent: "codex",
      targetAgent: "antigravity",
      read,
      switchAgent,
      apply,
    })
    expect(apply).toHaveBeenCalledOnce()
    expect(switchAgent).toHaveBeenCalledOnce()
  })

  it("rejects a stale client's selection before making a mutation", async () => {
    const switchAgent = vi.fn()
    await expect(
      handoffConversation({
        conversationId: 7,
        expectedAgent: "codex",
        targetAgent: "antigravity",
        read: vi.fn().mockResolvedValue(detail("antigravity", "b1")),
        switchAgent,
        apply: vi.fn(),
      })
    ).rejects.toThrow("another client")
    expect(switchAgent).not.toHaveBeenCalled()
  })
})

it("never resumes a different agent's native session, and prefers the current runtime id", () => {
  expect(
    handoffSessionId(null, detail("codex", "a1"), "antigravity")
  ).toBeUndefined()
  expect(handoffSessionId("a2", detail("codex", "a1"), "codex")).toBe("a2")
  expect(handoffSessionId(null, detail("codex", "a1"), "codex")).toBe("a1")
  expect(handoffSessionId("a2", null, "codex")).toBe("a2")
})

it("offers Claude Code as a handoff agent", () => {
  expect(supportsAgentHandoff("claude_code")).toBe(true)
  expect(supportsAgentHandoff("gemini")).toBe(false)
})

describe("reply provenance", () => {
  const history = {
    handoff_revision: 4,
    handoff_agents: {
      "0": "codex",
      "1": "custom:devin",
      "2": "antigravity",
      "3": "codex",
    },
  } as const
  it("keeps each historical agent through repeated switches and reloads", () => {
    for (const [id, expected] of [
      ["turn-0", "codex"],
      ["handoff-1-turn-0", "custom:devin"],
      ["handoff-2-turn-0", "antigravity"],
      ["handoff-3-turn-0", "codex"],
      ["handoff-4-turn-0", "custom:devin"],
      ["live-2-message", "custom:devin"],
    ])
      expect(replyAgent(id, "custom:devin", history)).toBe(expected)
  })
  it("does not assign the current agent to unknown archived segments", () => {
    expect(
      replyAgent("handoff-1-turn-0", "codex", { handoff_revision: 3 })
    ).toBeUndefined()
    expect(replyAgent("turn-0", "codex", {})).toBe("codex")
  })
})
