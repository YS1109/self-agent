/** Enforce the wire budget while consuming the stream, before JSON allocation. */
export async function readBoundedJson(response: Response, maximum: number, signal?: AbortSignal): Promise<unknown> {
  if (!response.body) throw new Error("后台响应为空或不可读取");
  const reader = response.body.getReader();
  try {
    const length = response.headers.get("content-length");
    if (length && /^\d+$/.test(length) && Number(length) > maximum) throw new Error("后台响应超过读取上限，未采用不完整数据");
    const chunks: Uint8Array[] = []; let size = 0;
    while (true) {
      const chunk = await reader.read(); signal?.throwIfAborted();
      if (chunk.done) break;
      size += chunk.value.byteLength;
      if (size > maximum) throw new Error("后台响应超过读取上限，未采用不完整数据");
      chunks.push(chunk.value);
    }
    const bytes = new Uint8Array(size); let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (reason) {
    if (signal?.aborted) throw new Error("已结束本次响应观察");
    if (reason instanceof Error && reason.message === "后台响应超过读取上限，未采用不完整数据") throw reason;
    throw new Error("后台响应格式无效或读取未确认，未回显响应正文");
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}
