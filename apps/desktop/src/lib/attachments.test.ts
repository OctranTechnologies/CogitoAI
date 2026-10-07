import { describe, expect, it } from "vitest";
import { readInputAttachments, validateAttachmentCollection } from "./attachments";

describe("task attachments", () => {
  it("normalizes supported images and UTF-8 text", async () => {
    const png = new File([new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 0])], "screen.png", { type: "image/png" });
    const note = new File(["API error: 404"], "notes.txt", { type: "text/plain" });
    const attachments = await readInputAttachments([png, note]);
    expect(attachments).toEqual([
      { kind: "image", file_name: "screen.png", media_type: "image/png", data: "iVBORw0KGgoA" },
      { kind: "text", file_name: "notes.txt", media_type: "text/plain", text: "API error: 404" },
    ]);
  });

  it("rejects credential-looking files and PDFs clearly", async () => {
    await expect(readInputAttachments([new File(["key"], ".env.local")])).rejects.toThrow(/credential/);
    await expect(readInputAttachments([new File(["%PDF"], "guide.pdf", { type: "application/pdf" })])).rejects.toThrow(/PDF attachments are not supported/);
  });

  it("rejects malformed images and files above the bounded size", async () => {
    await expect(readInputAttachments([new File(["not an image"], "broken.png", { type: "image/png" })])).rejects.toThrow(/valid image/);
    const large = new File([new Uint8Array(10 * 1024 * 1024 + 1)], "large.txt", { type: "text/plain" });
    await expect(readInputAttachments([large])).rejects.toThrow(/10 MiB/);
  });

  it("bounds combined attachments and enforces the file count", () => {
    const image = { kind: "image" as const, file_name: "screen.png", media_type: "image/png", data: "a".repeat(23 * 1024 * 1024) };
    expect(() => validateAttachmentCollection([image])).toThrow(/22 MiB/);
    const tooMuchRawImageData = {
      ...image,
      data: "A".repeat(Math.ceil((16 * 1024 * 1024 + 1) / 3) * 4),
    };
    expect(() => validateAttachmentCollection([tooMuchRawImageData])).toThrow(/16 MiB/);
    expect(() => validateAttachmentCollection(Array.from({ length: 9 }, (_, index) => ({
      kind: "text" as const,
      file_name: `file-${index}.txt`,
      media_type: "text/plain",
      text: "ok",
    })))).toThrow(/at most 8/);
  });

  it("rejects malformed base64 and unsupported image types", () => {
    expect(() => validateAttachmentCollection([{
      kind: "image",
      file_name: "screen.png",
      media_type: "image/png",
      data: "not-base64!",
    }])).toThrow(/valid image data/);
    expect(() => validateAttachmentCollection([{
      kind: "image",
      file_name: "screen.svg",
      media_type: "image/svg+xml",
      data: "iVBORw0KGgoA",
    }])).toThrow(/unsupported image type/);
  });
});
