import {describe,it,expect,vi} from "vitest";
import {parseDaemonCompatibility,fetchDaemonCompatibility} from "../scripts/daemon_compatibility";
const wire={handshake_schema:1,application:"self-agent",protocol_current:1,protocol_min:1,lifecycle:"running",capabilities:{bounded_history:1}};
const bytes=(v:unknown)=>new TextEncoder().encode(typeof v==="string"?v:JSON.stringify(v));
describe("development owner proxy compatibility",()=>{
  it("requires explicit bounded history and validates all fixed fields",()=>{
    expect(parseDaemonCompatibility(bytes(wire))).toEqual({protocol:1,bounded_history:1,lifecycle:"running"});
    for(const patch of [{capabilities:undefined},{capabilities:{}},{capabilities:{bounded_history:2}},{protocol_min:2,protocol_current:2},{protocol_min:0},{application:"other"},{lifecycle:"canary"},{unknown:true}])expect(()=>parseDaemonCompatibility(bytes({...wire,...patch}))).toThrow();
    for(const raw of ["",JSON.stringify(wire).replace('"protocol_min":1','"protocol_min":1,"protocol_min":1'),JSON.stringify(wire).replace('"bounded_history":1','"bounded_history":1.0'),JSON.stringify(wire).replace('"application"','"\\u0061pplication"')," ".repeat(513)])expect(()=>parseDaemonCompatibility(bytes(raw))).toThrow();
  });
  it("authenticates only the exact numeric loopback target and never echoes body",async()=>{
    const fetcher=vi.fn<typeof fetch>().mockResolvedValue(new Response(JSON.stringify(wire)));const signal=new AbortController().signal;
    await expect(fetchDaemonCompatibility(new URL("http://127.0.0.1:1234/"),"fixture-token",signal,fetcher)).resolves.toMatchObject({protocol:1});
    expect(fetcher).toHaveBeenCalledWith(new URL("http://127.0.0.1:1234/api/v1/compatibility"),{headers:{Authorization:"Bearer fixture-token"},redirect:"error",signal});
    for(const url of ["https://example.invalid/","http://localhost/","http://user@127.0.0.1/","http://127.0.0.1/path"]){await expect(fetchDaemonCompatibility(new URL(url),"private-canary",signal,fetcher)).rejects.not.toThrow("private-canary");}
    expect(fetcher).toHaveBeenCalledTimes(1);
    fetcher.mockRejectedValue(new Error("private-canary"));
    await expect(fetchDaemonCompatibility(new URL("http://127.0.0.1/"),"fixture-token",signal,fetcher)).rejects.not.toThrow("private-canary");
  });
  it("bounds streaming metadata without issuing a follow-on request",async()=>{
    let stopped=false;const fetcher=vi.fn<typeof fetch>().mockResolvedValue(new Response(new ReadableStream({start(c){c.enqueue(new Uint8Array(513));},cancel(){stopped=true;}})));
    await expect(fetchDaemonCompatibility(new URL("http://127.0.0.1/"),"fixture",new AbortController().signal,fetcher)).rejects.toThrow();expect(stopped).toBe(true);expect(fetcher).toHaveBeenCalledTimes(1);
  });
});
