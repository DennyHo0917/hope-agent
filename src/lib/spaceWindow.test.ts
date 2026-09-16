import { beforeEach, expect, it, vi } from "vitest"

const mocks = vi.hoisted(() => ({ isTauriMode: vi.fn(), emit: vi.fn() }))
vi.mock("@/lib/transport", () => ({ isTauriMode: mocks.isTauriMode }))
vi.mock("@tauri-apps/api/event", () => ({ emit: mocks.emit }))

import { focusMainWindow } from "./spaceWindow"
import { emitAskAi } from "./manual/askAi"

beforeEach(() => {
  vi.clearAllMocks()
  mocks.emit.mockResolvedValue(undefined)
})

it("reopens through the desktop shell so pending fullscreen hides are cancelled", async () => {
  mocks.isTauriMode.mockReturnValue(true)
  await focusMainWindow()
  expect(mocks.emit).toHaveBeenCalledExactlyOnceWith("main-window:show")
})

it("does not request a native window in browser mode", async () => {
  mocks.isTauriMode.mockReturnValue(false)
  await focusMainWindow()
  expect(mocks.emit).not.toHaveBeenCalled()
})

it("routes help-window Ask AI through the same pending-hide cancellation", async () => {
  mocks.isTauriMode.mockReturnValue(true)
  expect(await emitAskAi({ text: "manual excerpt" })).toBe("desktop")
  expect(mocks.emit.mock.calls).toEqual([
    ["help:ask-ai", { text: "manual excerpt" }],
    ["main-window:show"],
  ])
})
