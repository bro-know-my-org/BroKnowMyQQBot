import { expect, it } from "vitest";
import { logSource, readLog } from "./logPresentation";
import type { LogEntry, PluginView } from "./types";

const record = (fields: Record<string, string>): LogEntry => ({
  id: 1, timestamp: "2026-11-14T00:00:00Z", category: "runtime", level: "INFO",
  target: "bkm::message_trace", message: "safe original", instance_id: null, fields,
});
it.each([["private", "私聊"], ["group", "群聊"], ["channel", "频道"], ["", ""], ["constructor", ""]])("只从安全scope显示会话类型 %s", (message_scope, label) => {
  expect(readLog(record({ record_kind: "message.received", message_scope }))?.summary).toBe(`收到一条${label}消息`);
});
it("不从ID、平台事件名或格式化消息猜测会话类型", () => {
  const entry = record({ record_kind: "message.received", event_id: "C2C_MESSAGE_CREATE:user", event_type: "GROUP_AT_MESSAGE_CREATE", user_id: "private-user", scope: "group" });
  entry.message = "received private group channel message";
  expect(readLog(entry)).toEqual({ summary: "收到一条消息", status: "已收到" });
});
it.each([
  ["message.sending", "发送中"], ["message.sent", "已发送"], ["message.failed", "发送失败"],
  ["plugin.failed", "执行失败"],
])("区分收发与失败状态 %s", (record_kind, status) => {
  expect(readLog(record({ record_kind }))?.status).toBe(status);
});
it("声明命中只是候选，无输出不能称为已处理或已回复", () => {
  expect(readLog(record({ record_kind: "plugin.invoked", command_name: "echo" }))).toMatchObject({ summary: "插件开始执行", status: "指令候选" });
  expect(readLog(record({ record_kind: "plugin.invoked" }))?.status).toBe("调用中");
  expect(readLog(record({ record_kind: "plugin.completed", command_name: "echo", status: "no_output" }))).toMatchObject({ summary: "插件未产生输出", status: "无输出" });
});
it("已提交输出与动作发送分开，状态写入或诊断不谎称回复指令", () => {
  const fields = { record_kind: "plugin.completed", command_name: "echo", status: "output_committed", command_count: "1", state_op_count: "2", diagnostic_count: "0" };
  expect(readLog(record(fields))).toEqual({ summary: "插件产生了输出", status: "输出已提交", detail: "候选指令：echo · 动作 1 · 状态写入 2" });
  expect(readLog(record({ ...fields, command_count: "0" }))?.summary).toBe("插件产生了输出");
  const unrelated = readLog(record({ ...fields, action_type: "http.request" }));
  expect(unrelated?.summary).toBe("插件产生了输出"); expect(unrelated?.detail).toContain("候选指令：echo");
  expect(unrelated?.summary).not.toMatch(/响应|回复|处理成功/u);
});
it.each([
  ["succeeded", "已执行"], ["failed", "执行失败"], ["denied", "权限拒绝"],
  ["timed_out", "执行超时"], ["unknown", "结果未知"], ["cancelled", "已取消"],
])("动作回执保持真实状态 %s", (status, expected) => {
  expect(readLog(record({ record_kind: "plugin.action.completed", status, action_type: "message.reply" }))).toEqual({ summary: "插件动作执行结果", status: expected, detail: "回复消息" });
});
it("未知日志和未知完成状态回退到原始记录", () => {
  const cases: Record<string, string>[] = [{}, { record_kind: "new.event" }, { record_kind: "plugin.completed", status: "new_status" }, { record_kind: "plugin.action.completed", status: "constructor" }];
  for (const fields of cases) {
    expect(readLog(record(fields))).toBeUndefined();
  }
});
it("插件名称只按显式实例关联，ID不匹配时保留日志自己的插件标签", () => {
  const plugins = new Map([["ping/default", { name: "连通测试", plugin_id: "dev.bkm.ping" } as PluginView]]);
  const entry = record({ adapter_id: "qq-official", plugin_id: "dev.bkm.ping" }); entry.instance_id = "ping/default";
  expect(logSource(entry, plugins)).toBe("连通测试 · ping/default · qq-official");
  expect(logSource({ ...entry, fields: { plugin_id: "dev.other.plugin" } }, plugins)).toBe("dev.other.plugin · ping/default");
  expect(logSource(record({ adapter_id: "qq-official" }), plugins)).toBe("qq-official");
});
