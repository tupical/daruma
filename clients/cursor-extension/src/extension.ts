import * as vscode from "vscode";
import { Plan, Task, TaskagentApiClient } from "./apiClient.js";
import { TaskagentTreeProvider } from "./tree.js";

export function activate(context: vscode.ExtensionContext): void {
  const client = createClient();
  const treeProvider = new TaskagentTreeProvider(client);
  const tree = vscode.window.createTreeView("taskagent.tasks", {
    treeDataProvider: treeProvider,
    showCollapseAll: true
  });
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  status.text = "$(checklist) TaskAgent";
  status.command = "taskagent.refresh";
  status.show();
  const refresh = async () => {
    treeProvider.refresh();
    await updateStatus(status, client);
  };
  const timer = setInterval(() => {
    void refresh();
  }, 10_000);

  context.subscriptions.push(
    tree,
    status,
    { dispose: () => clearInterval(timer) },
    vscode.commands.registerCommand("taskagent.refresh", refresh),
    vscode.commands.registerCommand("taskagent.completeTask", async (task?: Task) => {
      if (!task?.id) {
        return;
      }
      await client.completeTask(task.id);
      treeProvider.refresh();
    }),
    vscode.commands.registerCommand("taskagent.showTask", async (task: Task) => {
      await vscode.window.showInformationMessage(`${task.title} (${task.status ?? "unknown"})`);
    }),
    vscode.commands.registerCommand("taskagent.openPlan", async (plan?: Plan) => {
      if (!plan?.id) {
        return;
      }
      const detail = await client.getPlan(plan.id);
      const panel = vscode.window.createWebviewPanel(
        "taskagent.plan",
        detail.plan.title,
        vscode.ViewColumn.One,
        { enableScripts: false }
      );
      const pct = Math.round(detail.progress.completion_pct ?? 0);
      panel.webview.html = `<!doctype html>
<html>
  <body>
    <h1>${escapeHtml(detail.plan.title)}</h1>
    <p>Status: ${escapeHtml(detail.plan.status ?? "unknown")}</p>
    <progress value="${pct}" max="100"></progress>
    <p>${pct}% complete</p>
  </body>
</html>`;
    })
  );

  void refresh();
  registerCursorMcpServer();
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (ch) => {
    switch (ch) {
      case "&":
        return "&amp;";
      case "<":
        return "&lt;";
      case ">":
        return "&gt;";
      case "\"":
        return "&quot;";
      default:
        return "&#39;";
    }
  });
}

async function updateStatus(status: vscode.StatusBarItem, client: TaskagentApiClient): Promise<void> {
  try {
    const tasks = await client.listTasks();
    const open = tasks.filter((task) => task.status !== "done" && task.status !== "cancelled").length;
    status.text = `$(checklist) TaskAgent ${open}`;
    status.tooltip = `${open} open TaskAgent task(s)`;
  } catch {
    status.text = "$(warning) TaskAgent";
    status.tooltip = "TaskAgent server is unreachable";
  }
}

export function deactivate(): void {}

function createClient(): TaskagentApiClient {
  const config = vscode.workspace.getConfiguration("taskagent");
  const apiUrl = config.get<string>("apiUrl") || process.env.TASKAGENT_API_URL || "http://localhost:8080";
  const token = config.get<string>("token") || process.env.TASKAGENT_TOKEN || "";
  return new TaskagentApiClient(apiUrl, token);
}

function registerCursorMcpServer(): void {
  const cursor = (vscode as unknown as { cursor?: { mcp?: { registerServer?: (server: unknown) => void } } }).cursor;
  const registerServer = cursor?.mcp?.registerServer;
  if (!registerServer) {
    return;
  }
  const config = vscode.workspace.getConfiguration("taskagent");
  const apiUrl = config.get<string>("apiUrl") || process.env.TASKAGENT_API_URL || "http://localhost:8080";
  const token = config.get<string>("token") || process.env.TASKAGENT_TOKEN || "";
  registerServer({
    name: "taskagent",
    server: {
      url: `${apiUrl.replace(/\/$/, "")}/v1/mcp`,
      headers: token ? { Authorization: `Bearer ${token}` } : undefined
    }
  });
}
