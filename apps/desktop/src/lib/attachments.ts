import type { InputAttachment } from "./rpc";

export const MAX_ATTACHMENT_FILES = 8;
export const MAX_ATTACHMENT_FILE_BYTES = 10 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES = 16 * 1024 * 1024;
const MAX_TEXT_BYTES = 512 * 1024;
const MAX_TOTAL_TEXT_BYTES = 2 * 1024 * 1024;

export async function readInputAttachments(files: File[]): Promise<InputAttachment[]> {
  if (files.length > MAX_ATTACHMENT_FILES) {
    throw new Error(`Choose at most ${MAX_ATTACHMENT_FILES} files.`);
  }
  const attachments: InputAttachment[] = [];
  let imageBytes = 0;
  let textBytes = 0;
  for (const file of files) {
    const fileName = file.name;
    if (isSensitiveAttachmentName(fileName)) {
      throw new Error(`${fileName} looks like a credential or shell profile and cannot be attached.`);
    }
    if (file.size > MAX_ATTACHMENT_FILE_BYTES) {
      throw new Error(`${fileName} exceeds the 10 MiB per-file limit.`);
    }
    const bytes = new Uint8Array(await file.arrayBuffer());
    const imageType = imageMediaType(file.type, fileName, bytes);
    if (imageType) {
      imageBytes += bytes.byteLength;
      if (imageBytes > MAX_TOTAL_IMAGE_BYTES) {
        throw new Error("Combined image attachments exceed the 16 MiB limit.");
      }
      attachments.push({ kind: "image", file_name: fileName, media_type: imageType, data: toBase64(bytes) });
      continue;
    }
    if (fileName.toLowerCase().endsWith(".pdf") || file.type === "application/pdf") {
      throw new Error("PDF attachments are not supported yet. Export the relevant pages as images or text.");
    }
    textBytes += bytes.byteLength;
    if (bytes.byteLength > MAX_TEXT_BYTES) {
      throw new Error(`${fileName} exceeds the 512 KiB per-file text limit.`);
    }
    if (textBytes > MAX_TOTAL_TEXT_BYTES) {
      throw new Error("Combined text attachments exceed the 2 MiB limit.");
    }
    let text: string;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch {
      throw new Error(`${fileName} is not a supported image or UTF-8 text file.`);
    }
    const mediaType = textMediaType(file.type, fileName);
    attachments.push({ kind: "text", file_name: fileName, media_type: mediaType, text });
  }
  validateAttachmentCollection(attachments);
  return attachments;
}

export function validateAttachmentCollection(attachments: InputAttachment[]): void {
  if (attachments.length > MAX_ATTACHMENT_FILES) {
    throw new Error(`Choose at most ${MAX_ATTACHMENT_FILES} files.`);
  }
  let imageBytes = 0;
  let encodedImageBytes = 0;
  let textBytes = 0;
  for (const attachment of attachments) {
    if (isSensitiveAttachmentName(attachment.file_name)) {
      throw new Error(`${attachment.file_name} looks like a credential or shell profile and cannot be attached.`);
    }
    if (attachment.kind === "image") {
      encodedImageBytes += attachment.data.length;
      imageBytes += base64DecodedLength(attachment.data);
    } else {
      const attachmentBytes = new TextEncoder().encode(attachment.text).byteLength;
      textBytes += attachmentBytes;
      if (attachmentBytes > MAX_TEXT_BYTES) {
        throw new Error(`${attachment.file_name} exceeds the 512 KiB per-file text limit.`);
      }
    }
  }
  if (encodedImageBytes > 22 * 1024 * 1024) throw new Error("Combined image attachments exceed the 22 MiB encoded-data limit.");
  if (imageBytes > MAX_TOTAL_IMAGE_BYTES) throw new Error("Combined image attachments exceed the 16 MiB limit.");
  if (textBytes > MAX_TOTAL_TEXT_BYTES) throw new Error("Combined text attachments exceed the 2 MiB limit.");
  for (const attachment of attachments) {
    if (attachment.kind !== "image") continue;
    const decodedBytes = base64DecodedLength(attachment.data);
    if (attachment.data.length === 0) {
      throw new Error(`${attachment.file_name} does not contain valid image data.`);
    }
    if (attachment.data.length > 14 * 1024 * 1024 || decodedBytes > MAX_ATTACHMENT_FILE_BYTES) {
      throw new Error(`${attachment.file_name} exceeds the 10 MiB per-file limit.`);
    }
    if (!["image/png", "image/jpeg", "image/gif", "image/webp"].includes(attachment.media_type)) {
      throw new Error(`${attachment.file_name} uses an unsupported image type.`);
    }
    if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(attachment.data)) {
      throw new Error(`${attachment.file_name} does not contain valid image data.`);
    }
  }
}

function base64DecodedLength(value: string): number {
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  return Math.max(0, Math.floor(value.length / 4) * 3 - padding);
}

export function isSensitiveAttachmentName(fileName: string): boolean {
  const name = fileName.trim().toLowerCase();
  return name === ".env"
    || name.startsWith(".env.")
    || [".npmrc", ".netrc", ".pypirc", ".bashrc", ".bash_profile", ".zshrc", ".profile", "profile.ps1", "powershell_profile.ps1"].includes(name)
    || /(^|[._-])(secret|secrets|credentials?)([._-]|$)/.test(name)
    || [".pem", ".key", ".p12", ".pfx"].some((extension) => name.endsWith(extension))
    || name === "id_rsa" || name.startsWith("id_rsa.")
    || name === "id_ed25519" || name.startsWith("id_ed25519.");
}

function imageMediaType(fileType: string, name: string, bytes: Uint8Array): string | null {
  const checks: [string, (value: Uint8Array) => boolean][] = [
    ["image/png", (value) => hasPrefix(value, [137, 80, 78, 71, 13, 10, 26, 10])],
    ["image/jpeg", (value) => hasPrefix(value, [255, 216, 255])],
    ["image/gif", (value) => hasPrefix(value, [71, 73, 70, 56, 55, 97]) || hasPrefix(value, [71, 73, 70, 56, 57, 97])],
    ["image/webp", (value) => value.length >= 12 && ascii(value, 0, 4) === "RIFF" && ascii(value, 8, 12) === "WEBP"],
  ];
  const expected = fileType.startsWith("image/") ? fileType : extensionImageType(name);
  const match = checks.find(([type, check]) => check(bytes) && (!expected || expected === type));
  if (match) return match[0];
  if (expected) throw new Error(`${name} does not contain a valid ${expected} image.`);
  return null;
}

function textMediaType(fileType: string, name: string): string {
  if (fileType === "application/json" || name.toLowerCase().endsWith(".json")) return "application/json";
  if (fileType === "application/xml" || name.toLowerCase().endsWith(".xml")) return "application/xml";
  return fileType.startsWith("text/") ? fileType : "text/plain";
}

function extensionImageType(name: string): string | null {
  const extension = name.toLowerCase().split(".").pop();
  if (extension === "png") return "image/png";
  if (extension === "jpg" || extension === "jpeg") return "image/jpeg";
  if (extension === "gif") return "image/gif";
  if (extension === "webp") return "image/webp";
  return null;
}

function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunkSize = 0x8000;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize));
  }
  return btoa(binary);
}

function hasPrefix(bytes: Uint8Array, prefix: number[]): boolean {
  return prefix.every((value, index) => bytes[index] === value);
}

function ascii(bytes: Uint8Array, start: number, end: number): string {
  return String.fromCharCode(...bytes.subarray(start, end));
}
