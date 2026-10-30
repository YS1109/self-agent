/** Server-side development proxy helper. Owner bearer must never enter browser code. */
export interface DaemonCompatibility { protocol:1; bounded_history:1; lifecycle:"running"|"draining"|"stopped" }
const failure=()=>new Error("后台协议元数据缺失或不兼容；未继续代理请求。");
export function parseDaemonCompatibility(raw:Uint8Array):DaemonCompatibility {
  if(raw.byteLength===0||raw.byteLength>512)throw failure();
  let text:string,value:unknown;
  try {text=new TextDecoder("utf-8",{fatal:true}).decode(raw);value=JSON.parse(text);}catch{throw failure();}
  const object=(v:unknown):v is Record<string,unknown>=>!!v&&typeof v==="object"&&!Array.isArray(v);
  const exact=(v:Record<string,unknown>,keys:string[])=>Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
  const fields=["handshake_schema","application","protocol_current","protocol_min","lifecycle","capabilities"];
  // The fixed wire uses ASCII keys/enums and integer values only. Reject escaped
  // aliases and repeated field tokens before JSON.parse can erase duplicates.
  if(text.includes("\\")||!object(value)||!exact(value,fields))throw failure();
  for(const key of [...fields,"bounded_history"])if([...text.matchAll(new RegExp(`"${key}"\\s*:`,"g"))].length!==1)throw failure();
  for(const key of ["handshake_schema","protocol_current","protocol_min","bounded_history"])if(!new RegExp(`"${key}"\\s*:\\s*[1-9][0-9]*(?=\\s*[,}])`).test(text))throw failure();
  if(value.handshake_schema!==1||value.application!=="self-agent"||!Number.isInteger(value.protocol_current)||!Number.isInteger(value.protocol_min)||Number(value.protocol_current)>65535||Number(value.protocol_min)<1||Number(value.protocol_current)<Number(value.protocol_min)||!object(value.capabilities)||!exact(value.capabilities,["bounded_history"])||value.capabilities.bounded_history!==1||!["running","draining","stopped"].includes(String(value.lifecycle)))throw failure();
  if(Number(value.protocol_min)>1||Number(value.protocol_current)<1)throw failure();
  return {protocol:1,bounded_history:1,lifecycle:value.lifecycle as DaemonCompatibility["lifecycle"]};
}
export async function fetchDaemonCompatibility(base:URL,token:string,signal:AbortSignal,fetcher:typeof fetch=fetch):Promise<DaemonCompatibility> {
  if(base.protocol!=="http:"||base.hostname!=="127.0.0.1"||base.username||base.password||base.pathname!=="/"||base.search||base.hash)throw failure();
  let response:Response;
  try {response=await fetcher(new URL("/api/v1/compatibility",base),{headers:{Authorization:`Bearer ${token}`},redirect:"error",signal});}catch{throw failure();}
  if(!response.ok||!response.body||Number(response.headers.get("content-length"))>512){await response.body?.cancel().catch(()=>{});throw failure();}
  const reader=response.body.getReader();const chunks:Uint8Array[]=[];let size=0;
  try {
    while(true){const {done,value}=await reader.read();if(done)break;size+=value.byteLength;if(size>512)throw failure();chunks.push(value);}
    const bytes=new Uint8Array(size);let offset=0;for(const chunk of chunks){bytes.set(chunk,offset);offset+=chunk.byteLength;}return parseDaemonCompatibility(bytes);
  }catch{throw failure();}finally{await reader.cancel().catch(()=>{});reader.releaseLock();}
}
