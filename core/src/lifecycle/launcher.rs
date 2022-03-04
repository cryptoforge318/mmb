use crate::balance_manager::balance_manager::BalanceManager;
use crate::config::{load_pretty_settings, try_load_settings};
use crate::exchanges::common::{ExchangeAccountId, ExchangeId};
use crate::exchanges::events::{ExchangeEvent, ExchangeEvents, CHANNEL_MAX_EVENTS_COUNT};
use crate::exchanges::general::currency_pair_to_symbol_converter::CurrencyPairToSymbolConverter;
use crate::exchanges::general::exchange::Exchange;
use crate::exchanges::general::exchange_creation::create_exchange;
use crate::exchanges::general::exchange_creation::create_timeout_manager;
use crate::exchanges::internal_events_loop::InternalEventsLoop;
use crate::exchanges::timeouts::timeout_manager::TimeoutManager;
use crate::exchanges::traits::ExchangeClientBuilder;
use crate::infrastructure::init_lifetime_manager;
use crate::lifecycle::app_lifetime_manager::AppLifetimeManager;
use crate::lifecycle::trading_engine::{EngineContext, TradingEngine};
use crate::order_book::local_snapshot_service::LocalSnapshotsService;
use crate::rpc::config_waiter::ConfigWaiter;
use crate::rpc::core_api::CoreApi;
use crate::settings::{AppSettings, BaseStrategySettings, CoreSettings};
use crate::statistic_service::StatisticEventHandler;
use crate::statistic_service::StatisticService;
use crate::strategies::disposition_strategy::DispositionStrategy;
use crate::{
    disposition_execution::executor::DispositionExecutorService, infrastructure::spawn_future,
};
use anyhow::{anyhow, Result};
use core::fmt::Debug;
use dashmap::DashMap;
use futures::{future::join_all, FutureExt};
use mmb_utils::infrastructure::{init_infrastructure, SpawnFutureFlags};
use mmb_utils::logger::print_info;
use mmb_utils::{hashmap, nothing_to_do};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::convert::identity;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::app_lifetime_manager::ActionAfterGracefulShutdown;

pub struct EngineBuildConfig {
    pub supported_exchange_clients: HashMap<ExchangeId, Box<dyn ExchangeClientBuilder + 'static>>,
}

impl EngineBuildConfig {
    pub fn standard(client_builder: Box<dyn ExchangeClientBuilder>) -> Self {
        let exchange_name = "Binance".into();
        let supported_exchange_clients = hashmap![exchange_name => client_builder];

        EngineBuildConfig {
            supported_exchange_clients,
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum InitSettings<StrategySettings>
where
    StrategySettings: BaseStrategySettings + Clone,
{
    Directly(AppSettings<StrategySettings>),
    Load {
        config_path: String,
        credentials_path: String,
    },
}

pub async fn load_settings_or_wait<StrategySettings>(
    config_path: &str,
    credentials_path: &str,
) -> Option<AppSettings<StrategySettings>>
where
    StrategySettings: BaseStrategySettings + Clone + Debug + DeserializeOwned + Serialize,
{
    let (wait_config_tx, mut wait_config_rx) = mpsc::channel::<()>(10);

    let wait_for_config = ConfigWaiter::create_and_start(wait_config_tx)
        .expect("Failed to start RPC server to waiting for config");

    let mut work_finished_receiver = wait_for_config
        .work_finished_receiver
        .lock()
        .take()
        .expect("work_finished_receiver is None");

    loop {
        if work_finished_receiver.try_recv().is_ok() {
            return None;
        }

        match try_load_settings::<StrategySettings>(&config_path, &credentials_path) {
            Ok(settings) => {
                wait_for_config.stop_server();

                tokio::select! {
                    _ = work_finished_receiver => nothing_to_do(),
                    _ = tokio::time::sleep(Duration::from_secs(3)) => log::warn!("Failed to receive stop signal from ConfigWaiter"),
                };
                return Some(settings);
            }
            Err(error) => {
                log::trace!("Failed to load settings: {:?}", error);
                wait_config_rx.recv().await;
            }
        }
    }
}

async fn before_engine_context_init<StrategySettings>(
    build_settings: &EngineBuildConfig,
    init_user_settings: InitSettings<StrategySettings>,
) -> Result<
    Option<(
        broadcast::Sender<ExchangeEvent>,
        broadcast::Receiver<ExchangeEvent>,
        AppSettings<StrategySettings>,
        DashMap<ExchangeAccountId, Arc<Exchange>>,
        Arc<EngineContext>,
        oneshot::Receiver<ActionAfterGracefulShutdown>,
    )>,
>
where
    StrategySettings: BaseStrategySettings + Clone + Debug + DeserializeOwned + Serialize,
{
    init_infrastructure("log.txt");

    log::info!("*****************************");
    log::info!("TradingEngine starting");

    let lifetime_manager = init_lifetime_manager();

    let settings = match init_user_settings {
        InitSettings::Directly(v) => v,
        InitSettings::Load {
            config_path,
            credentials_path,
        } => {
            match load_settings_or_wait::<StrategySettings>(&config_path, &credentials_path).await {
                Some(settings) => settings,
                None => return Ok(None),
            }
        }
    };

    let (events_sender, events_receiver) = broadcast::channel(CHANNEL_MAX_EVENTS_COUNT);

    let timeout_manager = create_timeout_manager(&settings.core, &build_settings);

    let exchanges = create_exchanges(
        &settings.core,
        build_settings,
        events_sender.clone(),
        lifetime_manager.clone(),
        &timeout_manager,
    )
    .await;

    let exchanges_map: DashMap<_, _> = exchanges
        .into_iter()
        .map(|exchange| (exchange.exchange_account_id, exchange))
        .collect();

    let exchange_events = ExchangeEvents::new(events_sender.clone());

    let exchanges_hashmap: HashMap<ExchangeAccountId, Arc<Exchange>> =
        exchanges_map.clone().into_iter().collect();

    let currency_pair_to_symbol_converter = CurrencyPairToSymbolConverter::new(exchanges_hashmap);

    let balance_manager = BalanceManager::new(currency_pair_to_symbol_converter);

    BalanceManager::update_balances_for_exchanges(
        balance_manager.clone(),
        lifetime_manager.stop_token(),
    )
    .await;

    for exchange in &exchanges_map {
        exchange
            .value()
            .setup_balance_manager(balance_manager.clone())
    }

    let (finish_graceful_shutdown_tx, finish_graceful_shutdown_rx) = oneshot::channel();
    let engine_context = EngineContext::new(
        settings.core.clone(),
        exchanges_map.clone(),
        exchange_events,
        finish_graceful_shutdown_tx,
        timeout_manager,
        lifetime_manager.clone(),
        balance_manager,
    );

    Ok(Some((
        events_sender,
        events_receiver,
        settings,
        exchanges_map,
        engine_context,
        finish_graceful_shutdown_rx,
    )))
}

fn run_services<'a, StrategySettings>(
    engine_context: Arc<EngineContext>,
    events_sender: broadcast::Sender<ExchangeEvent>,
    events_receiver: broadcast::Receiver<ExchangeEvent>,
    settings: AppSettings<StrategySettings>,
    exchanges_map: DashMap<ExchangeAccountId, Arc<Exchange>>,
    init_user_settings: InitSettings<StrategySettings>,
    build_strategy: impl Fn(
        &AppSettings<StrategySettings>,
        Arc<EngineContext>,
    ) -> Box<dyn DispositionStrategy + 'static>,
    finish_graceful_shutdown_rx: oneshot::Receiver<ActionAfterGracefulShutdown>,
) -> TradingEngine
where
    StrategySettings: BaseStrategySettings + Clone + Debug + Deserialize<'a> + Serialize,
{
    let internal_events_loop = InternalEventsLoop::new();
    engine_context
        .shutdown_service
        .register_core_service(internal_events_loop.clone());

    let exchange_events = ExchangeEvents::new(events_sender.clone());
    let statistic_service = StatisticService::new();
    let statistic_event_handler =
        create_statistic_event_handler(exchange_events, statistic_service.clone());
    let control_panel = CoreApi::create_and_start(
        engine_context.lifetime_manager.clone(),
        load_pretty_settings(init_user_settings),
        statistic_service,
    )
    .expect("Unable to start control panel");
    engine_context
        .shutdown_service
        .register_core_service(control_panel.clone());

    {
        let local_exchanges_map = exchanges_map.into_iter().map(identity).collect();
        let action = internal_events_loop.clone().start(
            events_receiver,
            local_exchanges_map,
            engine_context.lifetime_manager.stop_token(),
        );
        let _ = spawn_future(
            "internal_events_loop start",
            SpawnFutureFlags::STOP_BY_TOKEN | SpawnFutureFlags::CRITICAL,
            action.boxed(),
        );
    }

    let disposition_strategy = build_strategy(&settings, engine_context.clone());
    let disposition_executor_service = create_disposition_executor_service(
        &settings.strategy,
        &engine_context,
        disposition_strategy,
        &statistic_event_handler.stats,
    );
    engine_context
        .shutdown_service
        .register_user_service(disposition_executor_service);

    log::info!("TradingEngine started");
    TradingEngine::new(engine_context.clone(), finish_graceful_shutdown_rx)
}

pub(crate) fn unwrap_or_handle_panic<T>(
    action_outcome: Result<T, Box<dyn Any + Send>>,
    message_template: &str,
    lifetime_manager: Option<Arc<AppLifetimeManager>>,
) -> Result<T> {
    action_outcome.map_err(|_| {
        if let Some(lifetime_manager) = lifetime_manager {
            lifetime_manager
                .spawn_graceful_shutdown("Panic during TradingEngine creation".to_owned());
        }

        anyhow!(message_template.to_owned())
    })
}

pub async fn launch_trading_engine<StrategySettings>(
    build_settings: &EngineBuildConfig,
    init_user_settings: InitSettings<StrategySettings>,
    build_strategy: impl Fn(
        &AppSettings<StrategySettings>,
        Arc<EngineContext>,
    ) -> Box<dyn DispositionStrategy + 'static>,
) -> Result<Option<TradingEngine>>
where
    StrategySettings: BaseStrategySettings + Clone + Debug + DeserializeOwned + Serialize,
{
    print_info("The TradingEngine is going to start...");
    let action_outcome = AssertUnwindSafe(before_engine_context_init(
        build_settings,
        init_user_settings.clone(),
    ))
    .catch_unwind()
    .await;

    let message_template = "Panic happened during EngineContext initialization";
    let (
        events_sender,
        events_receiver,
        settings,
        exchanges_map,
        engine_context,
        finish_graceful_shutdown_rx,
    ) = match unwrap_or_handle_panic(action_outcome, message_template, None)?? {
        Some(result_tuple) => result_tuple,
        None => return Ok(None),
    };

    let cloned_lifetime_manager = engine_context.lifetime_manager.clone();
    let action = async move {
        signal::ctrl_c().await.expect("failed to listen for event");

        print_info("Ctrl-C signal was received so graceful_shutdown will be started");
        cloned_lifetime_manager.spawn_graceful_shutdown("Ctrl-C signal was received".to_owned());

        Ok(())
    };

    let _ = spawn_future(
        "Start Ctrl-C handler",
        SpawnFutureFlags::STOP_BY_TOKEN | SpawnFutureFlags::CRITICAL,
        action.boxed(),
    );

    let action_outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        run_services(
            engine_context.clone(),
            events_sender,
            events_receiver,
            settings,
            exchanges_map,
            init_user_settings,
            build_strategy,
            finish_graceful_shutdown_rx,
        )
    }));

    let message_template = "Panic happened during TradingEngine creation";
    let result = unwrap_or_handle_panic(
        action_outcome,
        message_template,
        Some(engine_context.lifetime_manager.clone()),
    )
    .map(|trading_engine| Some(trading_engine));

    print_info("The TradingEngine has been successfully launched");

    result
}

fn create_disposition_executor_service(
    base_settings: &dyn BaseStrategySettings,
    engine_context: &Arc<EngineContext>,
    disposition_strategy: Box<dyn DispositionStrategy>,
    statistics: &Arc<StatisticService>,
) -> Arc<DispositionExecutorService> {
    DispositionExecutorService::new(
        engine_context.clone(),
        engine_context.get_events_channel(),
        LocalSnapshotsService::default(),
        base_settings.exchange_account_id(),
        base_settings.currency_pair(),
        disposition_strategy,
        engine_context.lifetime_manager.stop_token(),
        statistics.clone(),
    )
}

fn create_statistic_event_handler(
    events: ExchangeEvents,
    statistic_service: Arc<StatisticService>,
) -> Arc<StatisticEventHandler> {
    StatisticEventHandler::new(events.get_events_channel(), statistic_service)
}

pub async fn create_exchanges(
    core_settings: &CoreSettings,
    build_settings: &EngineBuildConfig,
    events_channel: broadcast::Sender<ExchangeEvent>,
    lifetime_manager: Arc<AppLifetimeManager>,
    timeout_manager: &Arc<TimeoutManager>,
) -> Vec<Arc<Exchange>> {
    join_all(core_settings.exchanges.iter().map(|x| {
        create_exchange(
            x,
            build_settings,
            events_channel.clone(),
            lifetime_manager.clone(),
            timeout_manager.clone(),
        )
    }))
    .await
}
