// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import type { ContentBlock, FileChangeMetadata, Message } from "@/types/chat"
import { TooltipProvider } from "@/components/ui/tooltip"
import { extractMessageFileAttachments, type MessageFileAttachment } from "../chatUtils"
import { DiffPanel } from "../diff-panel/DiffPanel"
import { useDiffPanel } from "../diff-panel/useDiffPanel"
import MessageBubble from "./MessageBubble"

const run = vi.hoisted(() => vi.fn(async () => "executed"))
vi.mock("@/components/chat/files/useFileResource", () => ({
  useFileResource: () => ({ kind: "code", primary: "preview", menu: ["preview"], run }),
}))
vi.mock("react-i18next", () => ({
  initReactI18next: { type: "3rdParty", init: () => {} },
  useTranslation: () => ({ t: (key: string) => key }),
}))
vi.mock("./MessageContent", () => ({
  AssistantContentBlocks: ({ msg }: { msg: Message }) => <div>{msg.content}</div>,
}))
vi.mock("./MessageUrlPreviews", () => ({ default: () => null }))
vi.mock("@/components/common/ProviderIcon", () => ({ default: () => null }))

const created: FileChangeMetadata = {
  kind: "file_change",
  path: "/repo/demo-tour/ignored.html",
  action: "create",
  before: null,
  after: "<h1>saved snapshot</h1>\n",
  linesAdded: 1,
  linesRemoved: 0,
  language: "html",
  truncated: false,
}

function block(metadata: FileChangeMetadata): ContentBlock {
  return { type: "tool_call", tool: { callId: "c1", name: "write", arguments: "{}", metadata } }
}

function Harness({
  blocks = [],
  footerFiles,
  displayMode = "bubble",
}: {
  blocks?: ContentBlock[]
  footerFiles?: MessageFileAttachment[]
  displayMode?: "bubble" | "timeline"
}) {
  const diff = useDiffPanel()
  return (
    <TooltipProvider>
      <MessageBubble
        msg={{ role: "assistant", content: "answer", contentBlocks: blocks }}
        index={0}
        isLast={false}
        loading={false}
        agents={[]}
        onHover={() => {}}
        onContextMenu={() => {}}
        isHovered={false}
        isCopied={false}
        onCopy={() => {}}
        sessionId="s1"
        onOpenDiff={diff.openDiff}
        footerFiles={footerFiles}
        displayMode={displayMode}
      />
      {diff.showPanel && (
        <DiffPanel
          changes={diff.activeChanges}
          activeIndex={diff.activeIndex}
          openNonce={diff.openNonce}
          onActiveIndexChange={diff.setActiveIndex}
          onClose={diff.closeDiff}
        />
      )}
    </TooltipProvider>
  )
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.stubGlobal("localStorage", { getItem: () => null, setItem: () => {} })
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  )
  Object.defineProperty(Element.prototype, "scrollTo", { configurable: true, value: () => {} })
  Object.defineProperty(Element.prototype, "scrollIntoView", {
    configurable: true,
    value: () => {},
  })
})
afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe("modified file footer navigation", () => {
  it.each(["bubble", "timeline"] as const)(
    "opens an all-addition saved create diff in %s mode",
    (displayMode) => {
      const { container } = render(<Harness blocks={[block(created)]} displayMode={displayMode} />)
      fireEvent.click(screen.getByRole("button", { name: "ignored.html" }))
      expect(screen.getByText("<h1>saved snapshot</h1>")).toBeTruthy()
      const rows = container.querySelectorAll("[data-diff-row]")
      expect(rows.length).toBe(1)
      expect(rows[0].textContent).toContain("+")
      expect(run).not.toHaveBeenCalled()
    },
  )

  it("keeps current-file preview in the existing file menu", async () => {
    render(<Harness blocks={[block(created)]} />)
    fireEvent.contextMenu(screen.getByRole("button", { name: "ignored.html" }))
    fireEvent.click(await screen.findByRole("menuitem", { name: "fileActions.preview" }))
    expect(run).toHaveBeenCalledWith("preview")
    expect(screen.queryByText("<h1>saved snapshot</h1>")).toBeNull()
  })

  it("opens the selected file_changes snapshot, including truncated-content warning", () => {
    const edit = {
      ...created,
      path: "/repo/second.ts",
      action: "edit" as const,
      before: "old line\n",
      after: "new line\n",
      truncated: true,
    }
    render(
      <Harness
        blocks={[
          {
            type: "tool_call",
            tool: {
              callId: "patch",
              name: "apply_patch",
              arguments: "{}",
              metadata: { kind: "file_changes", changes: [created, edit] },
            },
          },
        ]}
      />,
    )
    fireEvent.click(screen.getByRole("button", { name: "second.ts" }))
    const text = [...document.querySelectorAll("[data-diff-row]")]
      .map((row) => row.textContent)
      .join("\n")
    expect(text).toContain("old line")
    expect(text).toContain("new line")
    expect(screen.getByText("diffPanel.fileTooLarge")).toBeTruthy()
    expect(screen.queryByText("<h1>saved snapshot</h1>")).toBeNull()
  })

  it("shows no diff data when a successful write has lost its metadata", () => {
    render(
      <Harness
        blocks={[
          {
            type: "tool_call",
            tool: {
              callId: "lost",
              name: "write",
              arguments: JSON.stringify({ path: created.path }),
              result: "Successfully wrote file",
            },
          },
        ]}
      />,
    )
    fireEvent.click(screen.getByRole("button", { name: "ignored.html" }))
    expect(screen.getByText("diffPanel.noDiffData")).toBeTruthy()
    expect(run).not.toHaveBeenCalled()
  })

  it("shows no diff data instead of inventing an all-addition edit from a missing before side", () => {
    render(<Harness blocks={[block({ ...created, action: "edit" })]} />)
    fireEvent.click(screen.getByRole("button", { name: "ignored.html" }))
    expect(screen.getByText("diffPanel.noDiffData")).toBeTruthy()
    expect(run).not.toHaveBeenCalled()
  })

  it("opens hoisted snapshots but prefers the final message's later write", () => {
    render(
      <Harness
        footerFiles={extractMessageFileAttachments([block(created)])}
        blocks={[block({ ...created, after: "final snapshot\n" })]}
      />,
    )
    fireEvent.click(screen.getByRole("button", { name: "ignored.html" }))
    expect(screen.getByText("final snapshot")).toBeTruthy()
    expect(screen.queryByText("<h1>saved snapshot</h1>")).toBeNull()
  })

  it("keeps media and generic output URL primary previews", () => {
    const { rerender } = render(
      <Harness
        footerFiles={[
          {
            kind: "media",
            item: {
              kind: "image",
              name: "image.png",
              mimeType: "image/png",
              sizeBytes: 1,
              url: "https://example.com/image.png",
            },
          },
        ]}
      />,
    )
    fireEvent.click(screen.getByRole("button", { name: "image.png" }))
    expect(run).toHaveBeenCalledWith("preview")
    run.mockClear()
    rerender(<Harness footerFiles={[{ kind: "path", path: "https://example.com/output.html" }]} />)
    fireEvent.click(screen.getByRole("button", { name: "output.html" }))
    expect(run).toHaveBeenCalledWith("preview")
  })
})
