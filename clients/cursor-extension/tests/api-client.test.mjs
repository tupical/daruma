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
  assert.equal(seen[0].url, "http://taskagent.local/v1/tasks?project_id=prj_1");
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
