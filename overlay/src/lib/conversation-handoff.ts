import {
  acpCompactConversation,
  acpSwitchAgent,
  getFolderConversation,
} from "./api"
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

interface CompactOptions {
  conversationId: number
  expectedAgent: AgentType
  apply: () => Promise<void>
  read?: (id: number) => Promise<DbConversationDetail>
  compact?: typeof acpCompactConversation
}

/** Codeg's own compaction: the same agent continues in a fresh native
 * session. Like a switch, a lost acknowledgement is reconciled from the
 * database: the session moved off the one we compacted means it happened. */
export async function compactConversation({
  conversationId,
  expectedAgent,
  apply,
  read = getFolderConversation,
  compact = acpCompactConversation,
}: CompactOptions): Promise<void> {
  const before = await read(conversationId)
  const session = before.summary.external_id
  if (before.summary.agent_type !== expectedAgent || !session) {
    throw new Error(
      "Conversation changed in another client; reload before compacting"
    )
  }
  try {
    await compact(conversationId, expectedAgent, session)
  } catch (error) {
    const after = await read(conversationId).catch(() => null)
    if (
      after?.summary.agent_type !== expectedAgent ||
      after.summary.external_id === session
    )
      throw error
  }
  await apply()
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
  { value: "claude_code", name: "Claude Code" },
  { value: "antigravity", name: "Antigravity" },
  { value: "custom:devin", name: "Devin" },
  { value: "deepseek", name: "DeepSeek Harness" },
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
