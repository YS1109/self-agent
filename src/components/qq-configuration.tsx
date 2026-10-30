import {useState} from "react";
import {Button} from "./ui/button";
import {qqCommandError} from "../qq-client";
import type {QqCommand,QqConfiguration} from "../qq-client";

/** Remount on config revision changes: a stale form never adopts a new CAS. */
export function QqConfigurationForm({configuration,disabled,onSubmit}:{configuration:QqConfiguration|null;disabled:boolean;onSubmit:(command:QqCommand)=>Promise<unknown>}) {
 const [appId,setAppId]=useState(configuration?.app_id??"");
 const [enabled,setEnabled]=useState(configuration?.enabled??false);
 const [windowMs,setWindowMs]=useState(configuration?.reply_policy.window_ms??300000);
 const [userQuota,setUserQuota]=useState(configuration?.reply_policy.max_user??4);
 const [groupQuota,setGroupQuota]=useState(configuration?.reply_policy.max_group??5);
 const [confirmed,setConfirmed]=useState(false);
 const revokes=!!configuration&&(configuration.app_id!==appId||configuration.enabled&&!enabled);
 const command:QqCommand={type:"configure",config_id:configuration?.id??null,expected_revision:configuration?.revision??null,app_id:appId,enabled,reply_policy:{window_ms:windowMs,max_user:userQuota,max_group:groupQuota}};
 const error=qqCommandError(command);
 return <fieldset className="qq-configuration" disabled={disabled}><legend>本机连接配置</legend>
  <label>机器人 AppID（不是 ClientSecret）<input value={appId} maxLength={128} autoComplete="off" spellCheck={false} disabled={configuration?.credential_state==="configured"} onChange={e=>{setAppId(e.target.value);setConfirmed(false);}}/></label>
  <p className="help-text">官方 token 与消息地址固定为 api.bot.qq.com，不能填写自定义服务器或把密钥放在 AppID。更换应用前先显式解除旧凭据绑定。</p>
  <label className="qq-check"><input type="checkbox" checked={enabled} disabled={!configuration||(configuration.credential_state!=="configured"&&configuration.mode!=="fixture")} onChange={e=>{setEnabled(e.target.checked);setConfirmed(false);}}/>启用官方 QQ 出站连接</label>
  {!configuration&&<p className="help-text">先停用创建配置，再通过专用输入登记本机系统凭据，最后明确启用。</p>}
  <p className="help-text">凭据：{configuration?.credential_state==="configured"?"存在独立绑定，尚不代表系统存储可读或平台鉴权成功":"尚无独立绑定"}。配置保存不代表消息送达。</p>
  <div className="qq-config-limits"><label>被动窗口（毫秒）<input type="number" min={1} max={300000} step={1} value={windowMs} onChange={e=>setWindowMs(Number(e.target.value))}/></label><label>单聊回复次数<input type="number" min={1} max={4} step={1} value={userQuota} onChange={e=>setUserQuota(Number(e.target.value))}/></label><label>群聊回复次数<input type="number" min={1} max={5} step={1} value={groupQuota} onChange={e=>setGroupQuota(Number(e.target.value))}/></label></div>
  <p className="help-text">只能收紧保守的 5 分钟与 4/5 次应用上限，不承诺平台一定允许。策略变更用于新来源，已接受消息不会获得更宽期限或额度。</p>
  {revokes&&<label className="qq-check"><input type="checkbox" checked={confirmed} onChange={e=>setConfirmed(e.target.checked)}/>确认停用或换 AppID 会撤销旧绑定及未兑换配对码；重新启用不会恢复旧权限，在途消息可能无法召回</label>}
  {error&&<p className="help-text">{error}</p>}
  <Button disabled={disabled||Boolean(error)||(revokes&&!confirmed)} onClick={()=>void onSubmit(command)}>{configuration?"保存此修订配置":"停用创建 QQ 配置"}</Button>
 </fieldset>;
}
