import { act, renderHook } from "@testing-library/react"
import { beforeEach, expect, it, vi } from "vitest"
import type { ConversationChange } from "@/lib/types"
const h = vi.hoisted(() => ({
  reconnect: null as null | (() => void),
  changed: null as null | ((c: ConversationChange) => void),
  off: vi.fn(),
  offReconnect: vi.fn(),
}))
vi.mock("@/lib/platform", () => ({
  onTransportReconnect: (cb: () => void) => {
    h.reconnect = cb
    return h.offReconnect
  },
  subscribe: async (_: string, cb: (c: ConversationChange) => void) => {
    h.changed = cb
    return h.off
  },
}))
import { useConversationHandoffSync } from "./use-conversation-handoff-sync"
beforeEach(() => {
  h.reconnect = null
  h.changed = null
  vi.clearAllMocks()
})
it("refreshes open detail on reconnect even when it used to own the old agent", async () => {
  const refresh = vi.fn()
  const { unmount } = renderHook(() =>
    useConversationHandoffSync(7, -3, "codex", refresh)
  )
  await act(async () => {})
  act(() => h.reconnect?.())
  expect(refresh).toHaveBeenCalledWith(-3, { preserveLive: true })
  unmount()
  expect(h.off).toHaveBeenCalledOnce()
  expect(h.offReconnect).toHaveBeenCalledOnce()
})
it("refreshes only this conversation's changed agent, preserving current live content", async () => {
  const refresh = vi.fn()
  renderHook(() => useConversationHandoffSync(7, 7, "codex", refresh))
  await act(async () => {})
  const emit = (id: number, agent_type: string) =>
    h.changed?.({
      kind: "upsert",
      summary: { id, agent_type },
    } as ConversationChange)
  act(() => {
    emit(8, "antigravity")
    emit(7, "codex")
  })
  expect(refresh).not.toHaveBeenCalled()
  act(() => emit(7, "antigravity"))
  expect(refresh).toHaveBeenCalledOnce()
})
it("does not subscribe a draft with no persisted conversation", () => {
  renderHook(() => useConversationHandoffSync(null, -1, "codex", vi.fn()))
  expect(h.reconnect).toBeNull()
  expect(h.changed).toBeNull()
})
