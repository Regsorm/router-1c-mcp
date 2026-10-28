//! router-1c-mcp (Rust): многобазовый MCP-прокси к опубликованным HTTP-сервисам 1С.
//!
//! Принимает запросы MCP-клиентов на `/mcp` (rmcp StreamableHTTP, stateless),
//! по обязательному параметру `base` выбирает базу и форвардит JSON-RPC в её
//! HTTP-сервис 1С. `tools/list` снимается при старте со всех ответивших баз,
//! объединяется по имени и обогащается параметром `base` с узким списком баз
//! (см. [`tool_catalog`]). Служебный `GET /health` без MCP-handshake.
//! Скачивание артефактов — `GET /download/{base}?kind=..&id=..` (белым списком
//! не закрывается: ссылку клиент получает из подсказки `export_artifact`).
//! Загрузка артефактов — `POST /upload/{base}` (белым списком не закрывается:
//! ссылку клиент получает из подсказки `import_artifact`).

mod config;
mod download;
mod forward;
mod pidlock;
mod server;
mod tool_catalog;
mod upload;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{extract::State, routing::get, Json, Router};
use rmcp::transport::streamable_http_server::{
    session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde_json::{json, Value};
use tracing::{error, info, warn};

use config::Config;
use forward::BaseForwarder;
use server::RouterServer;
use tool_catalog::CatalogState;

#[derive(Clone)]
struct HealthState {
    version: &'static str,
    bases: usize,
}

async fn health(State(s): State<HealthState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "router-1c-mcp",
        "version": s.version,
        "bases": s.bases
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env().context("загрузка конфигурации")?;
    init_tracing(&cfg.log_level);
    info!(
        host = %cfg.host,
        port = cfg.port,
        bases = cfg.bases.len(),
        "запуск router-1c-mcp (rust)"
    );

    // Опциональный singleton PID-lock: активен только если задан MCP_PID_FILE
    // (Windows под supervisor). В Docker переменной нет — lock пропускается.
    let pid_path = std::env::var("MCP_PID_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let _pidlock = pidlock::PidLock::acquire_optional(pid_path, "router-1c-mcp (rust)")
        .context("захват PID-lock")?;

    // Общий reqwest-клиент на все базы (внутри Arc, конкурентен по пулу соединений).
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .context("создание reqwest::Client")?;

    // Forward-клиенты по базам и порядок настроек: он же решает спор версий.
    let mut bases: HashMap<String, BaseForwarder> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for entry in &cfg.bases {
        let fwd = BaseForwarder::new(entry, http.clone());
        order.push(entry.name.clone());
        bases.insert(entry.name.clone(), fwd);
        info!(
            base = %entry.name,
            url = %entry.url,
            max_concurrent = ?entry.max_concurrent,
            "база подключена"
        );
    }
    let bases = Arc::new(bases);
    let order = Arc::new(order);

    // Состав инструментов: параллельный опрос всех баз (initialize + tools/list),
    // сборка общего набора и файл состава — до создания RouterServer.
    let targets: Vec<(String, BaseForwarder)> = order
        .iter()
        .filter_map(|name| bases.get(name).map(|fwd| (name.clone(), fwd.clone())))
        .collect();
    let collected = tool_catalog::collect_catalogs(targets).await;

    // Белый список из прежнего файла состава читается до первой записи: список
    // из переменной окружения в файл не попадает, а прежний сохраняется. Тот же
    // список остаётся в состоянии — фоновый повтор пишет его как был.
    let tools_path = cfg.tools_file();
    let file_tools = tool_catalog::read_tools_enabled(&tools_path);
    let state = CatalogState::new(collected.catalogs, collected.failures)
        .with_tools_enabled(file_tools.clone());

    // Отчёт по каждой базе: версия и количество инструментов либо причина отказа.
    for name in order.iter() {
        match state.catalog(name) {
            Some(catalog) => info!(
                base = %name,
                version = %catalog.version,
                tools = catalog.tools.len(),
                "база в составе"
            ),
            None => error!(
                base = %name,
                reason = %state.failure_reason(name).unwrap_or("причина неизвестна"),
                "база не ответила"
            ),
        }
    }

    // Матрица и файл строятся по полному составу, ещё до белого списка.
    let mut catalog = tool_catalog::build_catalog(&state.catalogs, &order);
    tool_catalog::set_tools_enabled(&mut catalog.document, file_tools.as_deref());
    let tools = catalog.tools.clone();
    let tool_bases = catalog.tool_bases.clone();
    if let Err(e) = tool_catalog::write_tools_file(&tools_path, &catalog.document) {
        return Err(anyhow::anyhow!(
            "не удалось записать файл состава инструментов {}: {e}",
            tools_path.display()
        ));
    }
    info!(
        path = %tools_path.display(),
        tools = tools.len(),
        "файл состава инструментов записан"
    );

    // Эффективный белый список: непустая MCP_TOOLS_ENABLED главнее, иначе —
    // tools_enabled из файла состава. На common/separate он не влияет.
    let tools_enabled =
        tool_catalog::effective_tools_enabled(&cfg.tools_enabled, file_tools.as_deref());

    // Имена сверяем с объединением инструментов всех ответивших баз: если ни
    // одна из них не отдала что-то из списка, пишем об этом в журнал — сразу
    // видно, докатилось ли обновление расширения до баз.
    if tools_enabled.is_empty() {
        info!("белый список пуст — выключен, доступны все инструменты базы");
    } else {
        if !cfg.tools_enabled.is_empty() {
            info!("белый список из MCP_TOOLS_ENABLED главнее списка в файле состава");
        } else if let Some(from_file) = &file_tools {
            info!(
                listed = from_file.len(),
                "белый список взят из tools_enabled файла состава"
            );
        }
        let snapshot: std::collections::HashSet<&str> =
            tools.iter().map(|t| t.name.as_ref()).collect();
        let missing: Vec<&str> = tools_enabled
            .iter()
            .map(String::as_str)
            .filter(|n| !snapshot.contains(n))
            .collect();
        if !missing.is_empty() {
            warn!(
                ?missing,
                "инструменты из белого списка отсутствуют в снимке базы"
            );
        }
        info!(
            listed = tools_enabled.len(),
            available = tools_enabled.len() - missing.len(),
            "белый список инструментов активен"
        );
    }

    // Скачивание артефактов. `BaseForwarder` клонируется вместе с `Arc<Semaphore>`,
    // поэтому лимит `max_concurrent` у MCP-вызовов и у скачиваний один.
    if cfg.download_public_url_is_default {
        warn!(
            public_url = %cfg.download_public_url,
            "MCP_DOWNLOAD_PUBLIC_URL не задана — в подсказке export_artifact будет собственный адрес роутера"
        );
    } else {
        info!(
            public_url = %cfg.download_public_url,
            timeout_secs = cfg.download_timeout_secs,
            "ссылки на скачивание артефактов"
        );
    }
    let download_state = download::DownloadState::new(
        bases.clone(),
        order.clone(),
        Duration::from_secs(cfg.download_timeout_secs),
    );

    let router = RouterServer::new(
        env!("CARGO_PKG_VERSION"),
        (*bases).clone(),
        (*order).clone(),
        tools,
        tool_bases,
    )
    .with_allowed_tools(&tools_enabled)
    .with_download_public_url(&cfg.download_public_url);

    // Фоновый повтор опроса баз без успешного снимка: первый — через две минуты,
    // дальше не больше `cfg.retry_attempts` раз. Успешные снимки не удаляются,
    // восстановленная база добавляется в состав, файл перезаписывается,
    // инструменты и матрица публикуются через `replace_snapshot`.
    let retry_task = tokio::spawn(tool_catalog::run_retry_loop(
        router.clone(),
        bases.clone(),
        order.clone(),
        tools_path,
        state,
        cfg.retry_attempts,
    ));

    // rmcp StreamableHTTP, stateless: NeverSessionManager + with_stateful_mode(false)
    // — игнорируем Mcp-Session-Id, нет 404 «Session not found» после рестарта.
    // json_response(true) — отдаём application/json (нет server-initiated notifications).
    let session_manager = Arc::new(NeverSessionManager::default());
    let svc_factory = {
        let r = router.clone();
        move || Ok(r.clone())
    };
    let http_config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true);
    let mcp_service = StreamableHttpService::new(svc_factory, session_manager, http_config);

    let health_state = HealthState {
        version: env!("CARGO_PKG_VERSION"),
        bases: cfg.bases.len(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .with_state(health_state)
        .merge(download::routes(download_state.clone()))
        .merge(upload::routes(download_state))
        .nest_service("/mcp", mcp_service);

    let bind = format!("{}:{}", cfg.host, cfg.port);
    info!(bind = %bind, "слушаю");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("не удалось забиндиться на {bind}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum::serve упал")?;
    // Фоновая задача повтора не должна пережить сервер.
    retry_task.abort();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("получен Ctrl+C — завершаюсь");
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
