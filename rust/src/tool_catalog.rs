//! Состав инструментов роутера: параллельный опрос баз, выбор схемы и файл состава.
//!
//! Модуль снимает `initialize` + `tools/list` со ВСЕХ баз сразу (а не с первой
//! живой), объединяет инструменты по имени, берёт определение с самой старой
//! версией расширения и вшивает узкий `base` — только те базы, которые этот
//! инструмент действительно предоставляют. Итог до применения белого списка
//! пишется в файл состава (`_generated`, `tools_enabled`, `common`, `separate`);
//! белый список глобальный: `MCP_TOOLS_ENABLED` главнее списка из файла состава,
//! а сам список применяется в `list_tools`/`call_tool` (см. [`crate::server`]).
//!
//! Секция `tools_enabled` — единственное, что читается из файла состава перед
//! перезаписью: список из переменной окружения в файл не попадает, а прежний
//! сохраняется без изменений (см. [`read_tools_enabled`] и [`set_tools_enabled`]).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::Tool;
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::forward::BaseForwarder;
use crate::server::RouterServer;

/// Имя файла состава инструментов по умолчанию.
pub const TOOLS_FILE_NAME: &str = "tools.json";

/// Период фонового повтора опроса баз без успешного снимка, секунды.
pub const RETRY_PERIOD_SECS: u64 = 120;

/// Снимок одной базы: версия расширения и исходный `tools/list` без изменений.
#[derive(Debug, Clone)]
pub struct BaseCatalog {
    pub name: String,
    /// Версия расширения 1С (`serverInfo.version` из ответа `initialize`).
    pub version: String,
    /// Инструменты базы ровно в том виде, в каком их отдал `tools/list`.
    pub tools: Vec<Value>,
}

impl BaseCatalog {
    /// Снимок базы. Версия проверяется сразу: непригодная для сравнения версия —
    /// это неудачный опрос, а не повод строить состав наугад.
    pub fn new(name: &str, version: &str, tools: Vec<Value>) -> Result<Self, String> {
        parse_version(version)
            .map_err(|e| format!("база {name}: непригодная версия расширения '{version}': {e}"))?;
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            tools,
        })
    }
}

/// Итог сборки состава: выбранные инструменты (с уже вшитым `base`) и документ
/// состава для файла.
#[derive(Clone)]
pub struct CatalogSnapshot {
    /// Итоговые инструменты в схеме роутера, имена отсортированы.
    pub tools: Vec<Tool>,
    /// Матрица «инструмент → базы, которые его предоставляют». Хранится в
    /// снимке, чтобы роутер мог отбивать вызов с чужой базой до форвардинга.
    pub tool_bases: BTreeMap<String, Vec<String>>,
    /// Содержимое файла состава: `_generated`, `common`, `separate`; секция
    /// `tools_enabled` добавляется при записи ([`set_tools_enabled`]) и порядок
    /// ключей задаёт [`write_tools_file`], а не этот `Value`.
    pub document: Value,
}

/// Состояние состава: успешные снимки и базы, которые ещё не ответили.
#[derive(Debug, Default)]
pub struct CatalogState {
    /// Успешные снимки: имя базы → версия расширения и исходные инструменты.
    pub catalogs: HashMap<String, BaseCatalog>,
    /// Ещё не ответившие базы с причиной, в порядке настроек.
    pub missing: Vec<(String, String)>,
    /// Белый список из прежнего файла состава: перезапись (в том числе
    /// фоновая) повторяет его без изменений. `None` — секции не было.
    pub tools_enabled: Option<Vec<String>>,
}

impl CatalogState {
    pub fn new(catalogs: HashMap<String, BaseCatalog>, missing: Vec<(String, String)>) -> Self {
        Self {
            catalogs,
            missing,
            tools_enabled: None,
        }
    }

    /// Запомнить белый список из прежнего файла состава — он же попадает в
    /// каждую следующую перезапись.
    pub fn with_tools_enabled(mut self, tools_enabled: Option<Vec<String>>) -> Self {
        self.tools_enabled = tools_enabled;
        self
    }

    /// Сохранить успешный снимок и снять базу с ожидания. Уже сохранённые
    /// снимки не удаляются никогда: исчезнувшая база оставляет инструменты
    /// доступными, ошибка возникает при обычном форвардинге.
    pub fn apply_success(&mut self, catalog: BaseCatalog) {
        self.missing.retain(|(name, _)| name != &catalog.name);
        self.catalogs.insert(catalog.name.clone(), catalog);
    }

    /// Снимок базы, если она уже ответила.
    pub fn catalog(&self, name: &str) -> Option<&BaseCatalog> {
        self.catalogs.get(name)
    }

    /// Причина, по которой база ещё не ответила.
    pub fn failure_reason(&self, name: &str) -> Option<&str> {
        self.missing
            .iter()
            .find(|(base, _)| base == name)
            .map(|(_, reason)| reason.as_str())
    }
}

/// Разобрать версию расширения в числовые компоненты.
/// Компонентов от одного до четырёх, отсутствующие считаются нулями.
///
/// Живые базы отдают версию с суффиксом сборки (`1.8.2-lite`), поэтому
/// сравнивается числовое ядро до первого `-` или `+`: суффикс на порядок
/// версий не влияет, а отказ из-за него оставил бы живую базу без инструментов.
fn parse_version(version: &str) -> Result<[u64; 4], String> {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        return Err("пустая версия".to_string());
    }
    let core = trimmed.split(['-', '+']).next().unwrap_or_default().trim();
    if core.is_empty() {
        return Err(format!("нет числовой части в '{version}'"));
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 4 {
        return Err(format!(
            "ожидалось не больше четырёх компонентов, получено {}",
            parts.len()
        ));
    }
    let mut out = [0u64; 4];
    for (index, part) in parts.iter().enumerate() {
        out[index] = part
            .parse::<u64>()
            .map_err(|_| format!("компонент '{part}' не число"))?;
    }
    Ok(out)
}

/// Сравнение версий расширения: численно по компонентам, включая четвёртый.
/// Равные версии считаются одинаковыми — порядок баз решает вызывающая сторона.
pub fn compare_extension_versions(a: &str, b: &str) -> Result<Ordering, String> {
    Ok(parse_version(a)?.cmp(&parse_version(b)?))
}

/// `initialize` + `tools/list` одной базы. Снимок успешен, только если оба
/// ответа корректны.
pub async fn fetch_base_catalog(
    name: &str,
    forwarder: &BaseForwarder,
) -> Result<BaseCatalog, String> {
    let version = forwarder.initialize_version().await?;
    let tools = forwarder.list_tools_raw().await?;
    BaseCatalog::new(name, &version, tools)
}

/// Результат параллельного опроса: успешные снимки и базы без ответа.
pub struct CollectedCatalogs {
    pub catalogs: HashMap<String, BaseCatalog>,
    pub failures: Vec<(String, String)>,
}

/// Опросить все переданные базы: на каждую — своя Tokio-задача, ждём завершения
/// всех задач. Успех одной базы не останавливает опрос остальных.
pub async fn collect_catalogs(targets: Vec<(String, BaseForwarder)>) -> CollectedCatalogs {
    let mut handles = Vec::with_capacity(targets.len());
    for (name, forwarder) in targets {
        let task_name = name.clone();
        handles.push((
            name,
            tokio::spawn(async move { fetch_base_catalog(&task_name, &forwarder).await }),
        ));
    }

    let mut catalogs = HashMap::new();
    let mut failures = Vec::new();
    for (name, handle) in handles {
        match handle.await {
            Ok(Ok(catalog)) => {
                info!(
                    base = %name,
                    version = %catalog.version,
                    tools = catalog.tools.len(),
                    "снимок базы получен"
                );
                catalogs.insert(name, catalog);
            }
            Ok(Err(reason)) => {
                warn!(base = %name, error = %reason, "база не ответила");
                failures.push((name, reason));
            }
            Err(join) => {
                let reason = format!("задача опроса упала: {join}");
                warn!(base = %name, error = %reason, "база не ответила");
                failures.push((name, reason));
            }
        }
    }
    CollectedCatalogs { catalogs, failures }
}

/// Собрать состав по успешным снимкам: объединение по имени, `common` —
/// пересечение по всем опрошенным базам, `separate` — «свои» инструменты базы.
pub fn build_catalog(catalogs: &HashMap<String, BaseCatalog>, order: &[String]) -> CatalogSnapshot {
    // Базы в порядке настроек: он же решает спор равных версий расширения.
    let ready: Vec<&BaseCatalog> = order.iter().filter_map(|name| catalogs.get(name)).collect();

    let versions: Vec<String> = ready
        .iter()
        .map(|catalog| format!("{}={}", catalog.name, catalog.version))
        .collect();
    info!(
        bases = %versions.join(", "),
        polled = ready.len(),
        "состав инструментов собирается"
    );

    // Матрица «инструмент → базы, которые его предоставляют».
    let mut tool_bases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for catalog in &ready {
        for tool in &catalog.tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                warn!(base = %catalog.name, "инструмент без имени в ответе tools/list — пропущен");
                continue;
            };
            let bases = tool_bases.entry(name.to_string()).or_default();
            if !bases.iter().any(|base| base == &catalog.name) {
                bases.push(catalog.name.clone());
            }
        }
    }

    // common — пересечение по всем успешно опрошенным базам. База, ответившая
    // пустым tools, тоже участвует, поэтому пересечение с ней пусто.
    let common: BTreeSet<String> = if ready.is_empty() {
        BTreeSet::new()
    } else {
        tool_bases
            .iter()
            .filter(|(_, bases)| bases.len() == ready.len())
            .map(|(name, _)| name.clone())
            .collect()
    };

    // separate — только инструменты базы вне common; пустые записи не пишем.
    let mut separate: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for catalog in &ready {
        let mut own: Vec<String> = catalog
            .tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .filter(|name| !common.contains(*name))
            .map(str::to_string)
            .collect();
        own.sort();
        own.dedup();
        if !own.is_empty() {
            separate.insert(catalog.name.clone(), own);
        }
    }

    // Выбранные определения: самая старая версия расширения среди баз,
    // предоставляющих инструмент; при равенстве — порядок настроек.
    let mut tools = Vec::with_capacity(tool_bases.len());
    for (name, base_names) in &tool_bases {
        let candidates: Vec<(&BaseCatalog, &Value)> = base_names
            .iter()
            .filter_map(|base| {
                let catalog = catalogs.get(base)?;
                let tool = catalog
                    .tools
                    .iter()
                    .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))?;
                Some((catalog, tool))
            })
            .collect();
        let Some((source, raw)) = select_oldest_schema(name, &candidates) else {
            warn!(tool = %name, "нет пригодного определения инструмента среди баз");
            continue;
        };
        log_schema_divergence(name, &candidates, source);
        match inject_base(raw.clone(), base_names) {
            Some(tool) => tools.push(tool),
            None => warn!(tool = %name, "не удалось разобрать Tool из ответа базы"),
        }
    }

    let document = json!({
        "_generated": utc_now_iso8601(),
        "common": common.iter().cloned().collect::<Vec<String>>(),
        "separate": separate,
    });

    info!(
        tools = tools.len(),
        common = common.len(),
        separate = document["separate"]
            .as_object()
            .map(|map| map.len())
            .unwrap_or(0),
        "состав инструментов собран"
    );

    CatalogSnapshot {
        tools,
        tool_bases,
        document,
    }
}

/// Определение инструмента с самой старой версией расширения. Кандидаты идут
/// в порядке настроек, поэтому при равных версиях побеждает первый из них.
fn select_oldest_schema<'a>(
    name: &str,
    candidates: &[(&'a BaseCatalog, &'a Value)],
) -> Option<(&'a BaseCatalog, &'a Value)> {
    let mut best: Option<(&BaseCatalog, &Value)> = None;
    for &(catalog, tool) in candidates {
        let older = match best {
            None => true,
            Some((best_catalog, _)) => {
                match compare_extension_versions(&catalog.version, &best_catalog.version) {
                    Ok(Ordering::Less) => true,
                    Ok(_) => false,
                    Err(e) => {
                        warn!(
                            tool = %name,
                            base = %catalog.name,
                            version = %catalog.version,
                            error = %e,
                            "непригодная версия расширения — база не участвует в выборе схемы"
                        );
                        false
                    }
                }
            }
        };
        if older {
            best = Some((catalog, tool));
        }
    }
    best
}

/// Схемы одного инструмента на разных базах расходятся — пишем в журнал, кто
/// именно предоставляет инструмент и чья схема выбрана.
fn log_schema_divergence(name: &str, candidates: &[(&BaseCatalog, &Value)], source: &BaseCatalog) {
    let Some((_, first)) = candidates.first() else {
        return;
    };
    let schema = |tool: &Value| {
        tool.get("inputSchema")
            .map(Value::to_string)
            .unwrap_or_default()
    };
    let reference = schema(first);
    if candidates.iter().all(|(_, tool)| schema(tool) == reference) {
        return;
    }
    let bases: Vec<String> = candidates
        .iter()
        .map(|(catalog, _)| format!("{} {}", catalog.name, catalog.version))
        .collect();
    warn!(
        tool = %name,
        bases = %bases.join(", "),
        chosen = %source.name,
        version = %source.version,
        "схема инструмента расходится между базами — выбрана самая старая версия расширения"
    );
}

/// Вшить обязательный параметр `base` в `inputSchema` одного инструмента.
///
/// `bases` — только те базы, которые этот инструмент предоставляют: enum и
/// описание содержат их и ничего больше. Остальные параметры и `required`
/// инструмента сохраняются, повторный `base` в `required` не добавляется.
pub fn inject_base(mut tool: Value, bases: &[String]) -> Option<Tool> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<без имени>")
        .to_string();
    if tool.as_object().is_none() {
        warn!(tool = %name, "определение инструмента не объект — пропущено");
        return None;
    }

    let schema = tool
        .as_object_mut()?
        .entry("inputSchema")
        .or_insert_with(|| json!({ "type": "object", "properties": {} }));
    let Some(schema) = schema.as_object_mut() else {
        warn!(tool = %name, "inputSchema инструмента не объект — пропущено");
        return None;
    };

    schema.entry("type").or_insert_with(|| json!("object"));
    {
        let props = schema.entry("properties").or_insert_with(|| json!({}));
        let Some(props) = props.as_object_mut() else {
            warn!(tool = %name, "inputSchema.properties инструмента не объект — пропущено");
            return None;
        };
        props.insert(
            "base".to_string(),
            json!({
                "type": "string",
                "enum": bases,
                "description": format!("Имя базы 1С для запроса. Допустимо: {}.", bases.join(", "))
            }),
        );
    }

    let required = schema.entry("required").or_insert_with(|| json!([]));
    let Some(required) = required.as_array_mut() else {
        warn!(tool = %name, "inputSchema.required инструмента не массив — пропущено");
        return None;
    };
    if !required.iter().any(|item| item.as_str() == Some("base")) {
        required.insert(0, json!("base"));
    }

    match serde_json::from_value::<Tool>(tool) {
        Ok(tool) => Some(tool),
        Err(e) => {
            warn!(tool = %name, error = %e, "не удалось разобрать Tool из ответа 1С");
            None
        }
    }
}

/// Текущее время в UTC по ISO-8601 с суффиксом `Z` — метка `_generated`.
pub fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Путь к файлу состава инструментов.
///
/// Приоритет: непустой `MCP_TOOLS_FILE` → `tools.json` рядом с файлом баз
/// (`MCP_ONEC_BASES_FILE`) → `tools.json` в рабочем каталоге (базы заданы
/// inline через `MCP_ONEC_BASES`). Относительные пути разрешаются от рабочего
/// каталога.
pub fn resolve_tools_path(tools_file: &str, bases_file: &str, current_dir: &Path) -> PathBuf {
    let tools_file = tools_file.trim();
    if !tools_file.is_empty() {
        let path = PathBuf::from(tools_file);
        return if path.is_absolute() {
            path
        } else {
            current_dir.join(path)
        };
    }

    let bases_file = bases_file.trim();
    if !bases_file.is_empty() {
        let path = PathBuf::from(bases_file);
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => return current_dir.join(TOOLS_FILE_NAME),
        };
        let dir = if path.is_absolute() {
            dir
        } else {
            current_dir.join(dir)
        };
        return dir.join(TOOLS_FILE_NAME);
    }

    current_dir.join(TOOLS_FILE_NAME)
}

/// Белый список инструментов из файла состава (`tools_enabled`).
///
/// Отсутствие файла, отсутствие поля или битый JSON — это «списка нет»:
/// `None`, запуск не падает. Битый JSON и нестроковые элементы массива пишутся
/// в журнал предупреждением; сами элементы пропускаются. Значения берутся ровно
/// в том виде, в каком лежат в файле, — при перезаписи секция сохраняется.
pub fn read_tools_enabled(path: &Path) -> Option<Vec<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "файл состава не прочитан — белого списка в нём нет"
            );
            return None;
        }
    };
    let document: Value = match serde_json::from_str(&raw) {
        Ok(document) => document,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "некорректный JSON в файле состава — белого списка нет"
            );
            return None;
        }
    };
    let items = document.get("tools_enabled")?;
    let Some(items) = items.as_array() else {
        warn!(
            path = %path.display(),
            "tools_enabled в файле состава не массив — белого списка нет"
        );
        return None;
    };

    let mut tools = Vec::with_capacity(items.len());
    let mut rejected = 0usize;
    for item in items {
        match item.as_str() {
            Some(name) => tools.push(name.to_string()),
            None => rejected += 1,
        }
    }
    if rejected > 0 {
        warn!(
            path = %path.display(),
            rejected, "в tools_enabled файла состава нестроковые элементы — пропущены"
        );
    }
    Some(tools)
}

/// Записать (или убрать) секцию `tools_enabled` в документе состава.
///
/// `None` — списка не было, ключ в файл не попадает. Значение берётся только из
/// прежнего файла: `MCP_TOOLS_ENABLED` в файл не пишется.
pub fn set_tools_enabled(document: &mut Value, tools_enabled: Option<&[String]>) {
    let Some(map) = document.as_object_mut() else {
        return;
    };
    match tools_enabled {
        Some(tools) => {
            map.insert("tools_enabled".to_string(), json!(tools));
        }
        None => {
            map.remove("tools_enabled");
        }
    }
}

/// Эффективный белый список: непустой `MCP_TOOLS_ENABLED` главнее, иначе список
/// из файла состава. Пусто и там — фильтр выключен.
pub fn effective_tools_enabled(env_tools: &[String], file_tools: Option<&[String]>) -> Vec<String> {
    if !env_tools.is_empty() {
        return env_tools.to_vec();
    }
    file_tools.map(|tools| tools.to_vec()).unwrap_or_default()
}

/// Записать файл состава инструментов. Порядок ключей строгий и задан структурой
/// (serde сохраняет порядок полей): `_generated`, `tools_enabled` (если есть),
/// `common`, `separate`. `serde_json::json!` для этого не годится — его `Map`
/// сортирует ключи по алфавиту.
pub fn write_tools_file(path: &Path, document: &Value) -> Result<(), String> {
    /// Вид файла состава на диске: поля идут в том порядке, в каком объявлены.
    #[derive(serde::Serialize)]
    struct ToolsFileDocument<'a> {
        #[serde(rename = "_generated")]
        generated: &'a Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools_enabled: Option<&'a Value>,
        common: &'a Value,
        separate: &'a Value,
    }

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("не удалось создать каталог {}: {e}", dir.display()))?;
        }
    }
    let generated = document
        .get("_generated")
        .ok_or_else(|| "в документе состава нет _generated".to_string())?;
    let common = document
        .get("common")
        .ok_or_else(|| "в документе состава нет common".to_string())?;
    let separate = document
        .get("separate")
        .ok_or_else(|| "в документе состава нет separate".to_string())?;
    let on_disk = ToolsFileDocument {
        generated,
        tools_enabled: document.get("tools_enabled"),
        common,
        separate,
    };

    let mut text = serde_json::to_string_pretty(&on_disk)
        .map_err(|e| format!("сериализация состава инструментов: {e}"))?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
}

/// Один тик фонового повтора: опубликовать незаписанный состав прошлого тика,
/// затем опросить базы без успешного снимка и при удаче пересобрать состав.
async fn retry_tick(
    server: &RouterServer,
    bases: &HashMap<String, BaseForwarder>,
    order: &[String],
    tools_path: &Path,
    state: &mut CatalogState,
    pending: &mut Option<CatalogSnapshot>,
) {
    // Публикацию откладываем до успешной записи файла.
    if let Some(snapshot) = pending.take() {
        *pending = write_and_publish(server, tools_path, snapshot);
    }

    if state.missing.is_empty() {
        return;
    }

    let targets: Vec<(String, BaseForwarder)> = state
        .missing
        .iter()
        .filter_map(|(name, _)| bases.get(name).map(|fwd| (name.clone(), fwd.clone())))
        .collect();
    if targets.is_empty() {
        return;
    }

    let collected = collect_catalogs(targets).await;
    if collected.catalogs.is_empty() {
        // Причины неудачи уже записаны в журнал при опросе.
        return;
    }
    for (name, catalog) in collected.catalogs {
        info!(
            base = %name,
            version = %catalog.version,
            tools = catalog.tools.len(),
            "база восстановилась — состав пересобирается"
        );
        state.apply_success(catalog);
    }

    let mut snapshot = build_catalog(&state.catalogs, order);
    set_tools_enabled(&mut snapshot.document, state.tools_enabled.as_deref());
    *pending = write_and_publish(server, tools_path, snapshot);
}

/// Записать состав и только после успешной записи опубликовать инструменты
/// вместе с матрицей «инструмент → базы». При ошибке записи снимок
/// возвращается — его публикацию повторяем на следующем тике.
fn write_and_publish(
    server: &RouterServer,
    tools_path: &Path,
    snapshot: CatalogSnapshot,
) -> Option<CatalogSnapshot> {
    match write_tools_file(tools_path, &snapshot.document) {
        Ok(()) => {
            let count = snapshot.tools.len();
            server.replace_snapshot(snapshot.tools, snapshot.tool_bases);
            info!(
                tools = count,
                path = %tools_path.display(),
                "состав инструментов записан и опубликован"
            );
            None
        }
        Err(e) => {
            error!(
                path = %tools_path.display(),
                error = %e,
                "не удалось записать файл состава — публикацию повторю на следующем тике"
            );
            Some(snapshot)
        }
    }
}

/// Фоновый повтор опроса баз без успешного снимка: раз в
/// [`RETRY_PERIOD_SECS`] секунд, не больше `attempts` раз (0 — не повторять).
/// Цикл останавливается досрочно, когда ответили все базы. Восстановленная база
/// сохраняется в состоянии навсегда, состав пересобирается, файл перезаписывается,
/// инструменты и матрица публикуются через [`RouterServer::replace_snapshot`].
/// `listChanged` не заявляем — новый список клиент увидит после переподключения.
pub async fn run_retry_loop(
    server: RouterServer,
    bases: std::sync::Arc<HashMap<String, BaseForwarder>>,
    order: std::sync::Arc<Vec<String>>,
    tools_path: PathBuf,
    mut state: CatalogState,
    attempts: usize,
) {
    if state.missing.is_empty() {
        info!("все базы ответили при старте — повтор опроса не нужен");
        return;
    }
    if attempts == 0 {
        warn!(
            missing = state.missing.len(),
            "повтор опроса выключен настройкой (0) — базы без снимка останутся без повторов"
        );
        return;
    }

    let mut pending: Option<CatalogSnapshot> = None;
    let mut left = attempts;
    loop {
        tokio::time::sleep(Duration::from_secs(RETRY_PERIOD_SECS)).await;
        retry_tick(
            &server,
            &bases,
            &order,
            &tools_path,
            &mut state,
            &mut pending,
        )
        .await;
        if state.missing.is_empty() {
            info!("все базы ответили — повтор опроса остановлен");
            return;
        }
        left -= 1;
        if left == 0 {
            warn!(
                missing = state.missing.len(),
                attempts, "повтор опроса исчерпан — эти базы останутся без снимка"
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BaseEntry;
    use crate::forward::BaseForwarder;
    use crate::server::RouterServer;
    use rmcp::model::CallToolResult;
    use serde_json::{json, Value};
    use std::cmp::Ordering;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::time::Instant;

    fn names_of(value: &Value) -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn raw_tool(name: &str, version: &str) -> Value {
        json!({
            "name": name,
            "description": format!("{name} версии {version}"),
            "inputSchema": {
                "type": "object",
                "properties": { "arg": { "type": "string" } },
                "required": ["arg"]
            }
        })
    }

    fn base_catalog(name: &str, version: &str, tools: &[&str]) -> BaseCatalog {
        BaseCatalog::new(
            name,
            version,
            tools.iter().map(|tool| raw_tool(tool, version)).collect(),
        )
        .expect("снимок базы")
    }

    fn tool_json(snapshot: &CatalogSnapshot, name: &str) -> Value {
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .expect("инструмент в составе");
        serde_json::to_value(tool).expect("сериализация Tool")
    }

    fn enum_of(snapshot: &CatalogSnapshot, name: &str) -> Vec<String> {
        let tool = tool_json(snapshot, name);
        names_of(&tool["inputSchema"]["properties"]["base"]["enum"])
    }

    fn temp_tools_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "router-1c-mcp-catalog-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("временный каталог");
        dir.join(TOOLS_FILE_NAME)
    }

    #[test]
    fn build_common_and_separate() {
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.2.3", &["x", "y"]));
        catalogs.insert("b".to_string(), base_catalog("b", "1.2.4", &["x", "z"]));
        // Третья база (c) не ответила и в состав не попадает.
        let order = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let snapshot = build_catalog(&catalogs, &order);

        assert_eq!(names_of(&snapshot.document["common"]), vec!["x"]);
        assert_eq!(names_of(&snapshot.document["separate"]["a"]), vec!["y"]);
        assert_eq!(names_of(&snapshot.document["separate"]["b"]), vec!["z"]);

        // Узкий enum: у инструмента только его собственные базы.
        assert_eq!(enum_of(&snapshot, "y"), vec!["a"]);
        assert_eq!(enum_of(&snapshot, "x"), vec!["a", "b"]);
        assert_eq!(snapshot.tools.len(), 3);

        // Метка времени — UTC ISO-8601, версии в файл не пишутся.
        let stamp = snapshot.document["_generated"]
            .as_str()
            .expect("метка времени");
        assert!(stamp.ends_with('Z'), "UTC-суффикс: {stamp}");
        chrono::DateTime::parse_from_rfc3339(stamp).expect("UTC ISO-8601");
        let keys: Vec<&String> = snapshot
            .document
            .as_object()
            .expect("документ состава")
            .keys()
            .collect();
        assert_eq!(
            keys.len(),
            3,
            "только _generated, common, separate: {keys:?}"
        );
    }

    #[test]
    fn empty_and_single_base_catalogs() {
        let order = vec!["a".to_string(), "b".to_string()];

        // Ни одна база не ответила: common и separate пусты.
        let none = build_catalog(&HashMap::new(), &order);
        assert!(none.tools.is_empty());
        assert!(none.tools.is_empty());
        assert!(names_of(&none.document["common"]).is_empty());
        assert_eq!(
            none.document["separate"].as_object().map(|map| map.len()),
            Some(0)
        );

        // Старый файл полностью заменяется пустым составом.
        let path = temp_tools_path("empty");
        std::fs::write(
            &path,
            r#"{"_generated":"старое","common":["old"],"separate":{"a":["old"]}}"#,
        )
        .expect("старый файл");
        write_tools_file(&path, &none.document).expect("запись пустого состава");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("чтение файла"))
                .expect("JSON файла состава");
        assert!(names_of(&written["common"]).is_empty());
        assert_eq!(
            written["separate"].as_object().map(|map| map.len()),
            Some(0)
        );
        assert_ne!(written["_generated"], "старое");

        // Одна ответившая база: всё её содержимое в common, separate пуст.
        let mut single = HashMap::new();
        single.insert("a".to_string(), base_catalog("a", "1.0", &["x", "y"]));
        let one = build_catalog(&single, &order);
        assert_eq!(names_of(&one.document["common"]), vec!["x", "y"]);
        assert_eq!(
            one.document["separate"].as_object().map(|map| map.len()),
            Some(0)
        );

        // База ответила пустым tools: она опрошена, но common пуст.
        let mut with_empty = HashMap::new();
        with_empty.insert("a".to_string(), base_catalog("a", "1.0", &["x"]));
        with_empty.insert("b".to_string(), base_catalog("b", "1.0", &[]));
        let mixed = build_catalog(&with_empty, &order);
        assert!(names_of(&mixed.document["common"]).is_empty());
        assert_eq!(names_of(&mixed.document["separate"]["a"]), vec!["x"]);
        assert_eq!(mixed.tools.len(), 1);
    }

    #[test]
    fn inject_per_tool_bases() {
        let raw = json!({
            "name": "execute_query",
            "description": "запрос к базе",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "required": ["query"]
            }
        });
        let tool = inject_base(raw, &["base-a".to_string(), "base-b".to_string()])
            .expect("Tool с параметром base");
        let value = serde_json::to_value(&tool).expect("сериализация Tool");

        let base = &value["inputSchema"]["properties"]["base"];
        assert_eq!(base["type"], "string");
        assert_eq!(names_of(&base["enum"]), vec!["base-a", "base-b"]);
        let description = base["description"].as_str().expect("описание base");
        assert!(description.contains("base-a"));
        assert!(description.contains("base-b"));
        assert!(
            !description.contains("base-c"),
            "в описании только базы этого инструмента"
        );

        // Остальные параметры и required сохранены, base добавлен один раз.
        assert_eq!(
            value["inputSchema"]["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(
            value["inputSchema"]["properties"]["limit"]["type"],
            "number"
        );
        assert_eq!(
            names_of(&value["inputSchema"]["required"]),
            vec!["base", "query"]
        );
        assert_eq!(value["description"], "запрос к базе");

        // Повторный base в required не дублируется.
        let again = json!({
            "name": "execute_query",
            "inputSchema": {
                "type": "object",
                "properties": { "base": { "type": "string" } },
                "required": ["base", "query"]
            }
        });
        let tool = inject_base(again, &["base-a".to_string()]).expect("Tool с параметром base");
        let value = serde_json::to_value(&tool).expect("сериализация Tool");
        assert_eq!(
            names_of(&value["inputSchema"]["required"]),
            vec!["base", "query"]
        );
        assert_eq!(
            names_of(&value["inputSchema"]["properties"]["base"]["enum"]),
            vec!["base-a"]
        );
    }

    #[test]
    fn select_oldest_schema() {
        // Численное сравнение по компонентам: 1.9 старше 1.10.
        assert_eq!(
            compare_extension_versions("1.9", "1.10").expect("версии"),
            Ordering::Less
        );
        // Четвёртый компонент участвует в сравнении.
        assert_eq!(
            compare_extension_versions("1.2.3.4", "1.2.3.5").expect("версии"),
            Ordering::Less
        );
        // 1.2 и 1.2.0.0 равны — решает порядок настроек.
        assert_eq!(
            compare_extension_versions("1.2", "1.2.0.0").expect("версии"),
            Ordering::Equal
        );
        assert!(compare_extension_versions("версия", "1.0").is_err());

        // Порядок настроек: b первой, но её версия 1.10 новее — берём схему a.
        let mut catalogs = HashMap::new();
        catalogs.insert("b".to_string(), base_catalog("b", "1.10", &["x"]));
        catalogs.insert("a".to_string(), base_catalog("a", "1.9", &["x"]));
        let order = vec!["b".to_string(), "a".to_string()];
        let snapshot = build_catalog(&catalogs, &order);
        let tool = tool_json(&snapshot, "x");
        assert!(
            tool["description"]
                .as_str()
                .unwrap_or_default()
                .contains("1.9"),
            "выбрана схема самой старой версии: {tool}"
        );
        assert_eq!(enum_of(&snapshot, "x"), vec!["b", "a"]);

        // Равные версии — побеждает база, идущая раньше в настройках.
        let mut equal = HashMap::new();
        equal.insert("a".to_string(), base_catalog("a", "1.2", &["x"]));
        equal.insert("b".to_string(), base_catalog("b", "1.2.0.0", &["x"]));
        let snapshot = build_catalog(&equal, &order);
        let tool = tool_json(&snapshot, "x");
        assert_eq!(tool["description"], "x версии 1.2.0.0");
    }

    #[test]
    fn resolve_tools_file_path() {
        let cwd = PathBuf::from("router-work");

        // MCP_TOOLS_FILE важнее файла баз, относительный путь — от рабочего каталога.
        assert_eq!(
            resolve_tools_path("conf/tools.json", "bases/bases.json", &cwd),
            cwd.join("conf/tools.json")
        );
        let absolute = std::env::temp_dir().join("router-1c-mcp-abs/tools.json");
        assert_eq!(
            resolve_tools_path(
                absolute.to_str().expect("путь UTF-8"),
                "bases/bases.json",
                &cwd
            ),
            absolute
        );

        // Без MCP_TOOLS_FILE — tools.json рядом с файлом баз.
        let bases = std::env::temp_dir().join("router-1c-mcp-abs/bases.json");
        assert_eq!(
            resolve_tools_path("", bases.to_str().expect("путь UTF-8"), &cwd),
            std::env::temp_dir().join("router-1c-mcp-abs/tools.json")
        );
        assert_eq!(
            resolve_tools_path("   ", "bases/bases.json", &cwd),
            cwd.join("bases/tools.json")
        );

        // Базы заданы inline — tools.json в рабочем каталоге.
        assert_eq!(resolve_tools_path("", "", &cwd), cwd.join(TOOLS_FILE_NAME));
    }

    /// Белый список из файла состава читается как есть; отсутствие файла,
    /// отсутствие поля и битый JSON — «списка нет», а нестроковые элементы
    /// пропускаются с предупреждением.
    #[test]
    fn tools_enabled_read_from_file() {
        let path = temp_tools_path("read-tools");

        std::fs::write(
            &path,
            r#"{"_generated":"2026-10-28T07:06:53Z","tools_enabled":["execute_query","db_tables"],"common":[],"separate":{}}"#,
        )
        .expect("файл состава");
        assert_eq!(
            read_tools_enabled(&path),
            Some(vec!["execute_query".to_string(), "db_tables".to_string()])
        );

        // Поля нет — списка нет.
        std::fs::write(&path, r#"{"_generated":"x","common":[],"separate":{}}"#)
            .expect("файл без списка");
        assert_eq!(read_tools_enabled(&path), None);

        // Битый JSON — списка нет, запуск не падает.
        std::fs::write(&path, "{ это не json").expect("битый файл");
        assert_eq!(read_tools_enabled(&path), None);

        // Файла нет — списка нет.
        let absent = path.with_file_name("нет-такого-файла.json");
        assert_eq!(read_tools_enabled(&absent), None);

        // Нестроковые элементы пропускаются, строковые остаются.
        std::fs::write(&path, r#"{"tools_enabled":["execute_query",42,true]}"#)
            .expect("файл со смешанным списком");
        assert_eq!(
            read_tools_enabled(&path),
            Some(vec!["execute_query".to_string()])
        );

        // tools_enabled не массив — списка нет.
        std::fs::write(&path, r#"{"tools_enabled":"execute_query"}"#).expect("файл со строкой");
        assert_eq!(read_tools_enabled(&path), None);
    }

    /// Перезапись (старт и фоновый повтор) сохраняет секцию без изменений:
    /// файл со списком → чтение → перезапись → в файле снова тот же список.
    #[test]
    fn rewrite_keeps_tools_enabled() {
        let path = temp_tools_path("keep-tools");
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.0", &["x", "y"]));
        let order = vec!["a".to_string()];
        let list = vec!["x".to_string()];

        let mut first = build_catalog(&catalogs, &order).document;
        set_tools_enabled(&mut first, Some(list.as_slice()));
        write_tools_file(&path, &first).expect("первая запись");
        assert_eq!(read_tools_enabled(&path), Some(list.clone()));

        // Второй старт: список прочитан из прежнего файла и сохранён как был.
        let found = read_tools_enabled(&path);
        let mut second = build_catalog(&catalogs, &order).document;
        set_tools_enabled(&mut second, found.as_deref());
        write_tools_file(&path, &second).expect("вторая запись");
        assert_eq!(read_tools_enabled(&path), Some(list));

        // common/separate при этом отражают текущий опрос баз.
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("чтение файла"))
                .expect("JSON файла состава");
        assert_eq!(names_of(&written["common"]), vec!["x", "y"]);
    }

    /// Списка не было — ключ в файл не попадает.
    #[test]
    fn no_tools_list_no_key() {
        let path = temp_tools_path("no-tools-key");
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.0", &["x"]));
        let order = vec!["a".to_string()];

        let no_list: Option<Vec<String>> = None;
        let mut document = build_catalog(&catalogs, &order).document;
        set_tools_enabled(&mut document, no_list.as_deref());
        assert!(document.get("tools_enabled").is_none());

        write_tools_file(&path, &document).expect("запись без списка");
        let text = std::fs::read_to_string(&path).expect("чтение файла");
        assert!(
            !text.contains("tools_enabled"),
            "ключа быть не должно: {text}"
        );
        assert_eq!(read_tools_enabled(&path), None);
    }

    /// Непустая MCP_TOOLS_ENABLED главнее списка из файла состава.
    #[test]
    fn env_tools_override_file_tools() {
        let file = vec!["x".to_string(), "y".to_string()];
        let env = vec!["y".to_string()];

        assert_eq!(
            effective_tools_enabled(&env, Some(file.as_slice())),
            vec!["y"]
        );
        assert_eq!(effective_tools_enabled(&[], Some(file.as_slice())), file);
        assert!(effective_tools_enabled(&[], None).is_empty());
    }

    /// Порядок ключей в записанном файле строгий: `_generated`, `tools_enabled`,
    /// `common`, `separate`. Проверяется по тексту — в разобранном JSON порядок
    /// теряется.
    #[test]
    fn document_key_order() {
        let path = temp_tools_path("key-order");
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.0", &["x", "y"]));
        catalogs.insert("b".to_string(), base_catalog("b", "1.0", &["x", "z"]));
        let order = vec!["a".to_string(), "b".to_string()];

        let list = vec!["x".to_string()];
        let mut document = build_catalog(&catalogs, &order).document;
        set_tools_enabled(&mut document, Some(list.as_slice()));
        write_tools_file(&path, &document).expect("запись состава");
        let text = std::fs::read_to_string(&path).expect("чтение файла");

        fn key_position(text: &str, key: &str) -> usize {
            text.find(&format!("\"{key}\""))
                .unwrap_or_else(|| panic!("нет ключа {key} в файле: {text}"))
        }

        let generated = key_position(&text, "_generated");
        let tools = key_position(&text, "tools_enabled");
        let common = key_position(&text, "common");
        let separate = key_position(&text, "separate");
        assert!(
            generated < tools && tools < common && common < separate,
            "порядок ключей нарушен: {text}"
        );

        // Без списка порядок тот же, только tools_enabled отсутствует.
        let no_list: Option<Vec<String>> = None;
        let mut document = build_catalog(&catalogs, &order).document;
        set_tools_enabled(&mut document, no_list.as_deref());
        write_tools_file(&path, &document).expect("запись состава без списка");
        let text = std::fs::read_to_string(&path).expect("чтение файла");
        let generated = key_position(&text, "_generated");
        let common = key_position(&text, "common");
        let separate = key_position(&text, "separate");
        assert!(
            generated < common && common < separate,
            "порядок ключей нарушен: {text}"
        );
    }

    #[test]
    fn whitelist_remains_global() {
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.0", &["x", "y"]));
        catalogs.insert("b".to_string(), base_catalog("b", "1.0", &["x", "z"]));
        let order = vec!["a".to_string(), "b".to_string()];
        let snapshot = build_catalog(&catalogs, &order);

        let server = RouterServer::new(
            "test",
            HashMap::new(),
            order,
            snapshot.tools.clone(),
            snapshot.tool_bases.clone(),
        )
        .with_allowed_tools(&["x".to_string()]);

        // Выдача сужена белым списком, полный снимок — нет.
        let visible: Vec<String> = server
            .visible_tools()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(visible, vec!["x"]);
        assert_eq!(server.tools_snapshot().len(), 3);

        // Вызов разрешён только белому списку.
        assert!(server.call_allowed("x"));
        assert!(!server.call_allowed("y"));
        assert!(!server.call_allowed("z"));
        assert!(!server.call_allowed("unknown_tool"));

        // Файл состава строится по полному набору, вне белого списка.
        let path = temp_tools_path("whitelist");
        write_tools_file(&path, &snapshot.document).expect("запись состава");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("чтение файла"))
                .expect("JSON файла состава");
        let mut written_names = names_of(&written["common"]);
        written_names.extend(names_of(&written["separate"]["a"]));
        written_names.extend(names_of(&written["separate"]["b"]));
        written_names.sort();
        assert_eq!(written_names, vec!["x", "y", "z"]);
    }

    #[test]
    fn utc_stamp_is_iso8601() {
        let stamp = utc_now_iso8601();
        assert!(stamp.ends_with('Z'), "суффикс Z: {stamp}");
        assert_eq!(stamp.len(), 20, "секунды без долей: {stamp}");
        assert!(chrono::DateTime::parse_from_rfc3339(&stamp).is_ok());
    }

    /// Живые базы отдают версию с суффиксом сборки: «1.8.2-lite». Суффикс не
    /// должен делать базу неответившей — сравнивается числовое ядро.
    #[test]
    fn version_suffix_is_ignored() {
        assert_eq!(
            compare_extension_versions("1.8.2-lite", "1.8.2").expect("сравнение"),
            Ordering::Equal
        );
        assert_eq!(
            compare_extension_versions("1.9.0-lite", "1.8.2").expect("сравнение"),
            Ordering::Greater
        );
        assert_eq!(
            compare_extension_versions("1.7.0-lite", "1.7.0+20261001").expect("сравнение"),
            Ordering::Equal
        );
        assert!(BaseCatalog::new("a", "1.8.2-lite", Vec::new()).is_ok());
        assert!(BaseCatalog::new("a", "-lite", Vec::new()).is_err());
        assert!(BaseCatalog::new("a", "1.x.2", Vec::new()).is_err());
    }

    // ---- заглушка HTTP-сервиса базы ----

    #[derive(Clone)]
    struct MockState {
        version: String,
        tools: Vec<String>,
        delay: Duration,
        broken: Arc<AtomicBool>,
        /// Число запросов к заглушке: тесты проверяют, дошёл ли вызов до сети.
        calls: Arc<AtomicUsize>,
    }

    struct MockBase {
        url: String,
        broken: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    impl MockBase {
        fn set_broken(&self, value: bool) {
            self.broken.store(value, AtomicOrdering::SeqCst);
        }

        fn calls(&self) -> usize {
            self.calls.load(AtomicOrdering::SeqCst)
        }
    }

    async fn mock_rpc(
        axum::extract::State(state): axum::extract::State<MockState>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        state.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if !state.delay.is_zero() {
            tokio::time::sleep(state.delay).await;
        }
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "initialize" => {
                if state.broken.load(AtomicOrdering::SeqCst) {
                    // Ответ без serverInfo.version — как у базы без расширения MCP.
                    json!({ "protocolVersion": "2024-11-05", "capabilities": {} })
                } else {
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-1c", "version": state.version }
                    })
                }
            }
            "tools/list" => json!({
                "tools": state
                    .tools
                    .iter()
                    .map(|name| raw_tool(name, &state.version))
                    .collect::<Vec<Value>>()
            }),
            // Форма, которую роутер разбирает как `CallToolResult`.
            "tools/call" => json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            }),
            _ => json!({}),
        };
        axum::Json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }

    async fn spawn_mock(version: &str, tools: &[&str], delay: Duration) -> MockBase {
        let broken = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            version: version.to_string(),
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
            delay,
            broken: broken.clone(),
            calls: calls.clone(),
        };
        let app = axum::Router::new()
            .route("/hs/mcp/rpc", axum::routing::post(mock_rpc))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("порт заглушки");
        let addr = listener.local_addr().expect("адрес заглушки");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockBase {
            url: format!("http://{addr}"),
            broken,
            calls,
        }
    }

    fn forwarder(name: &str, url: &str) -> BaseForwarder {
        let entry = BaseEntry {
            name: name.to_string(),
            url: url.to_string(),
            username: None,
            password: None,
            service_root: "mcp".to_string(),
            max_concurrent: None,
        };
        BaseForwarder::new(&entry, reqwest::Client::new())
    }

    #[tokio::test]
    async fn collect_all_bases_concurrently() {
        let delay = Duration::from_millis(400);
        let a = spawn_mock("1.2.0", &["x"], delay).await;
        let b = spawn_mock("1.3.0", &["x", "z"], delay).await;
        let c = spawn_mock("1.4.0", &["x"], delay).await;
        let broken = spawn_mock("0.0.0", &["x"], Duration::ZERO).await;
        broken.set_broken(true);

        let mut bases = HashMap::new();
        bases.insert("a".to_string(), forwarder("a", &a.url));
        bases.insert("b".to_string(), forwarder("b", &b.url));
        bases.insert("c".to_string(), forwarder("c", &c.url));
        bases.insert("broken".to_string(), forwarder("broken", &broken.url));
        let order = [
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "broken".to_string(),
        ];
        let targets = order
            .iter()
            .map(|name| (name.clone(), bases[name].clone()))
            .collect();

        let started = Instant::now();
        let collected = collect_catalogs(targets).await;
        let elapsed = started.elapsed();

        // Опрос не останавливается на неудаче и не останавливается на первом успехе.
        assert_eq!(collected.catalogs.len(), 3);
        assert_eq!(collected.failures.len(), 1);
        assert_eq!(collected.failures[0].0, "broken");
        assert!(
            !collected.failures[0].1.is_empty(),
            "причина отказа должна попасть в журнал"
        );
        // Параллельный опрос: каждый снимок — два запроса по 400 мс, все базы
        // сразу. Последовательный опрос дал бы 3 × 800 мс против ~800 мс здесь.
        assert!(
            elapsed < Duration::from_millis(1600),
            "опрос занял {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_recovers_missing_base() {
        let a = spawn_mock("1.2.0", &["x", "y"], Duration::ZERO).await;
        let b = spawn_mock("1.3.0", &["x", "z"], Duration::ZERO).await;
        b.set_broken(true);

        let mut bases = HashMap::new();
        bases.insert("a".to_string(), forwarder("a", &a.url));
        bases.insert("b".to_string(), forwarder("b", &b.url));
        let order = vec!["a".to_string(), "b".to_string()];

        let targets: Vec<(String, BaseForwarder)> = order
            .iter()
            .map(|name| (name.clone(), bases[name].clone()))
            .collect();
        let collected = collect_catalogs(targets).await;
        assert_eq!(collected.catalogs.len(), 1);
        assert_eq!(collected.failures.len(), 1);
        let mut state = CatalogState::new(collected.catalogs, collected.failures);

        let tools_path = temp_tools_path("retry");
        let snapshot = build_catalog(&state.catalogs, &order);
        // Ответила одна база — её инструменты и есть общий состав: база b
        // в пересечении не участвует, пока не ответила.
        assert_eq!(names_of(&snapshot.document["common"]), vec!["x", "y"]);
        write_tools_file(&tools_path, &snapshot.document).expect("стартовая запись");

        let server = RouterServer::new(
            "test",
            HashMap::new(),
            order.clone(),
            snapshot.tools,
            snapshot.tool_bases,
        );
        let shared = server.clone();
        let published: Vec<String> = shared
            .tools_snapshot()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(!published.contains(&"z".to_string()));

        // База b восстановилась — фоновый повтор пересобирает состав.
        b.set_broken(false);
        let mut pending: Option<CatalogSnapshot> = None;
        retry_tick(
            &server,
            &bases,
            &order,
            &tools_path,
            &mut state,
            &mut pending,
        )
        .await;

        assert!(state.missing.is_empty(), "база снята с ожидания");
        assert!(state.catalog("b").is_some(), "снимок базы сохранён");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&tools_path).expect("чтение файла"))
                .expect("JSON файла состава");
        assert_eq!(names_of(&written["common"]), vec!["x"]);
        assert_eq!(names_of(&written["separate"]["a"]), vec!["y"]);
        assert_eq!(names_of(&written["separate"]["b"]), vec!["z"]);

        // Пересобранный состав виден и в клоне сервера — снимок общий.
        let published: Vec<String> = shared
            .tools_snapshot()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(
            published.contains(&"z".to_string()),
            "инструмент восстановленной базы опубликован: {published:?}"
        );
    }

    fn result_json(res: &CallToolResult) -> Value {
        serde_json::to_value(res).expect("сериализация CallToolResult")
    }

    fn err_text(res: &CallToolResult) -> String {
        let value = result_json(res);
        assert_eq!(value["isError"], json!(true), "{value}");
        value["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn call_args(base: &str) -> serde_json::Map<String, Value> {
        let mut args = serde_json::Map::new();
        args.insert("base".to_string(), json!(base));
        args
    }

    /// Матрица «инструмент → базы» держит состав снимка: роутер знает, какая
    /// база какой инструмент предоставляет.
    #[test]
    fn snapshot_carries_tool_bases() {
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.0", &["x", "y"]));
        catalogs.insert("b".to_string(), base_catalog("b", "1.1", &["x", "z"]));
        let order = vec!["a".to_string(), "b".to_string()];
        let snapshot = build_catalog(&catalogs, &order);

        let bases = |name: &str| -> Vec<String> { snapshot.tool_bases[name].clone() };
        assert_eq!(bases("x"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(bases("y"), vec!["a".to_string()]);
        assert_eq!(bases("z"), vec!["b".to_string()]);
    }

    /// Вызов с базой, которой инструмент не предоставляет, отбивается роутером
    /// до сети: заглушки баз не получают ни одного запроса.
    #[tokio::test]
    async fn foreign_base_call_is_rejected_before_network() {
        let a = spawn_mock("1.2.0", &["x"], Duration::ZERO).await;
        let b = spawn_mock("1.3.0", &["x", "y"], Duration::ZERO).await;

        let mut bases = HashMap::new();
        bases.insert("a".to_string(), forwarder("a", &a.url));
        bases.insert("b".to_string(), forwarder("b", &b.url));

        // Матрица собрана по одной базе a: инструмент x предоставляет только она.
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.2.0", &["x"]));
        let order = vec!["a".to_string(), "b".to_string()];
        let snapshot = build_catalog(&catalogs, &order);

        let server = RouterServer::new(
            "test",
            bases,
            order,
            snapshot.tools.clone(),
            snapshot.tool_bases.clone(),
        );

        let res = server.handle_call("x".to_string(), call_args("b")).await;
        assert_eq!(
            err_text(&res),
            "Инструмент 'x' недоступен для базы 'b'. Допустимые базы: [\"a\"]"
        );
        assert_eq!(b.calls(), 0, "вызов не должен дойти до базы b");
        assert_eq!(a.calls(), 0, "вызов не должен дойти до базы a");

        // Имени, которого нет в матрице, роутер тоже не пропускает.
        let res = server.handle_call("zzz".to_string(), call_args("a")).await;
        assert_eq!(
            err_text(&res),
            "Инструмент 'zzz' отсутствует в составе инструментов (допустимые: [\"x\"])"
        );
        assert_eq!(a.calls(), 0, "вызов не должен дойти до базы a");
    }

    /// `replace_snapshot` меняет матрицу вместе со снимком: после публикации
    /// нового состава вызов с этой базой разрешается и доходит до неё.
    #[tokio::test]
    async fn replace_snapshot_updates_matrix() {
        let a = spawn_mock("1.2.0", &["x"], Duration::ZERO).await;
        let b = spawn_mock("1.3.0", &["x"], Duration::ZERO).await;

        let mut bases = HashMap::new();
        bases.insert("a".to_string(), forwarder("a", &a.url));
        bases.insert("b".to_string(), forwarder("b", &b.url));

        let order = vec!["a".to_string(), "b".to_string()];
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.2.0", &["x"]));
        let snapshot = build_catalog(&catalogs, &order);

        let server = RouterServer::new(
            "test",
            bases,
            order.clone(),
            snapshot.tools.clone(),
            snapshot.tool_bases.clone(),
        );

        // До обновления снимка база b инструмент x не предоставляет.
        let res = server.handle_call("x".to_string(), call_args("b")).await;
        assert_eq!(
            err_text(&res),
            "Инструмент 'x' недоступен для базы 'b'. Допустимые базы: [\"a\"]"
        );
        assert_eq!(b.calls(), 0, "отбитый вызов не дошёл до базы b");

        // Пересобранный состав: x теперь отдают обе базы.
        let mut catalogs = HashMap::new();
        catalogs.insert("a".to_string(), base_catalog("a", "1.2.0", &["x"]));
        catalogs.insert("b".to_string(), base_catalog("b", "1.3.0", &["x"]));
        let updated = build_catalog(&catalogs, &order);
        server.replace_snapshot(updated.tools.clone(), updated.tool_bases.clone());

        let res = server.handle_call("x".to_string(), call_args("b")).await;
        assert_ne!(result_json(&res)["isError"], json!(true), "вызов разрешён");
        assert_eq!(b.calls(), 1, "вызов дошёл до базы b");
        assert_eq!(a.calls(), 0, "вызов ушёл именно в выбранную базу");
    }
}
