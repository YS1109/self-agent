import {defineConfig} from "vite";
import react from "@vitejs/plugin-react";
import {isIP} from "node:net";

// Optional fixture development proxy only. Unlike the desktop development
// proxy this never reads daemon.json or injects local-owner credentials.
function fixtureTarget():string|undefined {
 const raw=process.env.HARNESS_GATEWAY_DEV_URL;if(!raw)return;
 const url=new URL(raw),host=url.hostname.replace(/^\[|\]$/g,"");
 if(url.protocol!=="http:"||!isIP(host)||!(host==="::1"||host.startsWith("127."))||url.username||url.password||url.pathname!=="/"||url.search||url.hash)throw new Error("H5 开发代理仅允许显式数字 loopback HTTP origin");
 return url.origin;
}
const target=fixtureTarget();
export default defineConfig({plugins:[react()],build:{outDir:"dist-mobile",rollupOptions:{input:"mobile.html"}},server:{host:"127.0.0.1",port:1421,strictPort:true,proxy:target?{"/gateway":{target,ws:true,changeOrigin:false}}:undefined},envPrefix:["VITE_"]});
