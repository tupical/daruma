export interface Project {
  id: string;
  title: string;
}

export interface Task {
  id: string;
  title: string;
  status?: string;
  priority?: string | null;
  project_id?: string | null;
}

export interface Plan {
  id: string;
  project_id: string;
  title: string;
  status?: string;
}

export interface PlanProgress {
  tasks_total?: number;
  tasks_done?: number;
  completion_pct?: number;
}

export interface PlanDetail {
  plan: Plan;
  progress: PlanProgress;
}

export interface PlanGraphNode {
  task_id: string;
  title: string;
  status?: string;
  position: number;
}

export interface PlanGraph {
  nodes: PlanGraphNode[];
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export class TaskagentApiClient {
  private readonly apiUrl: string;
  private readonly token: string;
  private readonly fetchFn: FetchLike;

  constructor(apiUrl: string, token: string, fetchFn: FetchLike = fetch) {
    this.apiUrl = apiUrl.replace(/\/$/, "");
    this.token = token;
    this.fetchFn = fetchFn;
  }

  async listProjects(): Promise<Project[]> {
    return this.getJson<Project[]>("/v1/projects");
  }

  async listTasks(projectId?: string): Promise<Task[]> {
    const path = projectId ? `/v1/tasks?project_id=${encodeURIComponent(projectId)}` : "/v1/tasks";
    return this.getJson<Task[]>(path);
  }

  async listPlans(projectId?: string): Promise<Plan[]> {
    const path = projectId ? `/v1/plans?project_id=${encodeURIComponent(projectId)}` : "/v1/plans";
    return this.getJson<Plan[]>(path);
  }

  async getPlan(id: string): Promise<PlanDetail> {
    return this.getJson<PlanDetail>(`/v1/plans/${encodeURIComponent(id)}`);
  }

  async getPlanGraph(id: string): Promise<PlanGraph> {
    return this.getJson<PlanGraph>(`/v1/plans/${encodeURIComponent(id)}/graph`);
  }

  async completeTask(id: string): Promise<void> {
    await this.postJson("/v1/commands", {
      command: { type: "complete_task", id },
      actor: { kind: "user" }
    });
  }

  private async getJson<T>(path: string): Promise<T> {
    const response = await this.fetchFn(`${this.apiUrl}${path}`, {
      headers: this.headers()
    });
    return decodeJson<T>(response);
  }

  private async postJson<T>(path: string, body: unknown): Promise<T> {
    const response = await this.fetchFn(`${this.apiUrl}${path}`, {
      method: "POST",
      headers: {
        ...this.headers(),
        "content-type": "application/json"
      },
      body: JSON.stringify(body)
    });
    return decodeJson<T>(response);
  }

  private headers(): Record<string, string> {
    return this.token ? { authorization: `Bearer ${this.token}` } : {};
  }
}

async function decodeJson<T>(response: Response): Promise<T> {
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`TaskAgent HTTP ${response.status}: ${text}`);
  }
  return text ? (JSON.parse(text) as T) : (null as T);
}
