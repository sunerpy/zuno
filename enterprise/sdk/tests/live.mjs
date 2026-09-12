import test from "node:test";
import assert from "node:assert/strict";
import { LiveActivity } from "../dist/src/index.js";

const frame=(generation,sequence,after="1",text="draft")=>({
  version:1,sessionId:"session",turnId:"turn",generation,sequence:String(sequence),afterCommitted:after,
  event:{kind:"snapshot",items:[{kind:"text",id:"draft",parentId:"message:source",text,truncated:false}]},
});
test("live snapshots wait for committed history and discard retired generations",()=>{
  const state=new LiveActivity("session");
  assert.equal(state.apply(frame("a",1,"2"),"1"),false);
  state.apply(frame("a",1,"2"),"2");
  state.apply(frame("b",1,"3","replacement"),"3");
  assert.equal(state.apply(frame("a",2,"2","stale"),"3"),false);
  assert.equal(state.items()[0].text,"replacement");
});
test("a retry snapshot clears drafts and newer progress can return after silence",()=>{
  const state=new LiveActivity("session");
  state.apply(frame("a",1),"1");
  const reset=frame("a",2);reset.event.items=[];
  state.apply(reset,"1");
  assert.deepEqual(state.items(),[]);
  state.apply(frame("a",3,"1","new attempt text"),"1");
  state.apply(null,"1");
  assert.deepEqual(state.items(),[]);
  assert.equal(state.apply(frame("a",2,"1","stale"),"1"),false);
  state.apply(frame("a",4,"1","newer snapshot"),"1");
  assert.equal(state.items()[0].text,"newer snapshot");
});
