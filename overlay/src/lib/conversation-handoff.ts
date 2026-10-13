import { acpSwitchAgent, getFolderConversation } from "./api"
import type { AgentType, DbConversationDetail } from "./types"

interface HandoffOptions {
  conversationId: number
  expectedAgent: AgentType
  targetAgent: AgentType
  apply: (agent: AgentType, title: string | null) => Promise<void>
  read?: (id: number) => Promise<DbConversationDetail>
  switchAgent?: typeof acpSwitchAgent
}

/** A lost HTTP acknowledgement is ambiguous: reconcile from the database,
 * never replay the switch (and never send a prompt as part of switching). */
export async function handoffConversation({
  conversationId,
  expectedAgent,
  targetAgent,
  apply,
  read = getFolderConversation,
  switchAgent = acpSwitchAgent,
}: HandoffOptions): Promise<void> {
  const before = await read(conversationId)
  if (before.summary.agent_type !== expectedAgent) {
    throw new Error(
      "Conversation changed in another client; reload before switching"
    )
  }
  try {
    await switchAgent(
      conversationId,
      expectedAgent,
      before.summary.external_id,
      targetAgent
    )
  } catch (error) {
    const after = await read(conversationId).catch(() => null)
    if (after?.summary.agent_type !== targetAgent) throw error
  }
  await apply(targetAgent, before.summary.title)
}

export function handoffSessionId(
  runtimeId: string | null,
  detail: DbConversationDetail | null,
  selectedAgent: AgentType
): string | undefined {
  return (
    runtimeId ??
    (detail?.summary.agent_type === selectedAgent
      ? (detail.summary.external_id ?? undefined)
      : undefined)
  )
}

export const HANDOFF_AGENTS = [
  { value: "codex", name: "Codex" },
  { value: "antigravity", name: "Antigravity" },
  { value: "custom:devin", name: "Devin" },
]

export function supportsAgentHandoff(agent: AgentType): boolean {
  return HANDOFF_AGENTS.some((option) => option.value === agent)
}

/** Resolve provenance from the archived segment, never from today's selector. */
export function replyAgent(
  turnId: string | undefined,
  currentAgent: AgentType,
  detail: Pick<DbConversationDetail, "handoff_revision" | "handoff_agents">
): AgentType | undefined {
  if (detail.handoff_revision == null) return currentAgent
  if (!turnId) return undefined
  if (turnId.startsWith("live-")) return currentAgent
  const segment = /^handoff-(\d+)-/.exec(turnId)
  const revision = segment ? Number(segment[1]) : 0
  return revision === detail.handoff_revision
    ? currentAgent
    : detail.handoff_agents?.[String(revision)]
}
