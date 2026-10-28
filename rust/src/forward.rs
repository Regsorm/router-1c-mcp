//! Forward-клиент к опубликованному HTTP-сервису 1С.
//!
//! Один [`BaseForwarder`] на базу. Сам инструменты не реализует — форвардит
//! JSON-RPC 2.0 в `{url}/hs/{service_root}/rpc` (методы `tools/list`,
//! `tools/call`). Авторизация наружу: Basic Auth (username/password базы).
//!
//! Конкурентность: общий `reqwest::Client` (внутри `Arc`, пул соединений) —
//! несколько запросов к одной базе идут параллельно. Если у базы задан
//! `max_concurrent`, перед запросом берётся пермит per-base семафора; лишние
//! ждут в очереди. Без `max_concurrent` лимита нет.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::BaseEntry;

/// Версия MCP-протокола, которую роутер заявляет базе в `initialize`.
/// Значение важно только как корректный `protocolVersion`: от 1С нужен лишь
/// `serverInfo.version` — версия расширения, по ней выбирается схема инструмента.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Базовый адрес HTTP-сервиса базы: `{url}/hs/{service_root}` без хвостовых
/// слешей. Используется и для JSON-RPC (`/rpc`), и для скачивания артефактов.
pub fn service_base(url: &str, service_root: &str) -> String {
    let base = url.trim_end_matches('/');
    let root = service_root.trim_matches('/');
    format!("{base}/hs/{root}")
}

#[derive(Clone)]
pub struct BaseForwarder {
    name: String,
    /// Корень HTTP-сервиса базы без хвостового слеша — нужен для скачивания.
    service_base: String,
    rpc_url: String,
    health_url: String,
    username: String,
    password: String,
    client: Client,
    /// per-base потолок конкурентности; `None` → без лимита.
    sem: Option<Arc<Semaphore>>,
}

impl BaseForwarder {
    pub fn new(entry: &BaseEntry, client: Client) -> Self {
        let service_base = service_base(&entry.url, &entry.service_root);
        Self {
            name: entry.name.clone(),
            rpc_url: format!("{service_base}/rpc"),
            health_url: format!("{service_base}/health"),
            service_base,
            username: entry.username.clone().unwrap_or_default(),
            password: entry.password.clone().unwrap_or_default(),
            client,
            sem: entry
                .max_concurrent
                .map(|n| Arc::new(Semaphore::new(n.max(1)))),
        }
    }

    /// Пермит per-base семафора (если он задан). Владелец пермита занимает
    /// слот `max_concurrent` — держать его надо, пока идёт запрос (а для
    /// скачивания — пока не допит поток тела).
    pub async fn acquire(&self) -> Result<Option<OwnedSemaphorePermit>, String> {
        match &self.sem {
            Some(s) => {
                Ok(Some(s.clone().acquire_owned().await.map_err(|e| {
                    format!("семафор базы {} закрыт: {e}", self.name)
                })?))
            }
            None => Ok(None),
        }
    }

    /// GET-запрос на скачивание артефакта через HTTP-сервис базы. Авторизация
    /// та же, что у `call_rpc`: Basic Auth. Таймаут ставится на
    /// уровне запроса и перекрывает общие 30 с клиента, поэтому отдельный
    /// `reqwest::Client` не нужен.
    pub fn download_request(
        &self,
        kind: &str,
        id: &str,
        timeout: Duration,
    ) -> reqwest::RequestBuilder {
        let url = crate::download::forward_url(&self.service_base, kind, id);
        self.client
            .get(url)
            .basic_auth(&self.username, Some(&self.password))
            .timeout(timeout)
    }

    /// POST-запрос на загрузку артефакта через HTTP-сервис базы. Авторизация
    /// та же, что у `call_rpc`: Basic Auth. Таймаут ставится на
    /// уровне запроса и перекрывает общие 30 с клиента.
    pub fn upload_request(
        &self,
        target: &crate::upload::UploadTarget<'_>,
        timeout: Duration,
    ) -> reqwest::RequestBuilder {
        let url = crate::upload::forward_url(&self.service_base, target);
        self.client
            .post(url)
            .basic_auth(&self.username, Some(&self.password))
            .timeout(timeout)
    }

    /// JSON-RPC вызов к базе. Держит пермит семафора (если задан) на всё время запроса.
    async fn call_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let _permit = self.acquire().await?;

        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self
            .client
            .post(&self.rpc_url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP к базе {}: {e}", self.name))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("база {} вернула HTTP {status}", self.name));
        }

        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("разбор JSON от базы {}: {e}", self.name))?;
        if let Some(err) = value.get("error") {
            return Err(format!("JSON-RPC ошибка от базы {}: {err}", self.name));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `initialize` → версия расширения (`serverInfo.version`) с этой базы.
    /// Авторизация, HTTP-таймауты и per-base семафор — те же, что у `call_rpc`:
    /// запрос идёт через [`BaseForwarder::call_rpc`].
    pub async fn initialize_version(&self) -> Result<String, String> {
        let result = self
            .call_rpc(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "router-1c-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        let version = result
            .get("serverInfo")
            .and_then(|info| info.get("version"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match version {
            Some(v) => Ok(v.to_string()),
            None => Err(format!(
                "база {} вернула ответ initialize без serverInfo.version",
                self.name
            )),
        }
    }

    /// Сырой `tools/list` (массив объектов инструментов) с этой базы.
    ///
    /// Отсутствие или неверный тип поля `tools` — ошибка: молча пустой массив
    /// означал бы «инструментов нет» и выкинул бы базу из состава. Настоящий
    /// пустой массив остаётся успешным ответом.
    pub async fn list_tools_raw(&self) -> Result<Vec<Value>, String> {
        let result = self.call_rpc("tools/list", json!({})).await?;
        match result.get("tools") {
            Some(Value::Array(tools)) => Ok(tools.clone()),
            Some(_) => Err(format!(
                "база {} вернула неверное поле tools в ответе tools/list (ожидался массив)",
                self.name
            )),
            None => Err(format!(
                "база {} не вернула поле tools в ответе tools/list",
                self.name
            )),
        }
    }

    /// `tools/call` → готовый rmcp `CallToolResult` (1С возвращает совместимую форму).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<CallToolResult, String> {
        let result = self
            .call_rpc(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        serde_json::from_value::<CallToolResult>(result)
            .map_err(|e| format!("разбор CallToolResult от базы {}: {e}", self.name))
    }

    /// Проверка здоровья базы (`GET .../health` → `{"status":"ok"}`).
    #[allow(dead_code)]
    pub async fn health(&self) -> bool {
        let rb = self
            .client
            .get(&self.health_url)
            .basic_auth(&self.username, Some(&self.password));
        match rb.send().await {
            Ok(r) => r
                .json::<Value>()
                .await
                .ok()
                .and_then(|j| j.get("status").and_then(|s| s.as_str()).map(|s| s == "ok"))
                .unwrap_or(false),
            Err(_) => false,
        }
    }
}
