// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, expect, test, vi } from "vitest"
import type { SessionSearchResult } from "@/types/chat"
import ChatSidebar from "./ChatSidebar"

const mocks = vi.hoisted(() => ({
  call: vi.fn(),
  reload: vi.fn(async () => {}),
  t: vi.fn((key: string) => key),
}))

vi.mock("@/lib/transport-provider", () => ({
  getTransport: () => ({ call: mocks.call }),
}))
vi.mock("@/lib/logger", () => ({ logger: { error: vi.fn(), warn: vi.fn() } }))
vi.mock("sonner", () => ({ toast: { success: vi.fn(), warning: vi.fn(), error: vi.fn() } }))
vi.mock("react-i18next", async (importOriginal) => ({
  ...(await importOriginal<typeof import("react-i18next")>()),
  useTranslation: () => ({ t: mocks.t }),
}))
vi.mock("@/components/ui/tooltip", () => ({
  IconTip: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}))
vi.mock("@/components/ui/floating-menu", () => ({ FloatingMenu: () => null }))
vi.mock("@/components/ui/resize-handle-glow", () => ({ ResizeHandleGlow: () => null }))
vi.mock("./AgentSection", () => ({ default: () => null }))
vi.mock("./PinnedSection", () => ({ default: () => null }))
vi.mock("../project/ProjectSection", () => ({ default: () => null }))
vi.mock("./useSidebarSessionPagination", () => ({
  useSidebarSessionPagination: () => ({
    sessionsByFilter: { session: [], subagent: [] },
    pinnedSessions: [],
    loading: false,
    loadingMoreByFilter: { session: false, subagent: false },
    hasMoreByFilter: { session: false, subagent: false },
    loadMore: vi.fn(),
    reload: mocks.reload,
  }),
}))
vi.mock("./SessionList", () => ({
  default: ({ searchResults }: { searchResults: SessionSearchResult[] | null }) => (
    <div data-testid="search-hits">
      {searchResults?.map((hit) => hit.messageId).join(",") ?? "none"}
    </div>
  ),
}))

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

test("refreshes active search hits after Codex re-import replaces message IDs", async () => {
  const hit = (messageId: number): SessionSearchResult => ({
    messageId,
    sessionId: "imported-session",
    sessionTitle: "Imported",
    agentId: "ha-main",
    messageRole: "user",
    contentSnippet: "matching text",
    timestamp: "2026-09-24T00:00:00Z",
    relevanceRank: 0,
    isCron: false,
    parentSessionId: null,
    projectId: null,
    channelType: null,
    channelChatType: null,
    matchKind: "message",
  })
  let currentHits = [hit(10)]
  mocks.call.mockImplementation(async (command: string) => {
    if (command === "get_sidebar_display_mode") return "compact"
    if (command === "search_sessions_cmd") return currentHits
    if (command === "import_local_codex_sessions_cmd") {
      currentHits = [hit(42)]
      return { scanned: 5, created: 0, updated: 1, unchanged: 2, skipped: 1, failed: 1 }
    }
    return null
  })

  render(
    <ChatSidebar
      sessions={[]}
      agents={[]}
      currentSessionId={null}
      readableSessionId={null}
      loadingSessionIds={new Set()}
      totalUnreadCount={0}
      panelWidth={280}
      sidebarCollapsed={false}
      onPanelWidthChange={vi.fn()}
      onSidebarCollapsedChange={vi.fn()}
      onSwitchSession={vi.fn()}
      onNewChat={vi.fn()}
      onArchiveSession={vi.fn()}
    />,
  )
  fireEvent.change(screen.getByRole("searchbox", { name: "chat.searchPlaceholder" }), {
    target: { value: "matching" },
  })
  await waitFor(() => expect(screen.getByTestId("search-hits").textContent).toBe("10"))

  fireEvent.click(screen.getByRole("button", { name: "chat.codexImportAction" }))
  await waitFor(() => expect(screen.getByTestId("search-hits").textContent).toBe("42"))
  expect(mocks.t).toHaveBeenCalledWith("chat.codexImportSummary", {
    created: 0,
    updated: 1,
    skipped: 4,
  })
  expect(mocks.call.mock.calls.filter(([command]) => command === "search_sessions_cmd")).toHaveLength(2)
})
