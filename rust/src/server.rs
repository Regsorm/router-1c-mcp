//! rmcp `ServerHandler` роутера: принимает запросы MCP-клиента, по параметру
//! `base` выбирает базу и форвардит вызов через [`BaseForwarder`].
//!
//! `tools/list` — снимок, собранный при старте по всем ответившим базам
//! (см. [`crate::tool_catalog`]), в схему каждого инструмента вшит обязательный
//! параметр `base` с узким списком баз.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    ErrorData as McpError, RoleServer,
};
use serde_json::{json, Value};

use crate::download;
use crate::forward::BaseForwarder;
use crate::upload;

/// Публикуемый снимок: инструменты и матрица «инструмент → базы» вместе.
/// Хранятся одним значением под одним `ArcSwap`, чтобы клиент не увидел новую
/// выдачу со старой матрицей (и наоборот) между двумя записями.
#[derive(Default)]
pub struct PublishSnapshot {
    pub tools: Vec<Tool>,
    pub tool_bases: BTreeMap<String, Vec<String>>,
}

#[derive(Clone)]
pub struct RouterServer {
    version: String,
    bases: Arc<HashMap<String, BaseForwarder>>,
    base_order: Arc<Vec<String>>,
    /// Снимок инструментов и матрица «инструмент → базы» — одной записью.
    snapshot: Arc<ArcSwap<PublishSnapshot>>,
    /// Белый список инструментов. `None` — фильтр выключен.
    allowed_tools: Arc<Option<BTreeSet<String>>>,
    /// Внешний адрес роутера для ссылок на скачивание. Пусто — подсказку не трогаем.
    download_public_url: Arc<String>,
}

impl RouterServer {
    pub fn new(
        version: &str,
        bases: HashMap<String, BaseForwarder>,
        base_order: Vec<String>,
        tools: Vec<Tool>,
        tool_bases: BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            version: version.to_string(),
            bases: Arc::new(bases),
            base_order: Arc::new(base_order),
            snapshot: Arc::new(ArcSwap::from_pointee(PublishSnapshot { tools, tool_bases })),
            allowed_tools: Arc::new(None),
            download_public_url: Arc::new(String::new()),
        }
    }

    /// Белый список инструментов: клиенту отдаются и разрешаются к вызову
    /// только перечисленные. Пустой список выключает фильтр — так роутер
    /// ведёт себя как до появления этого механизма.
    pub fn with_allowed_tools(mut self, enabled: &[String]) -> Self {
        self.allowed_tools = Arc::new(if enabled.is_empty() {
            None
        } else {
            Some(enabled.iter().cloned().collect())
        });
        self
    }

    /// Внешний адрес роутера, по которому клиент может скачать артефакт
    /// (`GET {url}/download/{base}?kind=..&id=..`). Пустая строка — подсказка
    /// `export_artifact` не переписывается.
    pub fn with_download_public_url(mut self, url: &str) -> Self {
        self.download_public_url = Arc::new(url.to_string());
        self
    }

    /// Заменить снимок инструментов и матрицу «инструмент → базы» атомарно:
    /// фоновый повтор опроса баз публикует через этот метод пересобранный
    /// состав без перезапуска роутера, и обе части остаются согласованными.
    pub fn replace_snapshot(&self, tools: Vec<Tool>, tool_bases: BTreeMap<String, Vec<String>>) {
        self.snapshot
            .store(Arc::new(PublishSnapshot { tools, tool_bases }));
    }

    /// Инструменты, которые видит клиент: снимок сужается белым списком.
    /// Без списка (`None`) снимок отдаётся целиком.
    pub(crate) fn visible_tools(&self) -> Vec<Tool> {
        let mut tools = self.snapshot.load().tools.clone();
        if let Some(allowed) = self.allowed_tools.as_ref().as_ref() {
            tools.retain(|t| allowed.contains(t.name.as_ref()));
        }
        tools
    }

    /// Разрешён ли вызов инструмента белым списком. Проверка нужна до пересылки
    /// в базу: клиент может вызвать инструмент, которого нет в выдаче.
    pub(crate) fn call_allowed(&self, name: &str) -> bool {
        match self.allowed_tools.as_ref().as_ref() {
            Some(allowed) => allowed.contains(name),
            None => true,
        }
    }

    /// Тело `call_tool` без MCP-контекста запроса: проверки и форвардинг.
    /// Порядок проверок: белый список → матрица «инструмент → базы» → форвардинг.
    pub(crate) async fn handle_call(
        &self,
        name: String,
        mut args: serde_json::Map<String, Value>,
    ) -> CallToolResult {
        // Проверка белого списка до пересылки в базу. Дублирует фильтр в
        // `list_tools` намеренно: клиент может вызвать инструмент, которого нет
        // в выдаче — зная имя из системного приглашения, из памяти или из
        // инструкций проекта. Без этой проверки вызов ушёл бы в базу.
        if !self.call_allowed(&name) {
            return err_result(format!(
                "Инструмент '{name}' отключён белым списком MCP_TOOLS_ENABLED"
            ));
        }

        // Параметры скачивания нужны только для подсказки `export_artifact`;
        // из args их не вынимаем — в базу они уходят как обычно.
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let id = args.get("id").and_then(|v| v.as_str()).map(str::to_string);

        let base = match args
            .remove("base")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
        {
            Some(b) => b,
            None => {
                return err_result(format!(
                    "Параметр 'base' обязателен. Допустимые: {:?}",
                    self.base_order.as_ref()
                ));
            }
        };

        // Неизвестная база — сначала про неё: «недоступен для базы, которой нет»
        // сбивает с толку, а список допустимых баз у инструмента тут ни при чём.
        let Some(fwd) = self.bases.get(&base) else {
            return err_result(format!(
                "Неизвестная база '{base}'. Допустимые: {:?}",
                self.base_order.as_ref()
            ));
        };

        // Матрица «инструмент → базы»: чужую базу отбиваем до обращения к базе,
        // иначе вызов ушёл бы в сеть и вернулся невнятной ошибкой платформы.
        {
            let snapshot = self.snapshot.load();
            let tool_bases = &snapshot.tool_bases;
            let Some(available) = tool_bases.get(&name) else {
                let known: Vec<&str> = tool_bases.keys().map(String::as_str).collect();
                return err_result(format!(
                    "Инструмент '{name}' отсутствует в составе инструментов (допустимые: {known:?})"
                ));
            };
            if !available.iter().any(|tool_base| tool_base == &base) {
                return err_result(format!(
                    "Инструмент '{name}' недоступен для базы '{base}'. Допустимые базы: {available:?}"
                ));
            }
        }

        match fwd.call_tool(&name, Value::Object(args)).await {
            Ok(res) => {
                // Подсказки импорта и экспорта ведут клиента через роутер,
                // когда известен его внешний адрес.
                if self.download_public_url.is_empty() {
                    return res;
                }
                if name == "import_artifact" {
                    return upload::patch_import_result(res, &self.download_public_url, &base);
                }
                if name != "export_artifact" {
                    return res;
                }
                match (kind.as_deref(), id.as_deref()) {
                    (Some(kind), Some(id)) => {
                        let url = download::public_download_url(
                            self.download_public_url.as_str(),
                            &base,
                            kind,
                            id,
                        );
                        download::patch_export_result(res, &url)
                    }
                    _ => res,
                }
            }
            Err(e) => err_result(format!("Ошибка выполнения '{name}' (base={base}): {e}")),
        }
    }

    /// Снимок инструментов как есть, без белого списка (для тестов состава).
    #[cfg(test)]
    pub fn tools_snapshot(&self) -> Vec<Tool> {
        self.snapshot.load().tools.clone()
    }
}

impl ServerHandler for RouterServer {
    fn get_info(&self) -> ServerInfo {
        let caps = ServerCapabilities::builder().enable_tools().build();
        ServerInfo::new(caps)
            .with_server_info(Implementation::new("router-1c-mcp", self.version.clone()))
            .with_instructions(
                "MCP-роутер 1С: один процесс на N баз через обязательный параметр `base`. \
                 Форвардит вызовы в опубликованные HTTP-сервисы 1С.",
            )
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self.visible_tools();
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Проверки (белый список, матрица «инструмент → базы») и форвардинг —
        // в `handle_call`.
        Ok(self
            .handle_call(
                params.name.to_string(),
                params.arguments.unwrap_or_default(),
            )
            .await)
    }
}

/// Сформировать `CallToolResult` с текстовой ошибкой (`isError=true`).
/// Через serde — чтобы не зависеть от точных полей структуры rmcp.
fn err_result(msg: String) -> CallToolResult {
    serde_json::from_value(json!({
        "content": [{ "type": "text", "text": msg }],
        "isError": true
    }))
    .expect("валидная форма CallToolResult")
}
