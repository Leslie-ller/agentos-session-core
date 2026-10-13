import { useEffect } from "react"
import { onTransportReconnect, subscribe } from "@/lib/platform"
import {
  CONVERSATION_CHANGED_EVENT,
  type AgentType,
  type ConversationChange,
} from "@/lib/types"

/** A sidebar refresh alone cannot reconcile an open composer's agent binding. */
export function useConversationHandoffSync(
  conversationId: number | null,
  runtimeId: number,
  agent: AgentType,
  refetchDetail: (id: number, options?: { preserveLive?: boolean }) => void
) {
  useEffect(() => {
    if (conversationId == null) return
    const refresh = () => refetchDetail(runtimeId, { preserveLive: true })
    const offReconnect = onTransportReconnect(refresh)
    let disposed = false
    let unlisten: (() => void) | undefined
    void subscribe<ConversationChange>(CONVERSATION_CHANGED_EVENT, (change) => {
      if (
        change.kind === "upsert" &&
        change.summary.id === conversationId &&
        change.summary.agent_type !== agent
      )
        refresh()
    }).then((off) => {
      if (disposed) off()
      else unlisten = off
    })
    return () => {
      disposed = true
      offReconnect?.()
      unlisten?.()
    }
  }, [conversationId, runtimeId, agent, refetchDetail])
}
