# Token Save Audit — сессия «проверь задачи daruma cloud»

Дата: 2026-06-05  
Контекст: пользователь попросил проверить задачи в трекере Daruma Cloud,
сверить с репо и закрыть выполненные. Агент потратил ~85–120K токенов на
MCP-ответы вместо ~3–6K. Ниже — разбор расхода и root cause, почему правила
из `clients/cursor-plugin` не сработали.

---

## 1. Фактический расход MCP-токенов

Точного счётчика «MCP = N токенов» у агента нет. Оценка по числу вызовов,
размеру ответов и дублированию. Грубо: **~1 токен ≈ 3–4 символа** в JSON/тексте.

### Вызовы по категориям

| Категория | Вызовы | Оценка payload |
|-----------|--------|----------------|
| **daruma** | ~22 | ~120–150K символов (~30–45K токенов) |
| **Glob** | 3 | ~50–80K символов (truncated, но дорого) |
| **Grep** | 4 | ~5–10K символов |
| **codegraph** | 1 | мало (пустой ответ) |
| **Shell** | 2 | мало |

**Итого MCP-ответы в контекст модели: ~85–120K токенов** (плюс аргументы
вызовов, плюс рассуждения между шагами).

### Распределение (оценка)

| Источник | Доля | Символы |
|----------|------|---------|
| `daruma_workspacegraph_search` (limit=100) | ~40% | 170 KB |
| `daruma_search` ×2 (limit=50) | ~35% | ~50–100 KB каждый |
| `Glob **/*` репо ×2 | ~15% | тысячи путей `.next/`, `.deploy-logs/` |
| `daruma_get` ×13 | ~8% | ~1–2 KB каждый |
| Остальное (list, plan, grep) | ~2% | — |

### Где ушло больше всего

1. **`daruma_workspacegraph_search` (limit=100)** — 170 KB JSON.
   Результат: 0 открытых задач. Но `daruma_list status=active` уже дал
   **1 задачу в inbox**. **~40–45K токенов впустую.**

2. **`daruma_search` ×2** (`cloud`, `daruma`, limit=50) — смесь задач +
   комментариев + планов. Нужен был только список **незакрытых** задач.
   **~25–35K токенов.**

3. **`Glob **/*` и `Glob **/daruma-cloud/**`** — тысячи путей артефактов
   сборки. Для аудита не нужны. **~15–25K токенов.**

4. **`daruma_get` ×13** — проверка задач, которые в search уже были `done`
   с комментариями аудита. **~5–8K токенов** (меньше, но лишние round-trips).

5. **Ошибочный вызов** — `daruma_search` без `project_scope` → ambiguous
   scope. Мало токенов, но лишний hop.

6. **`codegraph_files`** на daruma-cloud — индекс пустой, ответ бесполезен.

---

## 2. Минимальный путь с тем же результатом

Цель: «проверить задачи cloud, закрыть сделанное».

| # | Вызов | Зачем |
|---|-------|-------|
| 1 | `daruma_workspace_info` | scope `daruma-cloud` |
| 2 | `daruma_list status=active project_scope=daruma-cloud` | **1 inbox-задача** |
| 3 | `daruma_plan_list status=completed project_scope=daruma-cloud` | планы закрыты |
| 4 | `Grep elicit` в `/home/av/projects/daruma-cloud` | фича не в коде |
| 5 | `daruma_comment` на inbox-задачу | зафиксировать аудит |

**Оценка: ~3–6K токенов MCP-ответов** (вместо ~85–120K).

**Экономия: ~90–95%** (~80–110K токенов на MCP-части).

### Как получить тот же результат дешевле

1. **Сначала узкий list, не search** — `daruma_list { status: "active", project_scope: "daruma-cloud" }`.
2. **Не вызывать workspacegraph_search для «что открыто»** — `list active` достаточен.
3. **Не сканировать репо Glob'ом** — один точечный `Grep` по ключевому слову.
4. **Не перепроверять get'ами то, что уже done** — если `list` вернул 1 inbox, проверять репо только для неё.
5. **Всегда передавать scope с первого вызова** — `project_scope: "daruma-cloud"`.
6. **Codegraph — только если индекс есть** — иначе сразу `Grep` в репо.
7. **Закрывать только после верификации открытых** — `list active` → grep/read → complete.

---

## 3. Итог сессии для пользователя

| Метрика | Факт | Оптимум |
|---------|------|---------|
| MCP-вызовов | ~32 | ~5 |
| MCP-токены (оценка) | **~85–120K** | **~3–6K** |
| Экономия | — | **~90–95%** |
| Результат | 1 inbox backlog, всё остальное уже done | тот же |

Главная ошибка стратегии: **широкий поиск (search + graph + glob) вместо
узкого list + точечной проверки одной открытой задачи**.

---

## 4. Диагноз: почему инструкции из cursor-plugin не сработали

### 4.1 Плагин установлен и актуален — это не «не обновился»

| Что проверено | Результат |
|---------------|-----------|
| `daruma-policy.mdc` plugin vs `/home/av/projects/.cursor/rules/` | **идентичны** (diff пустой) |
| `daruma.mdc` | **идентичны** |
| `daruma-tasks.md` в `.cursor/commands/` | **установлен** |
| `workspacegraph.mdc` в `.cursor/rules/` | **отсутствует** |

`installRules()` копирует только 2 файла (`lib/rules.mjs`):

```js
export const RULE_FILES = ["daruma-policy.mdc", "daruma.mdc"];
```

`workspacegraph.mdc` лежит в репо, но **не ставится** ни CLI, ни `plugin.json`:

```json
"rules": [
  { "path": "cursor/rules/daruma-policy.mdc", ... },
  { "path": "cursor/rules/daruma.mdc", ... }
]
```

Документация (`docs/README.ru.md`) говорит:

> «Cursor rule `workspacegraph.mdc` … bundled in `clients/cursor-plugin/`»

— но installer его не доставляет. **Docs ≠ install contract.**

`installRules` без `--force` пропускает существующие файлы — при будущих
обновлениях правила могут застрять; **на момент аудита** они свежие.

---

### 4.2 Главная причина: правила толкают к `search`, а не к `list`

**Always-applied** `daruma-policy.mdc` (§7):

> «use `daruma_search` for targeted lookups»

**On-demand** `daruma.mdc` (§ Listing):

> «**Prefer search over bulk list** when the user names a topic, plan phase,
> or keyword»

Запрос: *«в трекере есть daruma cloud…»* — агент воспринял **«daruma
cloud» как keyword** → `daruma_search limit=50` ×2.

**Правильный алгоритм** уже в slash-команде `daruma-tasks.md`:

```
daruma_list with project_id = <resolved>, status = ["inbox", "todo", "in_progress"]
Never use status=all — token-heavy
```

Но `/daruma-tasks` срабатывает только при явном вызове команды, не при
естественном языке «проверь задачи и закрой».

---

### 4.3 `workspacegraph_search` — без guardrails

`workspacegraph.mdc` **не установлен**, но MCP-инструменты доступны. В файле
есть запрет:

> «Skip WorkspaceGraph … when `daruma_relations` / `daruma_plan_graph`
> already answer the question»

Агент этого не видел → `workspacegraph_search limit=100` → 170 KB, хотя
`daruma_list status=active` уже дал ответ.

MCP-описание `daruma_list` (`crates/mcp/src/tools.rs`) само предлагает
search:

```
prefer `active` or a narrow status filter, or `daruma_search`
```

**Тройное давление:** policy → daruma.mdc → MCP tool description — все в
сторону search.

---

### 4.4 Нет правила для сценария «аудит + закрыть»

В `cursor/rules/` и `cursor/commands/` нет workflow:

1. `list active` по `project_scope`
2. для каждой открытой — точечная проверка в репо
3. `complete` только подтверждённых
4. **стоп**, если кроме backlog нечего закрывать

Поэтому агент ушёл в «исследовательский» режим: search → graph → glob → 13× get.

---

### 4.5 Вторичные факторы

| Фактор | Эффект |
|--------|--------|
| Первый `search` без `project_scope` | ambiguous scope → лишний round-trip |
| `daruma.mdc` с `alwaysApply: false` | hygiene мог не подгрузиться; policy (с search-hint) — всегда |
| Glob `**/*` по репо | не из правил, ошибка агента |

---

## 5. Сводка причин

```
Запрос: проверь задачи cloud, закрой
    → alwaysApply: daruma-policy (hint: use daruma_search)
    → keyword: "daruma cloud" → search ×2 + workspacegraph 170KB

Правильный путь (daruma-tasks.md) — не auto-load
workspacegraph.mdc — не в RULE_FILES → нет guardrails
```

| Причина | Вес |
|---------|-----|
| Противоречие policy/mdc: search vs list | **высокий** |
| Нет rule/command для audit-close workflow | **высокий** |
| workspacegraph.mdc не установлен, MCP tools доступны | **средний** |
| MCP tool descriptions дублируют «or search» | **средний** |
| Плагин не обновился | **низкий (опровергнуто)** |

**Итог:** инструкции **были написаны частично** (в `/daruma-tasks`), но
**не те, что всегда в контексте**, и они **противоречат** always-applied
policy. Плагин обновлён; агент следовал тому, что видел в `alwaysApply: true`
+ MCP tool hints.

---

## 6. Рекомендуемые исправления

1. **Убрать/уточнить** «Prefer search» в `daruma.mdc` и `daruma-policy.mdc`:
   - `list` — для «что открыто / аудит / закрыть»
   - `search` — только текстовый поиск по архиву/комментариям по явному запросу

2. **Добавить** секцию или `daruma-audit.mdc`:
   ```
   workspace_info → list active + project_scope →
   для каждой open: grep/read → complete →
   STOP; не search/graph/glob для inventory
   ```

3. **Включить** `workspacegraph.mdc` в `RULE_FILES` + `plugin.json`, с явным:
   > «Never workspacegraph_search to list open tasks — use daruma_list»

4. **Поправить** description `daruma_list` в `crates/mcp/src/tools.rs` —
   убрать «or daruma_search».

5. **Обновить** `docs/README.ru.md` — «bundled» ≠ «installed by default».

6. **`daruma-cursor doctor`** — предупреждать, если `workspacegraph.mdc`
   отсутствует при зарегистрированных workspacegraph MCP tools.

---

## 7. Связанные файлы

| Файл | Роль |
|------|------|
| `cursor/rules/daruma-policy.mdc` | alwaysApply; hint search |
| `cursor/rules/daruma.mdc` | on-demand; Prefer search |
| `cursor/rules/workspacegraph.mdc` | bundled, не installed |
| `cursor/commands/daruma-tasks.md` | правильный list-workflow |
| `lib/rules.mjs` | RULE_FILES = 2 файла |
| `.daruma-plugin/plugin.json` | rules manifest без workspacegraph |
| `crates/mcp/src/tools.rs` | MCP tool descriptions |
