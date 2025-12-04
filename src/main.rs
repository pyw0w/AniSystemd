use anyhow::Result;
use anicore::Bot;
use notify::{Watcher, RecommendedWatcher, RecursiveMode, Event as NotifyEvent, EventKind};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{info, error, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Загрузка переменных окружения из .env файла
    dotenv::dotenv().ok();

    // Инициализация логгера (он в anicore)
    anicore::init_logger();

    info!("AniSystemd starting...");

    // Создание канала для уведомления об изменении плагинов
    let plugin_changed = Arc::new(Notify::new());
    let plugin_changed_clone = plugin_changed.clone();

    // Запуск мониторинга плагинов
    let plugins_dir = "./plugins";
    start_plugin_watcher(plugins_dir, plugin_changed_clone).await?;

    // Создание и запуск бота
    let bot = Bot::new().await?;
    
    // Отправка READY уведомления systemd после успешной инициализации
    if let Err(e) = libsystemd::daemon::notify(false, &[libsystemd::daemon::NotifyState::Ready]) {
        warn!("Failed to send systemd READY notification: {}", e);
    } else {
        info!("Sent systemd READY notification");
    }

    // Запуск watchdog в фоне
    let watchdog_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            if let Err(e) = libsystemd::daemon::notify(false, &[libsystemd::daemon::NotifyState::Watchdog]) {
                warn!("Failed to send systemd WATCHDOG notification: {}", e);
            }
        }
    });

    // Флаг для отслеживания изменений плагинов
    let should_restart = Arc::new(tokio::sync::Mutex::new(false));
    let should_restart_clone = should_restart.clone();
    let plugin_changed_monitor = plugin_changed.clone();

    // Мониторинг изменений плагинов в фоне
    tokio::spawn(async move {
        plugin_changed_monitor.notified().await;
        info!("Plugin change detected, will trigger restart after bot shutdown...");
        *should_restart_clone.lock().await = true;
    });

    // Запуск бота (блокирующий вызов)
    // При обнаружении изменений плагинов мы завершим процесс после остановки бота
    let bot_result = tokio::select! {
        result = bot.start() => {
            result
        }
        _ = plugin_changed.notified() => {
            // Изменение плагинов обнаружено
            // Bot::start() обрабатывает ctrl_c, но мы не можем отправить его напрямую
            // Поэтому просто завершаем с кодом 0 для перезапуска systemd
            info!("Plugin change detected, exiting for systemd restart...");
            // Даем небольшое время на логирование
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            // Устанавливаем флаг для перезапуска
            *should_restart.lock().await = true;
            Ok(())
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received, shutting down...");
            Ok(())
        }
    };

    // Остановка watchdog
    watchdog_handle.abort();

    // Проверяем результат работы бота
    if let Err(e) = bot_result {
        error!("Bot error: {:?}", e);
        return Err(e.into());
    }

    // Проверяем, было ли обнаружено изменение плагинов
    let should_restart_flag = *should_restart.lock().await;

    info!("AniSystemd stopping...");
    
    // Если обнаружено изменение плагинов, выходим с кодом 0 для перезапуска systemd
    if should_restart_flag {
        info!("Exiting with code 0 for systemd restart...");
        std::process::exit(0);
    }
    
    Ok(())
}

/// Запуск мониторинга директории плагинов
async fn start_plugin_watcher(plugins_dir: &str, notify: Arc<Notify>) -> Result<()> {
    let plugins_path = Path::new(plugins_dir);
    
    // Создать директорию если не существует
    if !plugins_path.exists() {
        tokio::fs::create_dir_all(plugins_path).await?;
        info!("Created plugins directory: {:?}", plugins_path);
    }

    let notify_clone = notify.clone();
    let plugins_dir_path = plugins_path.to_path_buf();

    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res: std::result::Result<NotifyEvent, notify::Error>| {
        match res {
            Ok(event) => {
                // Проверяем, что это событие связано с плагинами или сервисами
                let is_plugin_or_service = event.paths.iter().any(|path| {
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        file_name.ends_with("_plugin.so") 
                            || file_name.ends_with("_plugin.dll")
                            || file_name.ends_with("_plugin.dylib")
                            || file_name.ends_with("_service.so")
                            || file_name.ends_with("_service.dll")
                            || file_name.ends_with("_service.dylib")
                    } else {
                        false
                    }
                });

                if is_plugin_or_service {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                            info!("Plugin/Service change detected: {:?}", event.paths);
                            notify_clone.notify_one();
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!("Plugin watcher error: {:?}", e);
            }
        }
    }).map_err(|e| anyhow::anyhow!("Failed to create plugin watcher: {}", e))?;

    watcher.watch(&plugins_dir_path, RecursiveMode::NonRecursive)
        .map_err(|e| anyhow::anyhow!("Failed to watch plugins directory: {}", e))?;

    info!("Started monitoring plugins directory: {:?}", plugins_dir_path);

    // Сохранить watcher в Arc и запустить в фоне
    let watcher_arc = Arc::new(std::sync::Mutex::new(Some(watcher)));
    let watcher_clone = watcher_arc.clone();
    
    tokio::spawn(async move {
        // Watcher будет работать пока существует
        let _watcher = watcher_clone;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        }
    });

    Ok(())
}

