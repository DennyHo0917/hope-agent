// @vitest-environment jsdom

import { act, cleanup, renderHook, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest"
import { useDesignChat } from "./useDesignChat"

const transportMock = vi.hoisted(() => ({
  call: vi.fn(),
  listen: vi.fn(() => vi.fn()),
}))

vi.mock("@/lib/transport-provider", () => ({ getTransport: () => transportMock }))
vi.mock("@/lib/logger", () => ({ logger: { error: vi.fn() } }))

describe("useDesignChat model routing", () => {
  beforeEach(() => {
    transportMock.call.mockReset()
    transportMock.listen.mockReset()
    transportMock.listen.mockImplementation(() => vi.fn())
    transportMock.call.mockImplementation((command: string) => {
      if (command === "list_agents") return Promise.resolve([])
      if (command === "get_available_models") {
        return Promise.resolve([{ providerId: "p", modelId: "chosen" }])
      }
      if (command === "get_chat_runtime_defaults") {
        return Promise.resolve({
          model: { providerId: "p", modelId: "chosen" },
          reasoningEffort: "medium",
        })
      }
      return Promise.resolve(undefined)
    })
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  test("pins a selection on an existing design session", async () => {
    const { result } = renderHook(() => useDesignChat(null, false))
    act(() => result.current.setCurrentSessionId("design-session"))

    await act(async () => {
      await result.current.handleModelChange("p::chosen")
    })

    expect(transportMock.call).toHaveBeenCalledWith("set_session_model", {
      sessionId: "design-session",
      providerId: "p",
      modelId: "chosen",
    })
    expect(result.current.activeModel).toEqual({ providerId: "p", modelId: "chosen" })
    expect(result.current.draftModelOverrideRef.current).toBeNull()
  })

  test("passes the project model into a new design session", async () => {
    const selected = { providerId: "p", modelId: "chosen" }
    const { result } = renderHook(() => useDesignChat(null, true, selected))

    await waitFor(() => expect(result.current.availableModels).toHaveLength(1))

    expect(result.current.activeModel).toEqual(selected)
    expect(result.current.draftModelOverrideRef.current).toEqual(selected)
  })
})
