import assert from "node:assert/strict";
import test from "node:test";
import { TaskagentApiClient } from "../dist/apiClient.js";

test("listTasks adds project filter and bearer header", async () => {
  const seen = [];
  const client = new TaskagentApiClient("http://taskagent.local/", "ta_svc_test", async (url, init = {}) => {
    seen.push({ url, init });
    return new Response(JSON.stringify([{ id: "tsk_1", title: "One" }]), { status: 200 });
  });

  const tasks = await client.listTasks("prj_1");

  assert.equal(tasks[0].title, "One");
  assert.equal(seen[0].url, "http://taskagent.local/v1/tasks?status=active&project_id=prj_1");
  assert.equal(seen[0].init.headers.authorization, "Bearer ta_svc_test");
});

test("completeTask posts command envelope", async () => {
  let body = null;
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (_url, init = {}) => {
    body = JSON.parse(String(init.body));
    return new Response(JSON.stringify({ success: true }), { status: 200 });
  });

  await client.completeTask("tsk_1");

  assert.deepEqual(body.command, { type: "complete_task", id: "tsk_1" });
});

test("getPlanGraph fetches graph endpoint", async () => {
  let seenUrl = "";
  const client = new TaskagentApiClient("http://taskagent.local", "", async (url) => {
    seenUrl = url;
    return new Response(JSON.stringify({ nodes: [] }), { status: 200 });
  });

  await client.getPlanGraph("pln_1");

  assert.equal(seenUrl, "http://taskagent.local/v1/plans/pln_1/graph");
});

test("claimTask posts claim_task command", async () => {
  let body = null;
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (_url, init = {}) => {
    body = JSON.parse(String(init.body));
    return new Response(JSON.stringify({}), { status: 200 });
  });

  await client.claimTask("tsk_2");

  assert.equal(body.command.type, "claim_task");
  assert.equal(body.command.id, "tsk_2");
});

test("commentTask posts comment_task command with body", async () => {
  let body = null;
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (_url, init = {}) => {
    body = JSON.parse(String(init.body));
    return new Response(JSON.stringify({}), { status: 200 });
  });

  await client.commentTask("tsk_3", "looks good");

  assert.equal(body.command.type, "comment_task");
  assert.equal(body.command.id, "tsk_3");
  assert.equal(body.command.body, "looks good");
});

test("setTaskPriority posts set_priority command", async () => {
  let body = null;
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (_url, init = {}) => {
    body = JSON.parse(String(init.body));
    return new Response(JSON.stringify({}), { status: 200 });
  });

  await client.setTaskPriority("tsk_4", "p1");

  assert.equal(body.command.type, "set_priority");
  assert.equal(body.command.id, "tsk_4");
  assert.equal(body.command.priority, "p1");
});

test("splitTask posts split_task command with subtasks array", async () => {
  let body = null;
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (_url, init = {}) => {
    body = JSON.parse(String(init.body));
    return new Response(JSON.stringify({}), { status: 200 });
  });

  await client.splitTask("tsk_5", ["Part A", "Part B"]);

  assert.equal(body.command.type, "split_task");
  assert.equal(body.command.id, "tsk_5");
  assert.deepEqual(body.command.subtasks, [{ title: "Part A" }, { title: "Part B" }]);
});

test("getEventsSince fetches events/since without cursor", async () => {
  let seenUrl = "";
  const client = new TaskagentApiClient("http://taskagent.local", "token", async (url) => {
    seenUrl = url;
    return new Response(JSON.stringify({ events: [], cursor: "cur_1" }), { status: 200 });
  });

  const result = await client.getEventsSince();

  assert.equal(seenUrl, "http://taskagent.local/v1/events/since");
  assert.deepEqual(result.events, []);
  assert.equal(result.cursor, "cur_1");
});

test("getEventsSince passes cursor as since query param", async () => {
  let seenUrl = "";
  const client = new TaskagentApiClient("http://taskagent.local", "", async (url) => {
    seenUrl = url;
    return new Response(JSON.stringify({ events: [{ id: "evt_1", type: "task.updated" }] }), { status: 200 });
  });

  const result = await client.getEventsSince("cur_1");

  assert.equal(seenUrl, "http://taskagent.local/v1/events/since?since=cur_1");
  assert.equal(result.events[0].id, "evt_1");
});
