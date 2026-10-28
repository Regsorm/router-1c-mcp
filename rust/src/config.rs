//! Конфигурация router-1c-mcp (многобазовый MCP-прокси).
//!
//! Совместима по переменным окружения со старой Python-версией:
//! `MCP_HOST`, `MCP_PORT`, `MCP_LOG_LEVEL`,
//! `MCP_ONEC_BASES_FILE` (путь к JSON с базами) либо `MCP_ONEC_BASES`
//! (inline-JSON). Список баз — тот же `bases.json`, что монтируется в контейнер.
//!
//! Файл баз — только про подключения: имена, адреса, учётные данные. Белый
//! список инструментов живёт в переменной `MCP_TOOLS_ENABLED` либо в файле
//! состава (`tools_enabled`, см. [`crate::tool_catalog::read_tools_enabled`]);
//! переменная главнее. Поле `tools_enabled` в файле баз больше не читается.
//!
//! Скачивание артефактов: `MCP_DOWNLOAD_PUBLIC_URL` — внешний адрес роутера для
//! ссылок в подсказке (не задана → собирается из `MCP_HOST`/`MCP_PORT`),
//! `MCP_DOWNLOAD_TIMEOUT_SECS` — потолок передачи одного файла (по умолчанию 600).
//!
//! Файл состава инструментов: `MCP_TOOLS_FILE` — явный путь, иначе `tools.json`
//! рядом с файлом баз (`MCP_ONEC_BASES_FILE`) либо в рабочем каталоге, когда
//! базы заданы inline. Относительные пути разрешаются от рабочего каталога
//! (см. [`crate::tool_catalog::resolve_tools_path`]).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::warn;

/// Описание одной базы 1С, которую обслуживает роутер.
#[derive(Debug, Clone, Deserialize)]
pub struct BaseEntry {
    /// Короткое имя базы — значение параметра `base` в tool-call.
    pub name: String,
    /// URL базы 1С, например `http://host/path`.
    pub url: String,
    /// Логин пользователя 1С (Basic Auth).
    #[serde(default)]
    pub username: Option<String>,
    /// Пароль пользователя 1С (Basic Auth).
    #[serde(default)]
    pub password: Option<String>,
    /// Корень опубликованного HTTP-сервиса в 1С (формирует `/hs/<root>/...`).
    #[serde(default = "default_service_root")]
    pub service_root: String,
    /// Потолок одновременных запросов к ЭТОЙ базе. `None` → без лимита.
    /// Реализуется per-base семафором: лишние запросы ждут в очереди.
    #[serde(default)]
    pub max_concurrent: Option<usize>,
}

/// Сколько повторов опроса баз делать, если настройка не задана: 5 по 2 минуты.
pub const DEFAULT_RETRY_ATTEMPTS: usize = 5;

fn default_service_root() -> String {
    "mcp".to_string()
}

/// Настройки роутера.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub log_level: String,
    pub bases: Vec<BaseEntry>,
    /// Белый список инструментов из `MCP_TOOLS_ENABLED`: клиенту отдаются и
    /// разрешаются к вызову только перечисленные. Пустой список — в переменной
    /// списка нет, тогда действует `tools_enabled` из файла состава. Пусто и
    /// там — фильтр выключен, доступно всё, что отдала база.
    pub tools_enabled: Vec<String>,
    /// Внешний адрес роутера для ссылок на скачивание, без хвостового `/`.
    pub download_public_url: String,
    /// `true`, если `MCP_DOWNLOAD_PUBLIC_URL` не задана и адрес собран из host:port.
    pub download_public_url_is_default: bool,
    /// Потолок времени передачи одного артефакта, секунды.
    pub download_timeout_secs: u64,
    /// Сколько раз повторять опрос баз без успешного снимка (0 — не повторять).
    /// Источник: `MCP_RETRY_ATTEMPTS`, иначе поле `retry_attempts` в файле баз,
    /// иначе [`DEFAULT_RETRY_ATTEMPTS`].
    pub retry_attempts: usize,
}

impl Config {
    /// Собрать конфиг из переменных окружения (как контейнер на ВМ).
    pub fn from_env() -> Result<Self> {
        let host = env_or("MCP_HOST", "127.0.0.1");
        let port = std::env::var("MCP_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(8010);
        let log_level = env_or("MCP_LOG_LEVEL", "info");

        let (bases, file_retry_attempts) = load_bases()?;
        if bases.is_empty() {
            bail!("Список баз пуст. Задайте MCP_ONEC_BASES_FILE или MCP_ONEC_BASES.");
        }
        // Имена баз должны быть уникальны — иначе маршрутизация по `base` неоднозначна.
        let mut seen = std::collections::HashSet::new();
        for b in &bases {
            if !seen.insert(b.name.as_str()) {
                bail!("Имена баз должны быть уникальны, повтор: '{}'", b.name);
            }
        }

        // Из файла баз белый список больше не берётся: файл — только подключения.
        // Пустая (или незаданная) переменная означает «здесь списка нет» — тогда
        // действует `tools_enabled` из файла состава (см. `main`).
        let tools_enabled = parse_tools_enabled(&env_or("MCP_TOOLS_ENABLED", ""));
        // Повтор опроса: переменная главнее файла баз, 0 — не повторять.
        let retry_attempts =
            parse_retry_attempts(&env_or("MCP_RETRY_ATTEMPTS", ""), file_retry_attempts);

        // Внешний адрес для ссылок на скачивание: пустая переменная означает
        // «использовать собственный адрес роутера» — так контур без внешнего
        // имени работает без настроек, а в подсказке виден реальный адрес.
        let (download_public_url, download_public_url_is_default) =
            resolve_public_url(&env_or("MCP_DOWNLOAD_PUBLIC_URL", ""), &host, port);
        let download_timeout_secs = std::env::var("MCP_DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(600);

        Ok(Self {
            host,
            port,
            log_level,
            bases,
            tools_enabled,
            download_public_url,
            download_public_url_is_default,
            download_timeout_secs,
            retry_attempts,
        })
    }

    /// Путь к файлу состава инструментов. Вычисление — в
    /// [`crate::tool_catalog::resolve_tools_path`]: непустой `MCP_TOOLS_FILE`,
    /// иначе `tools.json` рядом с файлом баз, иначе `tools.json` в рабочем
    /// каталоге (базы заданы inline).
    pub fn tools_file(&self) -> std::path::PathBuf {
        let tools_file = env_or("MCP_TOOLS_FILE", "");
        let bases_file = std::env::var("MCP_ONEC_BASES_FILE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_default();
        let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        crate::tool_catalog::resolve_tools_path(&tools_file, &bases_file, &current_dir)
    }
}

/// Внешний адрес роутера для ссылок на скачивание.
///
/// Непустое `raw` (без пробелов по краям и без хвостовых `/`) берётся как есть;
/// пустое — собирается из `host:port`, причём `0.0.0.0` меняется на `127.0.0.1`
/// (клиент по ссылке `http://0.0.0.0/...` не пойдёт). Второй элемент результата
/// — `true`, если адрес собран по умолчанию.
fn resolve_public_url(raw: &str, host: &str, port: u16) -> (String, bool) {
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        return (trimmed.trim_end_matches('/').to_string(), false);
    }
    let host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    (format!("http://{host}:{port}"), true)
}

/// Разбирает `MCP_TOOLS_ENABLED` — имена инструментов через запятую.
/// Пустое значение даёт пустой список: фильтр в этом случае не применяется.
fn parse_tools_enabled(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Разбирает `MCP_RETRY_ATTEMPTS`: 0 — не повторять опрос, пусто — настройка не
/// задана. Мусор в переменной даёт предупреждение и значение из файла баз.
fn parse_retry_attempts(raw: &str, from_file: Option<usize>) -> usize {
    let raw = raw.trim();
    if !raw.is_empty() {
        match raw.parse::<usize>() {
            Ok(value) => return value,
            Err(_) => warn!(
                value = raw,
                "MCP_RETRY_ATTEMPTS не число — беру значение из файла баз или умолчание"
            ),
        }
    }
    from_file.unwrap_or(DEFAULT_RETRY_ATTEMPTS)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Содержимое файла баз. Исторически это был просто массив баз, поэтому оба
/// написания читаются: старые `bases.json` менять не нужно. Файл описывает
/// только подключения — прочие поля (в том числе устаревшее `tools_enabled`)
/// игнорируются.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BasesDocument {
    /// Новый вид: `{"bases": [...], "retry_attempts": N}`
    WithWrapper {
        bases: Vec<BaseEntry>,
        /// Сколько раз повторять опрос баз без снимка; `None` — как в настройке.
        #[serde(default)]
        retry_attempts: Option<usize>,
    },
    /// Прежний вид: `[{...}, {...}]`
    OnlyBases(Vec<BaseEntry>),
}

impl BasesDocument {
    fn into_parts(self) -> (Vec<BaseEntry>, Option<usize>) {
        match self {
            Self::WithWrapper {
                bases,
                retry_attempts,
            } => (bases, retry_attempts),
            Self::OnlyBases(bases) => (bases, None),
        }
    }
}

/// Разбирает содержимое файла баз в любом из двух написаний — вместе с числом
/// повторов, если оно задано в обёрнутом виде.
fn parse_bases_document_parts(raw: &str) -> Result<(Vec<BaseEntry>, Option<usize>)> {
    let документ: BasesDocument = serde_json::from_str(raw)?;
    Ok(документ.into_parts())
}

/// Приоритет: `MCP_ONEC_BASES_FILE` (путь к JSON) → `MCP_ONEC_BASES` (inline-JSON).
fn load_bases() -> Result<(Vec<BaseEntry>, Option<usize>)> {
    if let Ok(path) = std::env::var("MCP_ONEC_BASES_FILE") {
        if !path.trim().is_empty() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("не удалось прочитать MCP_ONEC_BASES_FILE: {path}"))?;
            return parse_bases_document_parts(&raw)
                .with_context(|| format!("некорректный JSON в {path}"));
        }
    }
    if let Ok(inline) = std::env::var("MCP_ONEC_BASES") {
        if !inline.trim().is_empty() {
            return parse_bases_document_parts(&inline)
                .context("MCP_ONEC_BASES содержит некорректный JSON");
        }
    }
    Ok((Vec::new(), None))
}

#[cfg(test)]
mod tests {
    use super::{
        parse_bases_document_parts, parse_retry_attempts, parse_tools_enabled, resolve_public_url,
        DEFAULT_RETRY_ATTEMPTS,
    };

    const ОДНА_БАЗА: &str = r#"{"name": "base-a", "url": "http://host/base-a"}"#;

    #[test]
    fn повторы_берутся_из_файла_баз() {
        let сырой = format!(r#"{{"retry_attempts": 3, "bases": [{ОДНА_БАЗА}]}}"#);
        let (базы, повторы) = parse_bases_document_parts(&сырой).unwrap();
        assert_eq!(базы.len(), 1);
        assert_eq!(повторы, Some(3));
    }

    #[test]
    fn массив_баз_повторов_не_задаёт() {
        let (базы, повторы) = parse_bases_document_parts(&format!("[{ОДНА_БАЗА}]")).unwrap();
        assert_eq!(базы.len(), 1);
        assert_eq!(повторы, None);
    }

    #[test]
    fn повторы_переменная_главнее_файла_и_умолчание() {
        assert_eq!(parse_retry_attempts("", Some(2)), 2);
        assert_eq!(parse_retry_attempts("   ", None), DEFAULT_RETRY_ATTEMPTS);
        assert_eq!(parse_retry_attempts("0", Some(4)), 0);
        assert_eq!(parse_retry_attempts("7", Some(2)), 7);
        assert_eq!(parse_retry_attempts("много", Some(2)), 2);
        assert_eq!(parse_retry_attempts("много", None), DEFAULT_RETRY_ATTEMPTS);
    }

    #[test]
    fn старый_файл_баз_читается_как_прежде() {
        let (базы, _) = parse_bases_document_parts(&format!("[{ОДНА_БАЗА}]")).unwrap();
        assert_eq!(базы.len(), 1);
        assert_eq!(базы[0].name, "base-a");
    }

    #[test]
    fn обёрнутый_файл_баз_читается() {
        let (базы, _) =
            parse_bases_document_parts(&format!(r#"{{"bases": [{ОДНА_БАЗА}]}}"#)).unwrap();
        assert_eq!(базы.len(), 1);
        assert_eq!(базы[0].name, "base-a");
    }

    /// Белый список из файла баз больше не поддерживается: файл — только
    /// подключения, а устаревшее поле не мешает прочитать базы.
    #[test]
    fn tools_enabled_в_файле_баз_игнорируется() {
        let сырой = format!(
            r#"{{"tools_enabled": [" execute_query ", "db_tables", "", "  "], "bases": [{ОДНА_БАЗА}]}}"#
        );
        let (базы, _) = parse_bases_document_parts(&сырой).unwrap();
        assert_eq!(базы.len(), 1);
        assert_eq!(базы[0].name, "base-a");
    }

    #[test]
    fn пустое_значение_даёт_выключенный_фильтр() {
        assert!(parse_tools_enabled("").is_empty());
        assert!(parse_tools_enabled("   ").is_empty());
        assert!(parse_tools_enabled(" , ,, ").is_empty());
    }

    #[test]
    fn имена_разбираются_и_очищаются_от_пробелов() {
        assert_eq!(
            parse_tools_enabled(" execute_query , db_tables,, list_artifacts "),
            vec!["execute_query", "db_tables", "list_artifacts"]
        );
    }

    #[test]
    fn внешний_адрес_обрезается_по_краям() {
        let (url, по_умолчанию) =
            resolve_public_url("  http://router.local:8014/ ", "127.0.0.1", 8014);
        assert_eq!(url, "http://router.local:8014");
        assert!(
            !по_умолчанию,
            "переменная задана — это не адрес по умолчанию"
        );
    }

    #[test]
    fn пустое_значение_даёт_адрес_по_умолчанию() {
        let (url, по_умолчанию) = resolve_public_url("", "router.local", 8014);
        assert_eq!(url, "http://router.local:8014");
        assert!(по_умолчанию);
    }

    #[test]
    fn wildcard_меняется_на_localhost() {
        let (url, по_умолчанию) = resolve_public_url("   ", "0.0.0.0", 8010);
        assert_eq!(url, "http://127.0.0.1:8010");
        assert!(по_умолчанию);
    }
}
