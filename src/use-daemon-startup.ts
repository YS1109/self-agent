import {useEffect,useState} from "react";
export interface StartupView {phase:"idle"|"starting"|"error";message:string}
export function startupEvent(value:unknown):StartupView|null {
  if(!value||typeof value!=="object"||Array.isArray(value))return null;
  const event=value as Record<string,unknown>;
  if(event.status==="connected")return {phase:"idle",message:""};
  if(!["starting","startup_error"].includes(String(event.status))||typeof event.message!=="string"||new TextEncoder().encode(event.message).length>1024)return null;
  return {phase:event.status==="starting"?"starting":"error",message:event.message};
}
export function useDaemonStartup() {
  const [state,setState]=useState<StartupView>({phase:"idle",message:""});
  useEffect(()=>{
    if(!("__TAURI_INTERNALS__" in window))return;
    let alive=true,revision=-1,stop:(()=>void)|undefined;
    const apply=(value:unknown)=>{
      const next=startupEvent(value), version=value&&typeof value==="object"?(value as Record<string,unknown>).revision:null;
      if(!alive||!next||!Number.isSafeInteger(version)||Number(version)<0||Number(version)<=revision)return;
      revision=Number(version);setState(previous=>previous.phase===next.phase&&previous.message===next.message?previous:next);
    };
    void Promise.all([import("@tauri-apps/api/event"),import("@tauri-apps/api/core")]).then(async([{listen},{invoke}])=>{
      const unsubscribe=await listen("daemon-status",event=>apply(event.payload));
      if(!alive){unsubscribe();return;}
      stop=unsubscribe;
      // Subscribe first; revisions fence a delayed snapshot behind newer events.
      apply(await invoke("daemon_startup_status"));
    }).catch(()=>{});
    return()=>{alive=false;stop?.();};
  },[]);
  return state;
}
